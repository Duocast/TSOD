use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use serde::Deserialize;
use tokio::sync::{watch, Notify};
use tracing::warn;

use crate::{net::dispatcher::ControlDispatcher, proto::voiceplatform::v1 as pb, ui};

const SCAN_INTERVAL: Duration = Duration::from_secs(30);
const GAME_DB_JSON: &str = include_str!("../assets/game_db.json");

#[derive(Clone)]
pub struct ActivityRuntimeSettings {
    share_game_activity: Arc<AtomicBool>,
    share_game_details: Arc<AtomicBool>,
    trigger: Arc<Notify>,
}

impl ActivityRuntimeSettings {
    pub fn from_app_settings(settings: &ui::model::AppSettings) -> Self {
        Self {
            share_game_activity: Arc::new(AtomicBool::new(settings.share_game_activity)),
            share_game_details: Arc::new(AtomicBool::new(settings.share_game_details)),
            trigger: Arc::new(Notify::new()),
        }
    }

    pub fn apply(&self, settings: &ui::model::AppSettings) {
        let mut changed = false;
        changed |= self
            .share_game_activity
            .swap(settings.share_game_activity, Ordering::Relaxed)
            != settings.share_game_activity;
        changed |= self
            .share_game_details
            .swap(settings.share_game_details, Ordering::Relaxed)
            != settings.share_game_details;
        if changed {
            self.trigger.notify_one();
        }
    }

    pub fn share_game_activity(&self) -> bool {
        self.share_game_activity.load(Ordering::Relaxed)
    }

    pub fn share_game_details(&self) -> bool {
        self.share_game_details.load(Ordering::Relaxed)
    }

    pub fn trigger_scan(&self) {
        self.trigger.notify_one();
    }

    fn notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.trigger.notified()
    }
}

#[derive(Debug, Deserialize)]
struct GameDbEntry {
    name: String,
    executables: Vec<String>,
}

#[derive(Clone)]
struct GameDb {
    by_executable: Arc<HashMap<String, String>>,
}

impl GameDb {
    fn load() -> Self {
        let entries: Vec<GameDbEntry> = serde_json::from_str(GAME_DB_JSON).unwrap_or_default();
        let mut by_executable = HashMap::new();
        for entry in entries {
            for executable in entry.executables {
                by_executable.insert(normalize_executable_name(&executable), entry.name.clone());
            }
        }
        Self {
            by_executable: Arc::new(by_executable),
        }
    }

    fn detect_game(&self, process_names: &HashSet<String>) -> Option<String> {
        process_names
            .iter()
            .find_map(|exe| self.by_executable.get(exe).cloned())
    }
}

pub fn spawn_activity_detector(
    dispatcher: ControlDispatcher,
    settings: ActivityRuntimeSettings,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let db = GameDb::load();
        let mut last_detected_game: Option<String> = None;
        let mut published_game: Option<String> = None;
        let mut started_at_ms: i64 = unix_ms_now();
        let mut interval = tokio::time::interval(SCAN_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            if *shutdown_rx.borrow() {
                break;
            }

            let mut detected = None;
            if settings.share_game_activity() {
                match scan_processes_and_detect(&db).await {
                    Ok(game) => detected = game,
                    Err(e) => warn!("activity scan failed: {e:#}"),
                }
            }

            if detected != last_detected_game {
                started_at_ms = unix_ms_now();
            }
            last_detected_game = detected.clone();

            let desired_publish = if !settings.share_game_activity() {
                None
            } else if let Some(game_name) = detected {
                if settings.share_game_details() {
                    Some(game_name)
                } else {
                    Some("a game".to_string())
                }
            } else {
                None
            };

            if desired_publish != published_game {
                let outbound = desired_publish.as_ref().map(|name| pb::GameActivity {
                    game_name: name.clone(),
                    details: String::new(),
                    state: String::new(),
                    started_at: Some(pb::Timestamp {
                        unix_millis: started_at_ms,
                    }),
                    large_image_url: String::new(),
                });
                if let Err(e) = dispatcher.update_current_activity(outbound).await {
                    warn!("activity publish failed: {e:#}");
                } else {
                    published_game = desired_publish;
                }
            }

            tokio::select! {
                _ = interval.tick() => {}
                _ = settings.notified() => {}
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }

        if published_game.is_some() {
            let _ = dispatcher.update_current_activity(None).await;
        }
    });
}

async fn scan_processes_and_detect(db: &GameDb) -> Result<Option<String>> {
    let names = tokio::task::spawn_blocking(collect_running_process_names).await??;
    Ok(db.detect_game(&names))
}

fn normalize_executable_name(name: &str) -> String {
    Path::new(name)
        .file_name()
        .map(|v| v.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string())
        .to_ascii_lowercase()
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(target_os = "linux")]
fn collect_running_process_names() -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let pid = entry.file_name();
        let pid = pid.to_string_lossy();
        if !pid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let base = entry.path();
        let exe_path = base.join("exe");
        if let Ok(target) = std::fs::read_link(&exe_path) {
            if let Some(name) = target.file_name() {
                names.insert(name.to_string_lossy().to_ascii_lowercase());
                continue;
            }
        }
        if let Ok(comm) = std::fs::read_to_string(base.join("comm")) {
            let trimmed = comm.trim();
            if !trimmed.is_empty() {
                names.insert(trimmed.to_ascii_lowercase());
            }
        }
    }
    Ok(names)
}

#[cfg(target_os = "windows")]
fn collect_running_process_names() -> Result<HashSet<String>> {
    use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
    use windows::Win32::System::ProcessStatus::K32EnumProcesses;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let mut pids = vec![0u32; 4096];
    let mut bytes_returned = 0u32;
    unsafe {
        K32EnumProcesses(
            pids.as_mut_ptr(),
            (pids.len() * std::mem::size_of::<u32>()) as u32,
            &mut bytes_returned,
        )?;
    }
    let count = bytes_returned as usize / std::mem::size_of::<u32>();
    let mut names = HashSet::new();
    for pid in pids.into_iter().take(count) {
        if pid == 0 {
            continue;
        }
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
        let Ok(handle) = handle else { continue };
        if handle.is_invalid() {
            continue;
        }

        let mut buf = vec![0u16; MAX_PATH as usize * 4];
        let mut size = buf.len() as u32;
        let ok = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32.0,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut size,
            )
            .is_ok()
        };
        unsafe {
            let _ = CloseHandle(handle);
        }
        if !ok || size == 0 {
            continue;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        names.insert(normalize_executable_name(&path));
    }
    Ok(names)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn collect_running_process_names() -> Result<HashSet<String>> {
    Ok(HashSet::new())
}
