#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! vp-client main — egui/eframe GUI + QUIC voice
//!
//! Architecture:
//! - eframe runs the GUI event loop on the main thread
//! - A tokio runtime runs in a background thread for networking + audio
//! - crossbeam channels bridge the GUI ↔ backend boundary
//! - DSP pipeline (RNNoise, AGC, VAD) processes audio before encoding

mod app;
mod audio;
mod config;
mod identity;
mod net;
mod proto;
mod settings_io;
mod ui;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use config::Config;
use crossbeam_channel::{bounded, Receiver, Sender};
use identity::DeviceIdentity;
use net::dispatcher::{ControlDispatcher, PushEvent};
use net::voice_datagram::{
    make_voice_datagram, VOICE_FORWARDED_HDR_LEN, VOICE_HDR_LEN, VOICE_VERSION,
};
use proto::voiceplatform::v1 as pb;
use std::collections::HashMap;
#[cfg(debug_assertions)]
use std::collections::HashSet;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};
#[cfg(debug_assertions)]
use std::sync::{Mutex as StdMutex, OnceLock};
use tokio::sync::{watch, Mutex, RwLock};
use tokio::time::{sleep, Duration, Instant};
use tracing::{debug, warn, Level};
use tracing_subscriber::EnvFilter;
use ui::model::{FecMode, PerUserAudioSettings};
use ui::{UiEvent, UiIntent, VpApp};

#[cfg(debug_assertions)]
static DEBUG_SEEN_AUTH_USER_IDS: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();

#[derive(Clone)]
struct AudioRuntimeSettings {
    output_auto_level: Arc<AtomicBool>,
    comfort_noise: Arc<AtomicBool>,
    comfort_noise_level: Arc<AtomicU32>,
    ducking_enabled: Arc<AtomicBool>,
    ducking_attenuation_db: Arc<AtomicU32>,
    typing_attenuation: Arc<AtomicBool>,
    denoise_attenuation_db: Arc<AtomicU32>,
    fec_mode: Arc<AtomicU32>,
    fec_strength: Arc<AtomicU32>,
}

impl AudioRuntimeSettings {
    fn from_app_settings(settings: &ui::model::AppSettings) -> Self {
        Self {
            output_auto_level: Arc::new(AtomicBool::new(settings.output_auto_level)),
            comfort_noise: Arc::new(AtomicBool::new(settings.comfort_noise)),
            comfort_noise_level: Arc::new(AtomicU32::new(f32_to_u32(settings.comfort_noise_level))),
            ducking_enabled: Arc::new(AtomicBool::new(settings.ducking_enabled)),
            ducking_attenuation_db: Arc::new(AtomicU32::new(f32_to_u32(
                settings.ducking_attenuation_db as f32,
            ))),
            typing_attenuation: Arc::new(AtomicBool::new(settings.typing_attenuation)),
            denoise_attenuation_db: Arc::new(AtomicU32::new(f32_to_u32(
                settings.denoise_attenuation_db as f32,
            ))),
            fec_mode: Arc::new(AtomicU32::new(settings.fec_mode as u32)),
            fec_strength: Arc::new(AtomicU32::new(settings.fec_strength as u32)),
        }
    }

    fn apply(&self, settings: &ui::model::AppSettings) {
        self.output_auto_level
            .store(settings.output_auto_level, Ordering::Relaxed);
        self.comfort_noise
            .store(settings.comfort_noise, Ordering::Relaxed);
        self.comfort_noise_level
            .store(f32_to_u32(settings.comfort_noise_level), Ordering::Relaxed);
        self.ducking_enabled
            .store(settings.ducking_enabled, Ordering::Relaxed);
        self.ducking_attenuation_db.store(
            f32_to_u32(settings.ducking_attenuation_db as f32),
            Ordering::Relaxed,
        );
        self.typing_attenuation
            .store(settings.typing_attenuation, Ordering::Relaxed);
        self.denoise_attenuation_db.store(
            f32_to_u32(settings.denoise_attenuation_db as f32),
            Ordering::Relaxed,
        );
        self.fec_mode
            .store(settings.fec_mode as u32, Ordering::Relaxed);
        self.fec_strength
            .store(settings.fec_strength as u32, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct VoiceTelemetryCounters {
    tx_packets: AtomicU64,
    tx_bytes: AtomicU64,
    rx_packets: AtomicU64,
    rx_bytes: AtomicU64,
    late_packets: AtomicU64,
    lost_packets: AtomicU64,
    concealment_frames: AtomicU64,
    jitter_buffer_depth: AtomicU64,
    peak_stream_level_bits: AtomicU32,
}

impl VoiceTelemetryCounters {
    fn observe_peak_stream_level(&self, level: f32) {
        let mut current = self.peak_stream_level_bits.load(Ordering::Relaxed);
        loop {
            let cur = f32::from_bits(current);
            if level <= cur {
                break;
            }
            match self.peak_stream_level_bits.compare_exchange_weak(
                current,
                level.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }
}

fn apply_fec_encoder_settings(
    encoder: &mut audio::opus::OpusEncoder,
    audio_runtime: &AudioRuntimeSettings,
) -> Result<()> {
    let fec_mode = match audio_runtime.fec_mode.load(Ordering::Relaxed) {
        0 => FecMode::Off,
        2 => FecMode::On,
        _ => FecMode::Auto,
    };
    let fec_strength = audio_runtime.fec_strength.load(Ordering::Relaxed).min(100) as i32;
    let enable_fec = fec_mode != FecMode::Off;
    let packet_loss = match fec_mode {
        FecMode::Off => 0,
        FecMode::Auto => fec_strength.clamp(10, 40),
        FecMode::On => fec_strength,
    };
    encoder.set_inband_fec(enable_fec)?;
    encoder.set_packet_loss_perc(packet_loss)?;
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let cfg = Config::load();

    // Channels between GUI and backend (crossbeam for sync/async bridging)
    let (tx_intent, rx_intent) = bounded::<UiIntent>(256);
    let (tx_event, rx_event) = bounded::<UiEvent>(1024);

    // Shared shutdown signal
    let running = Arc::new(AtomicBool::new(true));
    let (shutdown_tx, shutdown_rx) = watch::channel::<bool>(false);

    // PTT state
    let ptt_active = Arc::new(AtomicBool::new(!cfg.push_to_talk));

    // Start the tokio backend in a background thread
    let backend_cfg = cfg.clone();
    let backend_running = running.clone();
    let backend_tx_event = tx_event.clone();
    let backend_ptt = ptt_active.clone();

    let backend_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        rt.block_on(async {
            if let Err(e) = app_task(
                backend_cfg,
                backend_tx_event,
                rx_intent,
                backend_running,
                shutdown_rx,
                backend_ptt,
            )
            .await
            {
                warn!("backend error: {e:#}");
            }
        });
    });

    // Run the eframe GUI on the main thread
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TSOD")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };

    let gui_result = eframe::run_native(
        "TSOD",
        native_options,
        Box::new(move |cc| Ok(Box::new(VpApp::new(cc, tx_intent, rx_event)))),
    );

    // GUI exited — signal backend to shut down
    running.store(false, Ordering::Relaxed);
    let _ = shutdown_tx.send(true);

    // Do not block UI shutdown waiting for backend/network teardown.
    // Once the main thread returns, the process exits immediately.
    let _ = backend_thread;

    gui_result.map_err(|e| anyhow!("eframe error: {e}"))
}

// ── Backend task ───────────────────────────────────────────────────────

fn set_connection_stage(
    tx_event: &Sender<UiEvent>,
    stage: ui::model::ConnectionStage,
    detail: impl Into<String>,
) {
    let detail = detail.into();
    let _ = tx_event.send(UiEvent::SetConnectionStage {
        stage,
        detail: detail.clone(),
    });
    let _ = tx_event.send(UiEvent::AppendLog(format!("[conn] {detail}")));
}

async fn app_task(
    mut cfg: Config,
    tx_event: Sender<UiEvent>,
    rx_intent: Receiver<UiIntent>,
    running: Arc<AtomicBool>,
    mut shutdown_rx: watch::Receiver<bool>,
    ptt_active: Arc<AtomicBool>,
) -> Result<()> {
    let _ = tx_event.send(UiEvent::AppendLog(format!(
        "[sys] starting, server={}, sni={}, ca_cert={}",
        cfg.server,
        cfg.server_name,
        if cfg.ca_cert_pem.is_empty() {
            "(insecure dev mode)"
        } else {
            &cfg.ca_cert_pem
        }
    )));
    let _ = tx_event.send(UiEvent::SetNick(cfg.display_name.clone()));
    let (initial_host, initial_port) = split_server_host_port(&cfg.server);
    let _ = tx_event.send(UiEvent::SetServerAddress {
        host: initial_host,
        port: initial_port,
    });

    if cfg.server == "127.0.0.1:4433" || cfg.server == "localhost:4433" {
        let _ = tx_event.send(UiEvent::AppendLog(
            "[net] warning: using default server 127.0.0.1:4433; set --server or VP_SERVER for remote gateway".into(),
        ));
    }

    // Enumerate and report audio devices to the UI
    let input_devices = audio::capture::enumerate_input_devices();
    let output_devices = audio::playout::enumerate_output_devices();
    let playback_modes = audio::playout::enumerate_playback_modes();
    let _ = tx_event.send(UiEvent::SetAudioDevices {
        input_devices,
        output_devices,
        playback_modes,
    });

    // Load persisted settings and send to UI
    let saved_settings = settings_io::load_settings();
    if !saved_settings.identity_nickname.trim().is_empty() {
        cfg.display_name = saved_settings.identity_nickname.trim().to_string();
        let _ = tx_event.send(UiEvent::SetNick(cfg.display_name.clone()));
    }
    if !saved_settings.last_server_host.trim().is_empty() {
        cfg.server = format!(
            "{}:{}",
            saved_settings.last_server_host.trim(),
            saved_settings.last_server_port
        );
        cfg.server_name = saved_settings.last_server_host.trim().to_string();
        let _ = tx_event.send(UiEvent::SetServerAddress {
            host: saved_settings.last_server_host.trim().to_string(),
            port: saved_settings.last_server_port,
        });
    }
    let _ = tx_event.send(UiEvent::SettingsLoaded(Box::new(saved_settings.clone())));

    let audio_runtime = AudioRuntimeSettings::from_app_settings(&saved_settings);

    // Audio constants
    let sample_rate = 48_000u32;
    let channels = 1u16;
    let frame_ms = 20u32;

    let selected_audio = Arc::new(Mutex::new(AudioSelection {
        input_device: normalize_device_name(&saved_settings.capture_device),
        output_device: normalize_device_name(&saved_settings.playback_device),
        playback_mode: normalize_playback_mode(&saved_settings.playback_mode),
    }));

    // Audio pipeline
    let encoder = Arc::new(Mutex::new(audio::opus::OpusEncoder::new(
        sample_rate,
        channels as u8,
    )?));
    {
        let mut enc = encoder.lock().await;
        let _ = apply_fec_encoder_settings(&mut enc, &audio_runtime);
    }

    let initial_selection = selected_audio.lock().await.clone();
    let capture = Arc::new(RwLock::new(Arc::new(start_capture_with_fallback(
        sample_rate,
        channels,
        frame_ms,
        initial_selection.input_device.as_deref(),
    )?)));
    let playout = Arc::new(RwLock::new(Arc::new(start_playout_with_fallback(
        sample_rate,
        channels,
        initial_selection.output_device.as_deref(),
        initial_selection.playback_mode.as_deref(),
    )?)));

    // DSP pipeline
    let dsp_enabled = !cfg.no_noise_suppression;
    let capture_dsp = if dsp_enabled {
        Some(Arc::new(Mutex::new(audio::dsp::CaptureDsp::new(
            sample_rate,
        )?)))
    } else {
        None
    };

    if let Some(ref dsp) = capture_dsp {
        let mut d = dsp.lock().await;
        d.set_vad_threshold(cfg.vad_threshold);
        d.set_noise_suppression(saved_settings.noise_suppression);
        d.set_agc(saved_settings.agc_enabled);
        d.set_agc_target(saved_settings.agc_target_db);
        d.set_echo_cancellation(saved_settings.echo_cancellation);
    }

    // Shared self-mute/deafen state for the audio pipeline
    let self_muted = Arc::new(AtomicBool::new(false));
    let self_deafened = Arc::new(AtomicBool::new(false));
    let server_deafened = Arc::new(AtomicBool::new(false));

    // Shared gain values (stored as u32 bits of f32, default 1.0)
    let input_gain = Arc::new(std::sync::atomic::AtomicU32::new(f32_to_u32(1.0)));
    let output_gain = Arc::new(std::sync::atomic::AtomicU32::new(f32_to_u32(1.0)));
    let per_user_audio = Arc::new(std::sync::RwLock::new(
        saved_settings.per_user_audio.clone(),
    ));
    let loopback_active = Arc::new(AtomicBool::new(false));
    let session_voice_active = Arc::new(AtomicBool::new(false));
    let active_voice_channel_route = Arc::new(AtomicU32::new(0));
    let voice_counters = Arc::new(VoiceTelemetryCounters::default());
    let send_queue_drop_count = Arc::new(AtomicU32::new(0));

    let _telemetry = tokio::spawn(emit_telemetry_loop(
        tx_event.clone(),
        capture_dsp.clone(),
        voice_counters.clone(),
        send_queue_drop_count.clone(),
        running.clone(),
        shutdown_rx.clone(),
    ));

    let _mic_test = tokio::spawn(mic_test_loop(
        capture.clone(),
        playout.clone(),
        tx_event.clone(),
        input_gain.clone(),
        loopback_active.clone(),
        session_voice_active.clone(),
        running.clone(),
        shutdown_rx.clone(),
    ));

    let mut backoff = Backoff::new(Duration::from_millis(250), Duration::from_secs(10));

    while running.load(Ordering::Relaxed) && !*shutdown_rx.borrow() {
        match connect_and_run_session(
            &mut cfg,
            &tx_event,
            &rx_intent,
            encoder.clone(),
            capture.clone(),
            playout.clone(),
            capture_dsp.clone(),
            active_voice_channel_route.clone(),
            selected_audio.clone(),
            ptt_active.clone(),
            self_muted.clone(),
            self_deafened.clone(),
            server_deafened.clone(),
            input_gain.clone(),
            output_gain.clone(),
            per_user_audio.clone(),
            loopback_active.clone(),
            session_voice_active.clone(),
            voice_counters.clone(),
            audio_runtime.clone(),
            sample_rate,
            channels,
            frame_ms,
            &mut shutdown_rx,
        )
        .await
        {
            Ok(()) => {
                backoff.reset();
            }
            Err(e) => {
                set_connection_stage(
                    &tx_event,
                    ui::model::ConnectionStage::Failed,
                    format!("Connection failed: {e:#}"),
                );
                let _ = tx_event.send(UiEvent::AppendLog(format!("[net] disconnected: {e:#}")));

                let jitter = rand::random::<u64>() % 150;
                let wait_for = backoff.cur + Duration::from_millis(jitter);
                backoff.cur = (backoff.cur * 2).min(backoff.max);

                let deadline = tokio::time::Instant::now() + wait_for;
                'retry_wait: while tokio::time::Instant::now() < deadline {
                    while let Ok(intent) = rx_intent.try_recv() {
                        match intent {
                            UiIntent::Quit => return Ok(()),
                            UiIntent::ToggleLoopback => {
                                let new = !loopback_active.load(Ordering::Relaxed);
                                loopback_active.store(new, Ordering::Relaxed);
                                let _ = tx_event.send(UiEvent::SetLoopbackActive(new));
                                let _ = tx_event
                                    .send(UiEvent::AppendLog(format!("[audio] loopback: {new}")));
                            }
                            UiIntent::SetInputGain(gain) => {
                                input_gain.store(f32_to_u32(gain), Ordering::Relaxed);
                            }
                            UiIntent::SetEchoCancellation(enabled) => {
                                if let Some(ref dsp) = capture_dsp {
                                    let mut d = dsp.lock().await;
                                    d.set_echo_cancellation(enabled);
                                }
                            }
                            UiIntent::SetInputDevice(dev) => {
                                let selected = normalize_device_name(&dev);
                                {
                                    let mut state = selected_audio.lock().await;
                                    state.input_device = selected;
                                }
                                if let Err(e) = restart_audio_streams(
                                    &capture,
                                    &playout,
                                    &selected_audio,
                                    &tx_event,
                                    sample_rate,
                                    channels,
                                    frame_ms,
                                )
                                .await
                                {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[audio] failed to switch input device: {e:#}"
                                    )));
                                }
                            }
                            UiIntent::SetOutputDevice(dev) => {
                                let selected = normalize_device_name(&dev);
                                {
                                    let mut state = selected_audio.lock().await;
                                    state.output_device = selected;
                                }
                                if let Err(e) = restart_audio_streams(
                                    &capture,
                                    &playout,
                                    &selected_audio,
                                    &tx_event,
                                    sample_rate,
                                    channels,
                                    frame_ms,
                                )
                                .await
                                {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[audio] failed to switch output device: {e:#}"
                                    )));
                                }
                            }
                            UiIntent::SaveSettings(ref settings) => {
                                let _ = settings_io::save_settings(settings);
                            }
                            UiIntent::ConnectToServer {
                                host,
                                port,
                                nickname,
                            } => {
                                cfg.server = format!("{host}:{port}");
                                cfg.server_name = host.clone();
                                cfg.display_name = nickname.clone();
                                let _ = tx_event.send(UiEvent::SetNick(nickname.clone()));
                                let _ = tx_event.send(UiEvent::SetServerAddress { host, port });
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[net] target server updated: {}",
                                    cfg.server
                                )));
                                break 'retry_wait;
                            }
                            UiIntent::CancelConnect => {
                                set_connection_stage(
                                    &tx_event,
                                    ui::model::ConnectionStage::Idle,
                                    "Connection attempt cancelled",
                                );
                            }
                            UiIntent::SetPlaybackMode(mode) => {
                                {
                                    let mut state = selected_audio.lock().await;
                                    state.playback_mode = normalize_playback_mode(&mode);
                                }

                                if let Err(e) = restart_audio_streams(
                                    &capture,
                                    &playout,
                                    &selected_audio,
                                    &tx_event,
                                    sample_rate,
                                    channels,
                                    frame_ms,
                                )
                                .await
                                {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[audio] failed to switch playback mode: {e:#}"
                                    )));
                                }
                            }
                            UiIntent::SetAwayMessage { message } => {
                                let _ = tx_event.send(UiEvent::SetAwayMessage(message.clone()));
                                let text = if message.trim().is_empty() {
                                    "[presence] away message cleared".to_string()
                                } else {
                                    format!("[presence] away message set: {message}")
                                };
                                let _ = tx_event.send(UiEvent::AppendLog(text));
                            }
                            _ => {}
                        }
                    }

                    if *shutdown_rx.borrow() {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
    }

    let _ = tx_event.send(UiEvent::AppendLog("[sys] shutting down".into()));
    Ok(())
}

fn maybe_note_event_gap(_tx_event: &Sender<UiEvent>, _event_seq: u64) {
    // event_seq == 0 means the server did not stamp this push with a sequence
    // number; it is treated as unordered and always applied. No user-visible
    // log entry is emitted here — gap detection for stamped events is handled
    // inside should_apply_event_seq.
}

fn should_apply_event_seq(
    tx_event: &Sender<UiEvent>,
    last_event_seq: &mut u64,
    event_seq: u64,
) -> bool {
    if event_seq == 0 {
        // Server did not stamp this event; apply unconditionally.
        return true;
    }
    if event_seq <= *last_event_seq {
        let _ = tx_event.send(UiEvent::AppendLog(format!(
            "[sync] ignoring stale push event_seq={} <= last_event_seq={}",
            event_seq, *last_event_seq
        )));
        return false;
    }
    if *last_event_seq != 0 && event_seq > *last_event_seq + 1 {
        let _ = tx_event.send(UiEvent::AppendLog(format!(
            "[sync] event sequence gap detected: expected {} got {} (missed {} events)",
            *last_event_seq + 1,
            event_seq,
            event_seq - *last_event_seq - 1,
        )));
    }
    *last_event_seq = event_seq;
    let _ = tx_event.send(UiEvent::SetLastEventSeq(event_seq));
    true
}

fn apply_authoritative_snapshot(
    snapshot: &pb::InitialStateSnapshot,
    tx_event: &Sender<UiEvent>,
    requested_channel_id: Option<&str>,
) {
    let channels = snapshot
        .channels
        .iter()
        .filter_map(|ch| ch.info.as_ref())
        .map(|info| ui::model::ChannelEntry {
            id: info
                .channel_id
                .as_ref()
                .map(|id| id.value.clone())
                .unwrap_or_default(),
            name: info.name.clone(),
            channel_type: ui::model::ChannelType::Voice,
            parent_id: info.parent_channel_id.as_ref().map(|pid| pid.value.clone()),
            position: info.position,
            member_count: 0,
            user_limit: info.user_limit,
        })
        .collect::<Vec<_>>();

    let _ = tx_event.send(UiEvent::SetChannels(channels.clone()));
    let _ = tx_event.send(UiEvent::SetDefaultChannelId(
        snapshot
            .default_channel_id
            .as_ref()
            .map(|channel_id| channel_id.value.clone()),
    ));
    let _ = tx_event.send(UiEvent::SetLastEventSeq(snapshot.snapshot_version));

    for scope in &snapshot.channel_members {
        let channel_id = scope
            .channel_id
            .as_ref()
            .map(|id| id.value.clone())
            .unwrap_or_default();
        let members = scope
            .members
            .iter()
            .map(|m| ui::model::MemberEntry {
                user_id: m
                    .user_id
                    .as_ref()
                    .map(|u| u.value.clone())
                    .unwrap_or_default(),
                display_name: m.display_name.clone(),
                away_message: String::new(),
                muted: m.muted,
                deafened: m.deafened,
                self_muted: m.self_muted,
                self_deafened: m.self_deafened,
                streaming: m.streaming,
                speaking: false,
                avatar_url: None,
            })
            .collect::<Vec<_>>();
        let _ = tx_event.send(UiEvent::UpdateChannelMembers {
            channel_id,
            members,
        });
    }

    let selected = choose_initial_selected_channel(snapshot, requested_channel_id);
    if let Some(selected_channel) = selected {
        let _ = tx_event.send(UiEvent::SetChannelName(selected_channel));
    }

    let _ = tx_event.send(UiEvent::AppendLog(format!(
        "[sync] authoritative snapshot applied server_id={} auth_user_id={} channels={} member_scopes={} members_semantics=selected-channel scoped",
        snapshot.server_id.as_ref().map(|sid| sid.value.clone()).unwrap_or_default(),
        snapshot.self_user_id.as_ref().map(|u| u.value.clone()).unwrap_or_default(),
        snapshot.channels.len(),
        snapshot.channel_members.len(),
    )));
}

fn choose_initial_selected_channel(
    snapshot: &pb::InitialStateSnapshot,
    requested_channel_id: Option<&str>,
) -> Option<String> {
    if let Some(requested) = requested_channel_id {
        if snapshot.channels.iter().any(|channel| {
            channel
                .info
                .as_ref()
                .and_then(|info| info.channel_id.as_ref())
                .is_some_and(|cid| cid.value == requested)
        }) {
            return Some(requested.to_string());
        }
    }

    snapshot
        .default_channel_id
        .as_ref()
        .map(|id| id.value.clone())
        .or_else(|| {
            snapshot
                .channels
                .first()
                .and_then(|channel| channel.info.as_ref())
                .and_then(|info| info.channel_id.as_ref())
                .map(|id| id.value.clone())
        })
}

#[derive(Clone, Debug, Default)]
struct AudioSelection {
    input_device: Option<String>,
    output_device: Option<String>,
    playback_mode: Option<String>,
}

fn normalize_device_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "(system default)" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_playback_mode(mode: &str) -> Option<String> {
    let trimmed = mode.trim();
    if trimmed.is_empty() || trimmed == audio::playout::PLAYBACK_MODE_AUTO {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn start_capture_with_fallback(
    sample_rate: u32,
    channels: u16,
    frame_ms: u32,
    preferred_device: Option<&str>,
) -> Result<audio::capture::Capture> {
    if let Some(device) = preferred_device {
        match audio::capture::Capture::start_with_device(
            sample_rate,
            channels,
            frame_ms,
            Some(device),
        ) {
            Ok(capture) => return Ok(capture),
            Err(_) => {}
        }
    }
    audio::capture::Capture::start_with_device(sample_rate, channels, frame_ms, None)
}

fn start_playout_with_fallback(
    sample_rate: u32,
    channels: u16,
    preferred_device: Option<&str>,
    preferred_mode: Option<&str>,
) -> Result<audio::playout::Playout> {
    if let Some(device) = preferred_device {
        match audio::playout::Playout::start_with_mode(
            sample_rate,
            channels,
            Some(device),
            preferred_mode,
        ) {
            Ok(playout) => return Ok(playout),
            Err(_) => {}
        }
    }
    audio::playout::Playout::start_with_mode(sample_rate, channels, None, preferred_mode)
}

async fn restart_audio_streams(
    capture: &Arc<RwLock<Arc<audio::capture::Capture>>>,
    playout: &Arc<RwLock<Arc<audio::playout::Playout>>>,
    selection: &Arc<Mutex<AudioSelection>>,
    tx_event: &Sender<UiEvent>,
    sample_rate: u32,
    channels: u16,
    frame_ms: u32,
) -> Result<()> {
    let selected = selection.lock().await.clone();
    let preferred_input = selected.input_device.as_deref();
    let preferred_output = selected.output_device.as_deref();
    let preferred_mode = selected.playback_mode.as_deref();

    let new_capture = start_capture_with_fallback(sample_rate, channels, frame_ms, preferred_input)
        .context("restart capture")?;
    let new_playout =
        start_playout_with_fallback(sample_rate, channels, preferred_output, preferred_mode)
            .context("restart playout")?;

    {
        let mut cap = capture.write().await;
        *cap = Arc::new(new_capture);
    }
    {
        let mut out = playout.write().await;
        *out = Arc::new(new_playout);
    }

    let _ = tx_event.send(UiEvent::AppendLog(format!(
        "[audio] streams restarted (input={}, output={}, mode={})",
        selected
            .input_device
            .as_deref()
            .unwrap_or("(system default)"),
        selected
            .output_device
            .as_deref()
            .unwrap_or("(system default)"),
        selected
            .playback_mode
            .as_deref()
            .unwrap_or(audio::playout::PLAYBACK_MODE_AUTO)
    )));

    Ok(())
}

fn split_server_host_port(server: &str) -> (String, u16) {
    if let Some((host, port_text)) = server.rsplit_once(':') {
        if let Ok(port) = port_text.parse::<u16>() {
            return (host.to_string(), port);
        }
    }
    (server.to_string(), 4433)
}

async fn connect_and_run_session(
    cfg: &mut Config,
    tx_event: &Sender<UiEvent>,
    rx_intent: &Receiver<UiIntent>,
    encoder: Arc<Mutex<audio::opus::OpusEncoder>>,
    capture: Arc<RwLock<Arc<audio::capture::Capture>>>,
    playout: Arc<RwLock<Arc<audio::playout::Playout>>>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    active_voice_channel_route: Arc<AtomicU32>,
    selected_audio: Arc<Mutex<AudioSelection>>,
    ptt_active: Arc<AtomicBool>,
    self_muted: Arc<AtomicBool>,
    self_deafened: Arc<AtomicBool>,
    server_deafened: Arc<AtomicBool>,
    input_gain: Arc<std::sync::atomic::AtomicU32>,
    output_gain: Arc<std::sync::atomic::AtomicU32>,
    per_user_audio: Arc<std::sync::RwLock<HashMap<String, PerUserAudioSettings>>>,
    loopback_active: Arc<AtomicBool>,
    session_voice_active: Arc<AtomicBool>,
    voice_counters: Arc<VoiceTelemetryCounters>,
    audio_runtime: AudioRuntimeSettings,
    sample_rate: u32,
    channels: u16,
    frame_ms: u32,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let _ = tx_event.send(UiEvent::SetConnected(false));
    let _ = tx_event.send(UiEvent::SetAuthed(false));
    server_deafened.store(false, Ordering::Relaxed);

    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Resolving,
        format!("Connect requested for {}", cfg.server),
    );
    let resolve_started = Instant::now();
    let endpoint = make_endpoint_with_optional_pinning(cfg)?;
    let addr = cfg.server.parse().context("parse server addr")?;
    let resolve_elapsed = resolve_started.elapsed();
    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Resolving,
        format!("Host/addr prepared in {} ms", resolve_elapsed.as_millis()),
    );

    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Handshaking,
        format!("Establishing QUIC/TLS to {}", cfg.server_name),
    );
    let handshake_started = Instant::now();
    let conn = endpoint
        .connect(addr, &cfg.server_name)
        .context("connect start")?
        .await
        .context("connect await")?;
    let handshake_elapsed = handshake_started.elapsed();

    let _ = tx_event.send(UiEvent::SetConnected(true));
    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Handshaking,
        format!(
            "QUIC/TLS established in {} ms",
            handshake_elapsed.as_millis()
        ),
    );

    let (send, recv) = conn.open_bi().await.context("open control stream")?;
    let dispatcher = ControlDispatcher::start(send, recv, shutdown_rx.clone());

    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Authenticating,
        "Authenticating with gateway",
    );
    let device_identity =
        DeviceIdentity::load_or_create().context("load/create device identity")?;
    let auth_started = Instant::now();
    let auth_info = dispatcher
        .hello_auth(&cfg.alpn, &device_identity, &cfg.display_name)
        .await
        .context("hello/auth")?;
    let auth_elapsed = auth_started.elapsed();
    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Authenticating,
        format!(
            "Authentication completed in {} ms",
            auth_elapsed.as_millis()
        ),
    );

    debug!(
        session_id = %auth_info.session_id,
        user_id = %auth_info.user_id,
        display_name = %cfg.display_name,
        "auth success"
    );
    if !auth_info.user_id.is_empty() {
        let _ = tx_event.send(UiEvent::SetUserId(auth_info.user_id.clone()));
    }

    #[cfg(debug_assertions)]
    if !auth_info.user_id.trim().is_empty() {
        let seen = DEBUG_SEEN_AUTH_USER_IDS.get_or_init(|| StdMutex::new(HashSet::new()));
        if let Ok(mut seen_ids) = seen.lock() {
            if !seen_ids.insert(auth_info.user_id.clone()) {
                warn!(
                    user_id = %auth_info.user_id,
                    session_id = %auth_info.session_id,
                    "debug warning: auth user_id already seen in this process; sessions may represent the same identity"
                );
                let _ = tx_event.send(UiEvent::AppendLog(
                    format!(
                        "[auth] warning: authenticated user_id {} already exists in a local session; nickname does not change auth identity",
                        auth_info.user_id
                    ),
                ));
            }
        }
    }

    let local_user_id = if auth_info.user_id.trim().is_empty() {
        cfg.display_name.clone()
    } else {
        auth_info.user_id.clone()
    };

    let _ = tx_event.send(UiEvent::SetAuthed(true));
    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Syncing,
        "Syncing initial state",
    );

    let initial_active_channel: Option<String> = cfg.channel_id.clone();

    let snapshot = dispatcher
        .get_initial_state_snapshot()
        .await
        .context("get_initial_state_snapshot")?;
    let initially_server_deafened = snapshot.channel_members.iter().any(|scope| {
        scope.members.iter().any(|member| {
            member.user_id.as_ref().map(|u| u.value.as_str()) == Some(local_user_id.as_str())
                && member.deafened
        })
    });
    server_deafened.store(initially_server_deafened, Ordering::Relaxed);
    apply_authoritative_snapshot(&snapshot, tx_event, initial_active_channel.as_deref());

    // Server push consumer
    let mut push_rx = dispatcher.take_push_receiver().await;
    {
        let tx_event = tx_event.clone();
        let mut last_event_seq = snapshot.snapshot_version;
        let local_user_id = local_user_id.clone();
        let active_voice_channel_route = active_voice_channel_route.clone();
        let server_deafened = server_deafened.clone();
        tokio::spawn(async move {
            while let Some(ev) = push_rx.recv().await {
                match ev {
                    PushEvent::Chat {
                        event: c,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        let event_at_millis = c.at.as_ref().map(|t| t.unix_millis);
                        if let Some(kind) = c.kind {
                            match kind {
                                pb::chat_event::Kind::MessagePosted(mp) => {
                                    let author_id = mp
                                        .author_user_id
                                        .as_ref()
                                        .map(|u| u.value.clone())
                                        .unwrap_or_default();
                                    let channel_id = mp
                                        .channel_id
                                        .as_ref()
                                        .map(|c| c.value.clone())
                                        .unwrap_or_default();
                                    let timestamp = event_at_millis.unwrap_or_else(|| {
                                        let missing = [
                                            ("message.author_user_id", author_id.is_empty()),
                                            ("chat_event.at", event_at_millis.is_none()),
                                        ]
                                        .into_iter()
                                        .filter_map(|(name, miss)| miss.then_some(name))
                                        .collect::<Vec<_>>();
                                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                                            "[chat] missing metadata for message_posted fields={}",
                                            missing.join(", ")
                                        )));
                                        unix_ms() as i64
                                    });

                                    let message_id = mp
                                        .message_id
                                        .as_ref()
                                        .map(|m| m.value.clone())
                                        .unwrap_or_default();
                                    debug!(
                                        message_id = %message_id,
                                        author_user_id = %author_id,
                                        channel_id = %channel_id,
                                        "received message_posted push event"
                                    );

                                    let _ = tx_event.send(UiEvent::MessageReceived(
                                        ui::model::ChatMessage {
                                            message_id,
                                            channel_id,
                                            author_name: author_id.clone(),
                                            author_id,
                                            text: mp.text.clone(),
                                            timestamp,
                                            attachments: mp
                                                .attachments
                                                .into_iter()
                                                .map(|a| ui::model::AttachmentData {
                                                    asset_id: a
                                                        .asset_id
                                                        .map(|x| x.value)
                                                        .unwrap_or_default(),
                                                    filename: a.filename,
                                                    mime_type: a.mime_type,
                                                    size_bytes: a.size_bytes,
                                                    download_url: String::new(),
                                                    thumbnail_url: None,
                                                })
                                                .collect(),
                                            reply_to: mp.reply_to_message_id.map(|r| r.value),
                                            reactions: Vec::new(),
                                            pinned: mp.pinned,
                                            edited: mp.edited_at.is_some(),
                                        },
                                    ));
                                }
                                pb::chat_event::Kind::MessageEdited(me) => {
                                    let _ = tx_event.send(UiEvent::MessageEdited {
                                        channel_id: me
                                            .channel_id
                                            .map(|c| c.value)
                                            .unwrap_or_default(),
                                        message_id: me
                                            .message_id
                                            .map(|m| m.value)
                                            .unwrap_or_default(),
                                        new_text: me.new_text,
                                    });
                                }
                                pb::chat_event::Kind::MessageDeleted(md) => {
                                    let _ = tx_event.send(UiEvent::MessageDeleted {
                                        channel_id: md
                                            .channel_id
                                            .map(|c| c.value)
                                            .unwrap_or_default(),
                                        message_id: md
                                            .message_id
                                            .map(|m| m.value)
                                            .unwrap_or_default(),
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                    PushEvent::Presence {
                        event: p,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if let Some(kind) = p.kind {
                            match kind {
                                pb::presence_event::Kind::MemberJoined(mj) => {
                                    if let (Some(channel_id), Some(member)) =
                                        (mj.channel_id, mj.member)
                                    {
                                        let user_id = member
                                            .user_id
                                            .as_ref()
                                            .map(|u| u.value.clone())
                                            .unwrap_or_default();
                                        debug!(
                                            channel_id = %channel_id.value,
                                            user_id = %user_id,
                                            display_name = %member.display_name,
                                            "received member-joined push event"
                                        );
                                        if user_id == local_user_id {
                                            server_deafened
                                                .store(member.deafened, Ordering::Relaxed);
                                            let route = uuid::Uuid::parse_str(&channel_id.value)
                                                .map(vp_route_hash::channel_route_hash)
                                                .unwrap_or(0);
                                            active_voice_channel_route
                                                .store(route, Ordering::Relaxed);
                                            let _ =
                                                tx_event.send(UiEvent::SetActiveVoiceRoute(route));
                                        }
                                        let _ = tx_event.send(UiEvent::MemberJoined {
                                            channel_id: channel_id.value,
                                            member: ui::model::MemberEntry {
                                                user_id,
                                                display_name: member.display_name,
                                                away_message: String::new(),
                                                muted: member.muted,
                                                deafened: member.deafened,
                                                self_muted: member.self_muted,
                                                self_deafened: member.self_deafened,
                                                streaming: member.streaming,
                                                speaking: false,
                                                avatar_url: None,
                                            },
                                        });
                                    }
                                }
                                pb::presence_event::Kind::MemberLeft(ml) => {
                                    let left_user = ml
                                        .user_id
                                        .as_ref()
                                        .map(|u| u.value.clone())
                                        .unwrap_or_default();
                                    debug!(channel_id=%ml.channel_id.as_ref().map(|c| c.value.clone()).unwrap_or_default(), user_id=%left_user, "received member-left push event");
                                    if left_user == local_user_id {
                                        server_deafened.store(false, Ordering::Relaxed);
                                        active_voice_channel_route.store(0, Ordering::Relaxed);
                                        let _ = tx_event.send(UiEvent::SetActiveVoiceRoute(0));
                                        let _ = tx_event.send(UiEvent::AppendLog(
                                            "[moderation] you were removed from this channel"
                                                .into(),
                                        ));
                                    }
                                    let _ = tx_event.send(UiEvent::MemberLeft {
                                        channel_id: ml
                                            .channel_id
                                            .map(|c| c.value)
                                            .unwrap_or_default(),
                                        user_id: ml.user_id.map(|u| u.value).unwrap_or_default(),
                                    });
                                }
                                pb::presence_event::Kind::MemberVoiceStateChanged(vs) => {
                                    let user_id = vs
                                        .user_id
                                        .as_ref()
                                        .map(|u| u.value.clone())
                                        .unwrap_or_default();
                                    if user_id == local_user_id {
                                        server_deafened.store(vs.deafened, Ordering::Relaxed);
                                    }
                                    let _ = tx_event.send(UiEvent::MemberVoiceStateUpdated {
                                        channel_id: vs
                                            .channel_id
                                            .map(|c| c.value)
                                            .unwrap_or_default(),
                                        user_id,
                                        muted: vs.muted,
                                        deafened: vs.deafened,
                                        self_muted: vs.self_muted,
                                        self_deafened: vs.self_deafened,
                                        streaming: vs.streaming,
                                    });
                                }
                                pb::presence_event::Kind::UserOnlineStatusChanged(status) => {
                                    let user_id = status
                                        .user_id
                                        .as_ref()
                                        .map(|u| u.value.clone())
                                        .unwrap_or_default();
                                    let _ = tx_event.send(UiEvent::MemberAwayMessageUpdated {
                                        user_id,
                                        away_message: status.custom_status_text,
                                    });
                                }
                            }
                        }
                    }
                    PushEvent::Moderation {
                        event: m,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        if let Some(pb::moderation_event::Kind::UserKicked(ev)) = m.kind.clone() {
                            let _ = tx_event.send(UiEvent::MemberLeft {
                                channel_id: ev.channel_id.map(|c| c.value).unwrap_or_default(),
                                user_id: ev.target_user_id.map(|u| u.value).unwrap_or_default(),
                            });
                        }
                        let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] {:?}", m)));
                    }
                    PushEvent::Poke { event, event_seq } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                            "[poke] from={} message={}",
                            event.from_display_name, event.message
                        )));
                        let _ = tx_event.send(UiEvent::PokeReceived {
                            from_name: event.from_display_name,
                            message: event.message,
                        });
                    }
                    PushEvent::ChannelCreated {
                        event: cr,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        if let Some(channel) = cr.channel {
                            if let Some(ch_id) = channel.channel_id {
                                debug!(channel_id=%ch_id.value, name=%channel.name, event_seq, "received channel-created push event");
                                let _ = tx_event.send(UiEvent::ChannelCreated(
                                    ui::model::ChannelEntry {
                                        id: ch_id.value,
                                        name: channel.name,
                                        channel_type: ui::model::ChannelType::Voice,
                                        parent_id: channel.parent_channel_id.map(|pid| pid.value),
                                        position: channel.position,
                                        member_count: 0,
                                        user_limit: channel.user_limit,
                                    },
                                ));
                            }
                        }
                    }
                    PushEvent::ChannelRenamed {
                        event: cr,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        if let Some(channel) = cr.channel {
                            if let Some(ch_id) = channel.channel_id {
                                debug!(channel_id=%ch_id.value, name=%channel.name, event_seq, "received channel-renamed push event");
                                let _ = tx_event.send(UiEvent::ChannelRenamed(
                                    ui::model::ChannelEntry {
                                        id: ch_id.value,
                                        name: channel.name,
                                        channel_type: ui::model::ChannelType::Voice,
                                        parent_id: channel.parent_channel_id.map(|pid| pid.value),
                                        position: channel.position,
                                        member_count: 0,
                                        user_limit: channel.user_limit,
                                    },
                                ));
                            }
                        }
                    }
                    PushEvent::ChannelDeleted { event, event_seq } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        if let Some(channel_id) = event.channel_id {
                            debug!(channel_id=%channel_id.value, event_seq, "received channel-deleted push event");
                            let _ = tx_event.send(UiEvent::ChannelDeleted {
                                channel_id: channel_id.value,
                            });
                        }
                    }
                    PushEvent::ServerHint { hint: h, event_seq } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        let mut parts = vec![];
                        if h.receiver_report_interval_ms != 0 {
                            parts.push(format!("rr={}ms", h.receiver_report_interval_ms));
                        }
                        if h.max_stream_bitrate_bps != 0 {
                            parts.push(format!("stream_cap={}bps", h.max_stream_bitrate_bps));
                        }
                        if h.max_voice_bitrate_bps != 0 {
                            parts.push(format!("voice_cap={}bps", h.max_voice_bitrate_bps));
                        }
                        let msg = if parts.is_empty() {
                            "server_hint".into()
                        } else {
                            format!("server_hint {}", parts.join(" "))
                        };
                        let _ = tx_event.send(UiEvent::AppendLog(format!("[hint] {msg}")));
                    }
                    PushEvent::Snapshot {
                        snapshot,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                            "[sync] received snapshot push server_id={} channels={} member_scopes={} self_user_id={}",
                            snapshot.server_id.as_ref().map(|sid| sid.value.clone()).unwrap_or_default(),
                            snapshot.channels.len(),
                            snapshot.channel_members.len(),
                            snapshot.self_user_id.as_ref().map(|u| u.value.clone()).unwrap_or_default(),
                        )));
                    }
                    PushEvent::Unknown(_) => {}
                }
            }
        });
    }

    let selected_after_sync =
        choose_initial_selected_channel(&snapshot, initial_active_channel.as_deref());

    if let Some(channel_id) = selected_after_sync.as_ref() {
        let route = uuid::Uuid::parse_str(&channel_id)
            .map(vp_route_hash::channel_route_hash)
            .unwrap_or(0);
        active_voice_channel_route.store(route, Ordering::Relaxed);
    } else {
        active_voice_channel_route.store(0, Ordering::Relaxed);
    }

    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Connected,
        "Connected and ready",
    );

    let (voice_die_tx, mut voice_die_rx) = watch::channel::<bool>(false);
    let _session_voice_flag = SessionVoiceFlag::new(session_voice_active.clone());
    let _ = tx_event.send(UiEvent::VoiceSessionHealth(true));

    let _voice_send = tokio::spawn(voice_send_loop(
        conn.clone(),
        encoder.clone(),
        capture.clone(),
        playout.clone(),
        capture_dsp.clone(),
        tx_event.clone(),
        active_voice_channel_route.clone(),
        ptt_active.clone(),
        self_muted.clone(),
        self_deafened.clone(),
        server_deafened.clone(),
        input_gain.clone(),
        loopback_active.clone(),
        audio_runtime.clone(),
        voice_counters.clone(),
        local_user_id.clone(),
        cfg.push_to_talk,
        voice_die_tx.clone(),
    ));

    let _voice_recv = tokio::spawn(voice_recv_loop(
        conn.clone(),
        playout.clone(),
        capture_dsp.clone(),
        self_deafened.clone(),
        server_deafened.clone(),
        output_gain.clone(),
        per_user_audio.clone(),
        audio_runtime.clone(),
        tx_event.clone(),
        voice_counters.clone(),
        voice_die_tx.clone(),
    ));

    let disp_keepalive = dispatcher.clone();
    let ctl_keepalive = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            if let Err(e) = disp_keepalive.ping().await {
                return Err::<(), anyhow::Error>(e);
            }
        }
    });

    // Track the active channel (for SendChat and other channel-scoped operations)
    let mut active_channel: Option<String> = selected_after_sync;

    tokio::pin!(ctl_keepalive);
    let mut audio_health_tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = audio_health_tick.tick() => {
                let capture_healthy = {
                    let cap = capture.read().await;
                    cap.is_healthy()
                };
                let playout_healthy = {
                    let out = playout.read().await;
                    out.is_healthy()
                };

                if !capture_healthy || !playout_healthy {
                    let _ = tx_event.send(UiEvent::AppendLog(
                        "[audio] stream error detected; attempting restart".into(),
                    ));
                    if let Err(e) = restart_audio_streams(
                        &capture,
                        &playout,
                        &selected_audio,
                        tx_event,
                        sample_rate,
                        channels,
                        frame_ms,
                    )
                    .await
                    {
                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                            "[audio] stream restart failed: {e:#}"
                        )));
                    }
                }
            }
            // Check for UI intents (non-blocking poll from crossbeam)
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                while let Ok(intent) = rx_intent.try_recv() {
                    match intent {
                        UiIntent::Quit => return Ok(()),
                        UiIntent::CancelConnect => {
                            set_connection_stage(tx_event, ui::model::ConnectionStage::Idle, "Disconnect requested by user");
                            return Err(anyhow!("disconnect requested"));
                        }
                        UiIntent::ConnectToServer { host, port, nickname } => {
                            cfg.display_name = nickname.clone();
                            let _ = tx_event.send(UiEvent::SetNick(nickname));

                            let new_server = format!("{host}:{port}");
                            cfg.server = new_server.clone();
                            cfg.server_name = host.clone();
                            let _ = tx_event.send(UiEvent::SetServerAddress { host, port });
                            set_connection_stage(
                                tx_event,
                                ui::model::ConnectionStage::Resolving,
                                format!("Reconnect requested: {new_server}"),
                            );
                            return Err(anyhow!("reconnect requested"));
                        }
                        UiIntent::TogglePtt => {
                            if active_voice_channel_route.load(Ordering::Relaxed) != 0 {
                                let new = !ptt_active.load(Ordering::Relaxed);
                                ptt_active.store(new, Ordering::Relaxed);
                            }
                        }
                        UiIntent::PttDown => {
                            if active_voice_channel_route.load(Ordering::Relaxed) != 0 {
                                ptt_active.store(true, Ordering::Relaxed);
                            }
                        }
                        UiIntent::PttUp => {
                            if active_voice_channel_route.load(Ordering::Relaxed) != 0 {
                                ptt_active.store(false, Ordering::Relaxed);
                            }
                        }
                        UiIntent::ToggleSelfMute => {
                            if active_voice_channel_route.load(Ordering::Relaxed) != 0 {
                                let new = !self_muted.load(Ordering::Relaxed);
                                self_muted.store(new, Ordering::Relaxed);
                                let _ = tx_event.send(UiEvent::SetSelfMuted(new));
                            }
                        }
                        UiIntent::ToggleSelfDeafen => {
                            if active_voice_channel_route.load(Ordering::Relaxed) != 0 {
                                let new = !self_deafened.load(Ordering::Relaxed);
                                self_deafened.store(new, Ordering::Relaxed);
                                let _ = tx_event.send(UiEvent::SetSelfDeafened(new));
                            }
                        }
                        UiIntent::SendChat { text, attachments } => {
                            if let Some(ref ch) = active_channel {
                                // Optimistic local echo
                                let now_ms = unix_ms() as i64;
                                let local_message_id = format!("local-{now_ms}");
                                debug!(
                                    message_id = %local_message_id,
                                    author_user_id = %local_user_id,
                                    channel_id = %ch,
                                    "sending chat message (optimistic local echo)"
                                );
                                let mut uploaded_attachments = Vec::new();
                                let mut upload_failed = false;
                                for attachment in &attachments {
                                    let result: anyhow::Result<()> = async {
                                        if attachment.asset_id.starts_with('/') {
                                            let uploaded = upload_attachment_quic(
                                                &conn,
                                                ch,
                                                attachment,
                                            )
                                            .await?;
                                            uploaded_attachments.push(uploaded);
                                        } else {
                                            uploaded_attachments.push(attachment.clone());
                                        }
                                        Ok(())
                                    }.await;
                                    if let Err(e) = result {
                                        let _ = tx_event.send(UiEvent::AttachmentUploadError {
                                            path: attachment.asset_id.clone(),
                                            error: e.to_string(),
                                        });
                                        let _ = tx_event.send(UiEvent::AppendLog(format!("[upload] failed: {e:#}")));
                                        uploaded_attachments.clear();
                                        upload_failed = true;
                                        break;
                                    }
                                }

                                if upload_failed {
                                    continue;
                                }

                                let _ = tx_event.send(UiEvent::MessageReceived(
                                    ui::model::ChatMessage {
                                        message_id: local_message_id,
                                        channel_id: ch.clone(),
                                        author_id: local_user_id.clone(),
                                        author_name: cfg.display_name.clone(),
                                        text: text.clone(),
                                        timestamp: now_ms,
                                        attachments: uploaded_attachments.clone(),
                                        reply_to: None,
                                        reactions: Vec::new(),
                                        pinned: false,
                                        edited: false,
                                    },
                                ));
                                let pb_attachments = uploaded_attachments
                                    .into_iter()
                                    .map(|a| pb::AttachmentRef {
                                        asset_id: Some(pb::AssetId { value: a.asset_id }),
                                        filename: a.filename,
                                        mime_type: a.mime_type,
                                        size_bytes: a.size_bytes,
                                        sha256: String::new(),
                                        ..Default::default()
                                    })
                                    .collect();
                                if let Err(e) = dispatcher.send_chat(ch, &text, pb_attachments).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[ctl] send_chat failed: {e:#}",
                                    )));
                                } else {
                                    let _ = tx_event.send(UiEvent::ClearPendingAttachments);
                                }
                            } else {
                                let _ = tx_event.send(UiEvent::AppendLog(
                                    "[ctl] no channel selected, cannot send message".into(),
                                ));
                            }
                        }
                        UiIntent::JoinChannel { channel_id } => {
                            match dispatcher.join_channel(&channel_id).await {
                                Ok(state) => {
                                    for member in &state.members {
                                        debug!(
                                            channel_id = %channel_id,
                                            user_id = %member.user_id.as_ref().map(|u| u.value.clone()).unwrap_or_default(),
                                            display_name = %member.display_name,
                                            "join/member upsert snapshot"
                                        );
                                    }
                                    active_channel = Some(channel_id.clone());
                                    if let Some(local_member) =
                                        state.members.iter().find(|m| {
                                            m.user_id
                                                .as_ref()
                                                .map(|u| u.value.as_str())
                                                == Some(local_user_id.as_str())
                                        })
                                    {
                                        server_deafened.store(local_member.deafened, Ordering::Relaxed);
                                    }
                                    let route = uuid::Uuid::parse_str(&channel_id)
                                        .map(vp_route_hash::channel_route_hash)
                                        .unwrap_or(0);
                                    active_voice_channel_route.store(route, Ordering::Relaxed);
                                    let _ = tx_event.send(UiEvent::SetActiveVoiceRoute(route));
                                    let _ = tx_event.send(UiEvent::SetChannelName(channel_id.clone()));
                                    let _ = tx_event.send(UiEvent::UpdateChannelMembers {
                                        channel_id: channel_id.clone(),
                                        members: state
                                            .members
                                            .into_iter()
                                            .map(|m| ui::model::MemberEntry {
                                                user_id: m.user_id.map(|u| u.value).unwrap_or_default(),
                                                display_name: m.display_name,
                                                away_message: String::new(),
                                                muted: m.muted,
                                                deafened: m.deafened,
                                                self_muted: m.self_muted,
                                                self_deafened: m.self_deafened,
                                                streaming: m.streaming,
                                                speaking: false,
                                                avatar_url: None,
                                            })
                                            .collect(),
                                    });
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] joined channel {channel_id}"),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] join failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::LeaveChannel => {
                            if let Some(ref ch) = active_channel {
                                if let Err(e) = dispatcher.leave_channel(ch).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] leave failed: {e:#}"),
                                    ));
                                }
                            }
                            active_channel = None;
                            server_deafened.store(false, Ordering::Relaxed);
                            active_voice_channel_route.store(0, Ordering::Relaxed);
                            let _ = tx_event.send(UiEvent::SetActiveVoiceRoute(0));
                        }
                        UiIntent::CreateChannel { name, description, channel_type, codec: _, quality, user_limit, parent_channel_id } => {
                            match dispatcher.create_channel(&name, &description, channel_type, quality * 1000, user_limit, parent_channel_id.as_deref()).await {
                                Ok(ch_id) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] created channel '{name}' ({ch_id})"),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] create_channel failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::RenameChannel { channel_id, new_name } => {
                            match dispatcher.rename_channel(&channel_id, &new_name).await {
                                Ok(()) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] renamed channel {channel_id} -> '{new_name}'"),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] rename_channel failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::DeleteChannel { channel_id } => {
                            match dispatcher.delete_channel(&channel_id).await {
                                Ok(()) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] deleted channel {channel_id}"),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] delete_channel failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::Help => {
                            let _ = tx_event.send(UiEvent::AppendLog(
                                "[help] Space=PTT | Enter=Send | Settings for audio config".into(),
                            ));
                        }
                        UiIntent::SetNoiseSuppression(enabled) => {
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_noise_suppression(enabled);
                            }
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[dsp] noise suppression: {enabled}"),
                            ));
                        }
                        UiIntent::SetAgcEnabled(enabled) => {
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_agc(enabled);
                            }
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[dsp] AGC: {enabled}"),
                            ));
                        }
                        UiIntent::SetEchoCancellation(enabled) => {
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_echo_cancellation(enabled);
                            }
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[dsp] echo cancellation: {enabled}"),
                            ));
                        }
                        UiIntent::SetVadThreshold(threshold) => {
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_vad_threshold(threshold);
                            }
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[dsp] VAD threshold: {threshold:.2}"),
                            ));
                        }
                        UiIntent::SetInputGain(gain) => {
                            input_gain.store(f32_to_u32(gain), Ordering::Relaxed);
                        }
                        UiIntent::SetOutputGain(gain) => {
                            output_gain.store(f32_to_u32(gain), Ordering::Relaxed);
                        }
                        UiIntent::SetUserOutputGain { user_id, gain } => {
                            if let Ok(mut per_user) = per_user_audio.write() {
                                per_user.entry(user_id).or_default().gain = gain.clamp(0.0, 2.0);
                            }
                        }
                        UiIntent::SetUserLocalMute { user_id, muted } => {
                            if let Ok(mut per_user) = per_user_audio.write() {
                                per_user.entry(user_id).or_default().muted = muted;
                            }
                        }
                        UiIntent::ToggleLoopback => {
                            let new = !loopback_active.load(Ordering::Relaxed);
                            loopback_active.store(new, Ordering::Relaxed);
                            let _ = tx_event.send(UiEvent::SetLoopbackActive(new));
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[audio] loopback: {new}"),
                            ));
                        }
                        UiIntent::SetInputDevice(dev) => {
                            let selected = normalize_device_name(&dev);
                            {
                                let mut state = selected_audio.lock().await;
                                state.input_device = selected;
                            }
                            if let Err(e) = restart_audio_streams(
                                &capture,
                                &playout,
                                &selected_audio,
                                tx_event,
                                sample_rate,
                                channels,
                                frame_ms,
                            )
                            .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to switch input device: {e:#}"
                                )));
                            }
                        }
                        UiIntent::SetOutputDevice(dev) => {
                            let selected = normalize_device_name(&dev);
                            {
                                let mut state = selected_audio.lock().await;
                                state.output_device = selected;
                            }
                            if let Err(e) = restart_audio_streams(
                                &capture,
                                &playout,
                                &selected_audio,
                                tx_event,
                                sample_rate,
                                channels,
                                frame_ms,
                            )
                            .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to switch output device: {e:#}"
                                )));
                            }
                        }
                        UiIntent::SetPlaybackMode(mode) => {
                            {
                                let mut state = selected_audio.lock().await;
                                state.playback_mode = normalize_playback_mode(&mode);
                            }

                            if let Err(e) = restart_audio_streams(
                                &capture,
                                &playout,
                                &selected_audio,
                                tx_event,
                                sample_rate,
                                channels,
                                frame_ms,
                            )
                            .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to switch playback mode: {e:#}"
                                )));
                            }
                        }
                        UiIntent::SetAwayMessage { message } => {
                            let _ = tx_event.send(UiEvent::SetAwayMessage(message.clone()));
                            match dispatcher.set_away_message(&message).await {
                                Ok(()) => {
                                    let text = if message.trim().is_empty() {
                                        "[presence] away message cleared".to_string()
                                    } else {
                                        format!("[presence] away message set: {message}")
                                    };
                                    let _ = tx_event.send(UiEvent::AppendLog(text));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[presence] set away message failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::SetAvatar { path } => {
                            match dispatcher.set_avatar(&path).await {
                                Ok(()) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        "[profile] avatar updated".to_string(),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[profile] set avatar failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::ApplySettings(ref settings) => {
                            // Apply all settings to the audio pipeline
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_noise_suppression(settings.noise_suppression);
                                d.set_agc(settings.agc_enabled);
                                d.set_vad_threshold(settings.vad_threshold);
                                d.set_agc_target(settings.agc_target_db);
                                d.set_echo_cancellation(settings.echo_cancellation);
                            }
                            input_gain.store(f32_to_u32(settings.input_gain), Ordering::Relaxed);
                            output_gain.store(f32_to_u32(settings.output_gain), Ordering::Relaxed);
                            if let Ok(mut per_user) = per_user_audio.write() {
                                *per_user = settings.per_user_audio.clone();
                            }
                            audio_runtime.apply(settings);
                            {
                                let mut enc = encoder.lock().await;
                                if let Err(e) = apply_fec_encoder_settings(&mut enc, &audio_runtime) {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[audio] failed to apply FEC settings: {e:#}"
                                    )));
                                }
                            }

                            // Update PTT mode
                            let is_ptt = settings.capture_mode == ui::model::CaptureMode::PushToTalk;
                            let is_continuous = settings.capture_mode == ui::model::CaptureMode::Continuous;
                            ptt_active.store(!is_ptt || is_continuous, Ordering::Relaxed);

                            {
                                let mut state = selected_audio.lock().await;
                                state.input_device = normalize_device_name(&settings.capture_device);
                                state.output_device = normalize_device_name(&settings.playback_device);
                                state.playback_mode = normalize_playback_mode(&settings.playback_mode);
                            }

                            if let Err(e) = restart_audio_streams(
                                &capture,
                                &playout,
                                &selected_audio,
                                tx_event,
                                sample_rate,
                                channels,
                                frame_ms,
                            )
                            .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to apply audio device settings: {e:#}"
                                )));
                            }

                            let _ = tx_event.send(UiEvent::AppendLog(
                                "[settings] applied".into(),
                            ));
                        }
                        UiIntent::SaveSettings(ref settings) => {
                            if let Err(e) = settings_io::save_settings(settings) {
                                let _ = tx_event.send(UiEvent::AppendLog(
                                    format!("[settings] save failed: {e:#}"),
                                ));
                            }
                        }
                        UiIntent::PokeUser { user_id, message } => {
                            if let Err(e) = dispatcher.poke_user(&user_id, &message).await {
                                let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] poke failed: {e:#}")));
                            } else {
                                let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] poked {user_id}")));
                            }
                        }
                        UiIntent::MuteUser { user_id, muted } => {
                            if let Some(ref ch) = active_channel {
                                let action = pb::moderation_action_request::Action::Mute(pb::MuteUser { muted, duration_seconds: 0 });
                                if let Err(e) = dispatcher.moderate_user(ch, &user_id, action).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] mute failed: {e:#}")));
                                }
                            }
                        }
                        UiIntent::DeafenUser { user_id, deafened } => {
                            if let Some(ref ch) = active_channel {
                                let action = pb::moderation_action_request::Action::Deafen(pb::DeafenUser { deafened, duration_seconds: 0 });
                                if let Err(e) = dispatcher.moderate_user(ch, &user_id, action).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] deafen failed: {e:#}")));
                                }
                            }
                        }
                        UiIntent::KickUser { user_id, reason } => {
                            if let Some(ref ch) = active_channel {
                                let action = pb::moderation_action_request::Action::Kick(pb::KickUser { reason });
                                if let Err(e) = dispatcher.moderate_user(ch, &user_id, action).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] kick failed: {e:#}")));
                                }
                            }
                        }
                        _ => {
                            // Remaining intents (moderation, file upload, etc.)
                        }
                    }
                }
            }

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let _ = tx_event.send(UiEvent::VoiceSessionHealth(false));
                    return Ok(());
                }
            }

            _ = voice_die_rx.changed() => {
                if *voice_die_rx.borrow() {
                    let _ = tx_event.send(UiEvent::VoiceSessionHealth(false));
                    return Err(anyhow!("voice loop terminated"));
                }
            }

            r = &mut ctl_keepalive => {
                let _ = tx_event.send(UiEvent::VoiceSessionHealth(false));
                return Err(anyhow!("control keepalive ended: {:?}", r));
            }
        }
    }
}

async fn upload_attachment_quic(
    conn: &quinn::Connection,
    channel_id: &str,
    attachment: &ui::model::AttachmentData,
) -> anyhow::Result<ui::model::AttachmentData> {
    use tokio::io::AsyncReadExt;

    let (mut send, mut recv) = conn.open_bi().await.context("open media stream")?;
    let mut file = tokio::fs::File::open(&attachment.asset_id)
        .await
        .with_context(|| format!("open attachment: {}", attachment.asset_id))?;
    let size_bytes = file.metadata().await?.len();

    let init = pb::MediaRequest {
        payload: Some(pb::media_request::Payload::UploadInit(pb::UploadInit {
            channel_id: Some(pb::ChannelId {
                value: channel_id.to_string(),
            }),
            filename: attachment.filename.clone(),
            mime: attachment.mime_type.clone(),
            size_bytes,
        })),
    };
    net::frame::write_delimited(&mut send, &init).await?;

    let ready: pb::MediaResponse = net::frame::read_delimited(&mut recv, 64 * 1024).await?;
    let max_chunk = match ready.payload {
        Some(pb::media_response::Payload::UploadReady(r)) => usize::max(r.max_chunk as usize, 4096),
        Some(pb::media_response::Payload::Error(e)) => {
            return Err(anyhow!("media upload rejected: {}", e.message))
        }
        _ => return Err(anyhow!("unexpected media upload response")),
    };

    let mut buf = vec![0u8; max_chunk];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        send.write_all(&buf[..n]).await?;
    }

    let complete: pb::MediaResponse = net::frame::read_delimited(&mut recv, 64 * 1024).await?;
    match complete.payload {
        Some(pb::media_response::Payload::UploadComplete(done)) => Ok(ui::model::AttachmentData {
            asset_id: done.attachment_id.map(|a| a.value).unwrap_or_default(),
            filename: done.filename,
            mime_type: done.mime,
            size_bytes: done.size_bytes,
            download_url: String::new(),
            thumbnail_url: None,
        }),
        Some(pb::media_response::Payload::Error(e)) => {
            Err(anyhow!("media upload failed: {}", e.message))
        }
        _ => Err(anyhow!("unexpected media upload completion")),
    }
}

async fn emit_telemetry_loop(
    tx_event: Sender<UiEvent>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    counters: Arc<VoiceTelemetryCounters>,
    send_queue_drop_count: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    let mut prev_tx_packets = 0u64;
    let mut prev_tx_bytes = 0u64;
    let mut prev_rx_packets = 0u64;
    let mut prev_rx_bytes = 0u64;
    let mut prev_late = 0u64;
    let mut prev_lost = 0u64;
    let mut prev_conceal = 0u64;

    while running.load(Ordering::Relaxed) && !*shutdown_rx.borrow() {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = tick.tick() => {}
        }

        let tx_packets = counters.tx_packets.load(Ordering::Relaxed);
        let tx_bytes = counters.tx_bytes.load(Ordering::Relaxed);
        let rx_packets = counters.rx_packets.load(Ordering::Relaxed);
        let rx_bytes = counters.rx_bytes.load(Ordering::Relaxed);
        let late = counters.late_packets.load(Ordering::Relaxed);
        let lost = counters.lost_packets.load(Ordering::Relaxed);
        let conceal = counters.concealment_frames.load(Ordering::Relaxed);
        let jitter_buffer_depth = counters.jitter_buffer_depth.load(Ordering::Relaxed) as u32;
        let peak_stream_level = f32::from_bits(
            counters
                .peak_stream_level_bits
                .swap(0.0f32.to_bits(), Ordering::Relaxed),
        );

        let tx_pps = tx_packets.saturating_sub(prev_tx_packets) as u32;
        let rx_pps = rx_packets.saturating_sub(prev_rx_packets) as u32;
        let tx_bitrate_bps = (tx_bytes.saturating_sub(prev_tx_bytes) * 8) as u32;
        let rx_bitrate_bps = (rx_bytes.saturating_sub(prev_rx_bytes) * 8) as u32;

        prev_tx_packets = tx_packets;
        prev_tx_bytes = tx_bytes;
        prev_rx_packets = rx_packets;
        prev_rx_bytes = rx_bytes;

        let late_delta = late.saturating_sub(prev_late) as u32;
        let lost_delta = lost.saturating_sub(prev_lost) as u32;
        let conceal_delta = conceal.saturating_sub(prev_conceal) as u32;

        prev_late = late;
        prev_lost = lost;
        prev_conceal = conceal;

        let vad_probability = if let Some(ref dsp) = capture_dsp {
            let d = dsp.lock().await;
            d.last_vad_probability()
        } else {
            0.0
        };

        let _ = tx_event.send(UiEvent::TelemetryUpdate(ui::model::TelemetryData {
            tx_bitrate_bps,
            rx_bitrate_bps,
            tx_pps,
            rx_pps,
            jitter_buffer_depth,
            late_packets: late_delta,
            lost_packets: lost_delta,
            concealment_frames: conceal_delta,
            peak_stream_level,
            send_queue_drop_count: send_queue_drop_count.load(Ordering::Relaxed),
            vad_probability,
            ..Default::default()
        }));
    }
}

async fn mic_test_loop(
    capture: Arc<RwLock<Arc<audio::capture::Capture>>>,
    playout: Arc<RwLock<Arc<audio::playout::Playout>>>,
    tx_event: Sender<UiEvent>,
    input_gain: Arc<std::sync::atomic::AtomicU32>,
    loopback_active: Arc<AtomicBool>,
    session_voice_active: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    shutdown_rx: watch::Receiver<bool>,
) {
    let sample_rate = 48_000u32;
    let channels = 1usize;
    let frame_ms = 20u32;
    let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels;

    let mut pcm = vec![0i16; frame_samples];
    let mut tick = tokio::time::interval(Duration::from_millis(10));

    loop {
        if !running.load(Ordering::Relaxed) || *shutdown_rx.borrow() {
            return;
        }
        tick.tick().await;

        if !loopback_active.load(Ordering::Relaxed) || session_voice_active.load(Ordering::Relaxed)
        {
            continue;
        }

        let capture_stream = capture.read().await.clone();
        if !capture_stream.read_frame(&mut pcm) {
            continue;
        }

        let gain = u32_to_f32(input_gain.load(Ordering::Relaxed));
        if (gain - 1.0).abs() > 0.001 {
            for s in pcm.iter_mut() {
                *s = (*s as f32 * gain).clamp(-32768.0, 32767.0) as i16;
            }
        }

        let playout_stream = playout.read().await.clone();
        playout_stream.push_pcm(&pcm);
        let waveform = build_mic_test_waveform(&pcm, 96);
        let _ = tx_event.send(UiEvent::MicTestWaveform(waveform));
    }
}

async fn voice_send_loop(
    conn: quinn::Connection,
    encoder: Arc<Mutex<audio::opus::OpusEncoder>>,
    capture: Arc<RwLock<Arc<audio::capture::Capture>>>,
    playout: Arc<RwLock<Arc<audio::playout::Playout>>>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    tx_event: Sender<UiEvent>,
    active_voice_channel_route: Arc<AtomicU32>,
    ptt_active: Arc<AtomicBool>,
    self_muted: Arc<AtomicBool>,
    self_deafened: Arc<AtomicBool>,
    server_deafened: Arc<AtomicBool>,
    input_gain: Arc<std::sync::atomic::AtomicU32>,
    loopback_active: Arc<AtomicBool>,
    audio_runtime: AudioRuntimeSettings,
    voice_counters: Arc<VoiceTelemetryCounters>,
    local_user_id: String,
    push_to_talk: bool,
    voice_die_tx: watch::Sender<bool>,
) {
    let mut seq: u32 = 0;
    let ssrc: u32 = rand::random();

    let sample_rate = 48_000u32;
    let channels = 1usize;
    let frame_ms = 20u32;
    let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels;

    let mut pcm = vec![0i16; frame_samples];
    let mut enc_out = vec![0u8; 4000];

    let mut tick = tokio::time::interval(Duration::from_millis(frame_ms as u64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut vad_report_counter = 0u32;
    let mut stream_ts_ms = 0u32;
    let mut last_local_speaking = false;

    loop {
        tick.tick().await;

        loop {
            let capture_stream = capture.read().await.clone();
            if capture_stream.read_frame(&mut pcm) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // Apply input gain
        let gain = u32_to_f32(input_gain.load(Ordering::Relaxed));
        if (gain - 1.0).abs() > 0.001 {
            for s in pcm.iter_mut() {
                *s = (*s as f32 * gain).clamp(-32768.0, 32767.0) as i16;
            }
        }

        // Loopback: feed capture directly to playout for mic testing
        if loopback_active.load(Ordering::Relaxed) {
            let playout_stream = playout.read().await.clone();
            playout_stream.push_pcm(&pcm);
            let waveform = build_mic_test_waveform(&pcm, 96);
            let _ = tx_event.send(UiEvent::MicTestWaveform(waveform));
        }

        let can_send = active_voice_channel_route.load(Ordering::Relaxed) != 0
            && !self_muted.load(Ordering::Relaxed)
            && !self_deafened.load(Ordering::Relaxed)
            && !server_deafened.load(Ordering::Relaxed)
            && (!push_to_talk || ptt_active.load(Ordering::Relaxed));
        if !can_send {
            if last_local_speaking {
                last_local_speaking = false;
                let _ = tx_event.send(UiEvent::VoiceActivity {
                    user_id: local_user_id.clone(),
                    speaking: false,
                });
            }
            let _ = tx_event.send(UiEvent::VoiceMeter {
                user_id: local_user_id.clone(),
                level: 0.0,
            });
            continue;
        }

        // Apply DSP pipeline (noise suppression + AGC + VAD)
        let mut is_voice = true;
        if let Some(ref dsp) = capture_dsp {
            let mut d = dsp.lock().await;
            d.process_frame(&mut pcm);
            is_voice = d.is_voice_active();

            // Report VAD level to GUI periodically
            vad_report_counter += 1;
            if vad_report_counter % 10 == 0 {
                let _ = tx_event.send(UiEvent::VadLevel(d.last_vad_probability()));
            }
        }

        if !is_voice {
            let mut attenuation_db =
                u32_to_f32(audio_runtime.denoise_attenuation_db.load(Ordering::Relaxed));
            if audio_runtime.typing_attenuation.load(Ordering::Relaxed) {
                attenuation_db = attenuation_db.min(-18.0);
            }
            let attn = 10.0_f32.powf((attenuation_db.min(0.0)) / 20.0);
            if attn < 0.999 {
                for s in pcm.iter_mut() {
                    *s = (*s as f32 * attn).clamp(-32768.0, 32767.0) as i16;
                }
            }
        }

        // Skip sending if VAD says no voice (and not PTT mode)
        let speaking_now = push_to_talk || is_voice;
        if speaking_now != last_local_speaking {
            last_local_speaking = speaking_now;
            let _ = tx_event.send(UiEvent::VoiceActivity {
                user_id: local_user_id.clone(),
                speaking: speaking_now,
            });
        }

        let local_level = if speaking_now {
            pcm.iter()
                .map(|s| (*s as i32).unsigned_abs() as f32 / 32768.0)
                .fold(0.0_f32, f32::max)
        } else {
            0.0
        };
        let _ = tx_event.send(UiEvent::VoiceMeter {
            user_id: local_user_id.clone(),
            level: local_level,
        });

        if !speaking_now {
            continue;
        }

        let n = match encoder.lock().await.encode(&pcm, &mut enc_out) {
            Ok(n) => n,
            Err(_) => continue,
        };

        let d = make_voice_datagram(
            active_voice_channel_route.load(Ordering::Relaxed),
            ssrc,
            seq,
            stream_ts_ms,
            is_voice,
            &enc_out[..n],
        );
        seq = seq.wrapping_add(1);
        stream_ts_ms = stream_ts_ms.wrapping_add(frame_ms);

        voice_counters.tx_packets.fetch_add(1, Ordering::Relaxed);
        voice_counters
            .tx_bytes
            .fetch_add(d.len() as u64, Ordering::Relaxed);

        if conn.send_datagram(d).is_err() {
            let _ = voice_die_tx.send(true);
            return;
        }
    }
}

async fn voice_recv_loop(
    conn: quinn::Connection,
    playout: Arc<RwLock<Arc<audio::playout::Playout>>>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    self_deafened: Arc<AtomicBool>,
    server_deafened: Arc<AtomicBool>,
    output_gain: Arc<std::sync::atomic::AtomicU32>,
    per_user_audio: Arc<std::sync::RwLock<HashMap<String, PerUserAudioSettings>>>,
    audio_runtime: AudioRuntimeSettings,
    tx_event: Sender<UiEvent>,
    voice_counters: Arc<VoiceTelemetryCounters>,
    voice_die_tx: watch::Sender<bool>,
) {
    const SPEAKING_HANGOVER_MS: u64 = 350;
    const STREAM_IDLE_DROP_MS: u64 = 10_000;
    const PLC_MAX_FRAMES: usize = 5;
    const JITTER_MISSING_WAIT_MS: u64 = 40;
    let fec_mode = match audio_runtime.fec_mode.load(Ordering::Relaxed) {
        0 => FecMode::Off,
        2 => FecMode::On,
        _ => FecMode::Auto,
    };
    let opus_use_inband_fec = fec_mode != FecMode::Off;

    let sample_rate = 48_000u32;
    let channels = 1usize;
    let frame_ms = 20u32;
    let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels;

    let mut streams = HashMap::<StreamKey, InboundStreamState>::new();
    let mut tick = tokio::time::interval(Duration::from_millis(frame_ms as u64));
    let mut mix_out = vec![0f32; frame_samples];
    let mut mixed_pcm = vec![0i16; frame_samples];

    loop {
        tokio::select! {
            datagram = conn.read_datagram() => {
                let d = match datagram {
                    Ok(d) => d,
                    Err(_e) => {
                        let _ = voice_die_tx.send(true);
                        return;
                    }
                };

                if self_deafened.load(Ordering::Relaxed) || server_deafened.load(Ordering::Relaxed) {
                    continue;
                }

                let packet = match parse_voice_payload(&d) {
                    Some(v) => v,
                    None => continue,
                };

                voice_counters.rx_packets.fetch_add(1, Ordering::Relaxed);
                voice_counters.rx_bytes.fetch_add(d.len() as u64, Ordering::Relaxed);

                let now_ms = unix_ms();
                let stream = streams
                    .entry(packet.stream_key())
                    .or_insert_with(|| InboundStreamState::new(sample_rate, channels as u8, 64));
                if stream.last_packet_ts_ms != 0 {
                    let gap = packet.ts_ms.wrapping_sub(stream.last_packet_ts_ms);
                    if gap > 10_000 {
                        stream.jitter.set_expected(packet.seq);
                    }
                    if packet.seq < stream.jitter.expected_seq() {
                        voice_counters.late_packets.fetch_add(1, Ordering::Relaxed);
                    }
                }
                stream.last_packet_ts_ms = packet.ts_ms;
                stream.last_packet_wall_ms = now_ms;
                if let Some(user_id) = packet.sender_user_id {
                    stream.user_id = Some(user_id.to_string());
                }
                stream.jitter.push(packet.seq, packet.payload.to_vec());
            }
            _ = tick.tick() => {
                if self_deafened.load(Ordering::Relaxed) || server_deafened.load(Ordering::Relaxed) {
                    continue;
                }

                let now_ms = unix_ms();
                mix_out.fill(0.0);
                let mut mixed_streams = 0usize;

                for stream in streams.values_mut() {
                    let mut frame_present = false;
                    voice_counters
                        .jitter_buffer_depth
                        .fetch_max(stream.jitter.depth() as u64, Ordering::Relaxed);
                    let mut frame_level = 0.0_f32;

                    let ready = stream
                        .jitter
                        .pop_ready(now_ms, JITTER_MISSING_WAIT_MS);

                    match ready {
                        audio::jitter::PopResult::Frame(frame) => {
                            let n = match stream.decoder.decode(&frame, &mut stream.pcm_out) {
                                Ok(n) => n,
                                Err(_) => 0,
                            };
                            if n > 0 {
                                frame_present = true;
                                stream.plc_frames = 0;
                                for (acc, sample) in mix_out[..n].iter_mut().zip(stream.pcm_out[..n].iter()) {
                                    let scaled = *sample as f32 * stream.effective_gain(&per_user_audio);
                                    frame_level = frame_level.max((scaled.abs() / 32768.0).min(1.0));
                                    *acc += scaled;
                                }
                                mixed_streams += 1;
                            }
                        }
                        audio::jitter::PopResult::Missing
                            if stream.last_packet_wall_ms != 0 && stream.plc_frames < PLC_MAX_FRAMES =>
                        {
                            voice_counters.lost_packets.fetch_add(1, Ordering::Relaxed);
                            let n = if opus_use_inband_fec {
                                match stream.jitter.peek_expected() {
                                    Some(next_frame) => stream
                                        .decoder
                                        .decode_fec(next_frame, &mut stream.pcm_out)
                                        .or_else(|_| stream.decoder.decode_plc(&mut stream.pcm_out))
                                        .unwrap_or(0),
                                    None => stream.decoder.decode_plc(&mut stream.pcm_out).unwrap_or(0),
                                }
                            } else {
                                stream.decoder.decode_plc(&mut stream.pcm_out).unwrap_or(0)
                            };
                            if n > 0 {
                                stream.plc_frames += 1;
                                voice_counters.concealment_frames.fetch_add(1, Ordering::Relaxed);
                                frame_present = true;
                                for (acc, sample) in mix_out[..n].iter_mut().zip(stream.pcm_out[..n].iter()) {
                                    let scaled = *sample as f32 * stream.effective_gain(&per_user_audio);
                                    frame_level = frame_level.max((scaled.abs() / 32768.0).min(1.0));
                                    *acc += scaled;
                                }
                                mixed_streams += 1;
                            }
                        }
                        audio::jitter::PopResult::Waiting
                            if stream.plc_frames < PLC_MAX_FRAMES && stream.last_packet_wall_ms != 0 =>
                        {
                            let since_packet = now_ms.saturating_sub(stream.last_packet_wall_ms);
                            if since_packet <= (PLC_MAX_FRAMES as u64 * frame_ms as u64) {
                                let n = match stream.decoder.decode_plc(&mut stream.pcm_out) {
                                    Ok(n) => n,
                                    Err(_) => 0,
                                };
                                if n > 0 {
                                    stream.plc_frames += 1;
                                    voice_counters.concealment_frames.fetch_add(1, Ordering::Relaxed);
                                    frame_present = true;
                                    for (acc, sample) in mix_out[..n].iter_mut().zip(stream.pcm_out[..n].iter()) {
                                        let scaled = *sample as f32 * stream.effective_gain(&per_user_audio);
                                        frame_level = frame_level.max((scaled.abs() / 32768.0).min(1.0));
                                        *acc += scaled;
                                    }
                                    mixed_streams += 1;
                                }
                            }
                        }
                        _ => {}
                    }

                    if frame_present {
                        stream.last_voice_frame_wall_ms = now_ms;
                    }

                    let speaking_now =
                        now_ms.saturating_sub(stream.last_voice_frame_wall_ms) <= SPEAKING_HANGOVER_MS;
                    stream.speaking = speaking_now;
                    if speaking_now != stream.last_emitted_speaking {
                        stream.last_emitted_speaking = speaking_now;
                        if let Some(user_id) = stream.user_id.as_ref() {
                            let _ = tx_event.send(UiEvent::VoiceActivity {
                                user_id: user_id.clone(),
                                speaking: speaking_now,
                            });
                        }
                    }

                    stream.level = if speaking_now { frame_level.max(stream.level * 0.75) } else { 0.0 };
                    voice_counters.observe_peak_stream_level(stream.level);
                    if let Some(user_id) = stream.user_id.as_ref() {
                        let _ = tx_event.send(UiEvent::VoiceMeter {
                            user_id: user_id.clone(),
                            level: stream.level,
                        });
                    }
                }

                streams.retain(|_, stream| {
                    let idle = now_ms.saturating_sub(stream.last_packet_wall_ms);
                    if idle >= STREAM_IDLE_DROP_MS {
                        if stream.last_emitted_speaking {
                            if let Some(user_id) = stream.user_id.as_ref() {
                                let _ = tx_event.send(UiEvent::VoiceActivity { user_id: user_id.clone(), speaking: false });
                            }
                        }
                        return false;
                    }
                    true
                });

                let speaking_streams = streams.values().filter(|s| s.speaking).count();
                mixed_pcm.fill(0);

                if mixed_streams > 0 {
                    for (dst, sample) in mixed_pcm.iter_mut().zip(mix_out.iter()) {
                        let x = *sample / 32768.0;
                        let soft = (x / (1.0 + x.abs())).clamp(-1.0, 1.0);
                        *dst = (soft * 32768.0) as i16;
                    }
                }

                let mut output_mul = u32_to_f32(output_gain.load(Ordering::Relaxed));

                if audio_runtime.output_auto_level.load(Ordering::Relaxed) && mixed_streams > 0 {
                    let peak = mixed_pcm
                        .iter()
                        .map(|s| (*s as i32).unsigned_abs() as f32 / 32768.0)
                        .fold(0.0_f32, f32::max);
                    if peak > 0.001 {
                        let target_peak = 0.8_f32;
                        let norm = (target_peak / peak).clamp(0.5, 2.0);
                        output_mul *= norm;
                    }
                }

                if audio_runtime.ducking_enabled.load(Ordering::Relaxed) && speaking_streams > 0 {
                    let duck_db = u32_to_f32(
                        audio_runtime
                            .ducking_attenuation_db
                            .load(Ordering::Relaxed),
                    )
                    .min(0.0);
                    output_mul *= 10.0_f32.powf(duck_db / 20.0);
                }

                if mixed_streams == 0 && audio_runtime.comfort_noise.load(Ordering::Relaxed) {
                    let noise = u32_to_f32(audio_runtime.comfort_noise_level.load(Ordering::Relaxed))
                        .clamp(0.0, 0.1);
                    if noise > 0.0 {
                        for s in mixed_pcm.iter_mut() {
                            let n = (rand::random::<f32>() * 2.0 - 1.0) * noise * 32767.0;
                            *s = n as i16;
                        }
                    }
                }

                if mixed_streams == 0 && !audio_runtime.comfort_noise.load(Ordering::Relaxed) {
                    continue;
                }

                if (output_mul - 1.0).abs() > 0.001 {
                    for s in mixed_pcm.iter_mut() {
                        *s = (*s as f32 * output_mul).clamp(-32768.0, 32767.0) as i16;
                    }
                }

                if let Some(ref dsp) = capture_dsp {
                    let mut d = dsp.lock().await;
                    d.feed_echo_reference(&mixed_pcm);
                }

                let playout_stream = playout.read().await.clone();
                playout_stream.push_pcm(&mixed_pcm);
            }
        }
    }
}

struct InboundStreamState {
    jitter: audio::jitter::JitterBuffer,
    decoder: audio::opus::OpusDecoder,
    pcm_out: Vec<i16>,
    user_id: Option<String>,
    level: f32,
    last_packet_ts_ms: u32,
    last_packet_wall_ms: u64,
    last_voice_frame_wall_ms: u64,
    plc_frames: usize,
    speaking: bool,
    last_emitted_speaking: bool,
}

impl InboundStreamState {
    fn new(sample_rate: u32, channels: u8, max_frames: usize) -> Self {
        let channel_count = channels as usize;
        let frame_samples = (sample_rate as usize * 20 / 1000) * channel_count;
        Self {
            jitter: audio::jitter::JitterBuffer::new(max_frames),
            decoder: audio::opus::OpusDecoder::new(sample_rate, channels)
                .expect("inbound opus decoder init"),
            pcm_out: vec![0i16; frame_samples],
            user_id: None,
            level: 0.0,
            last_packet_ts_ms: 0,
            last_packet_wall_ms: 0,
            last_voice_frame_wall_ms: 0,
            plc_frames: 0,
            speaking: false,
            last_emitted_speaking: false,
        }
    }
}

impl InboundStreamState {
    fn effective_gain(
        &self,
        per_user_audio: &std::sync::RwLock<HashMap<String, PerUserAudioSettings>>,
    ) -> f32 {
        let Some(user_id) = self.user_id.as_ref() else {
            return 1.0;
        };
        let Ok(per_user) = per_user_audio.read() else {
            return 1.0;
        };
        per_user
            .get(user_id)
            .map(|settings| {
                if settings.muted {
                    0.0
                } else {
                    settings.gain.clamp(0.0, 2.0)
                }
            })
            .unwrap_or(1.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum StreamKey {
    Sender(uuid::Uuid),
    Ssrc(u32),
}

struct InboundVoice<'a> {
    sender_user_id: Option<uuid::Uuid>,
    channel_id: Option<uuid::Uuid>,
    ssrc: u32,
    seq: u32,
    ts_ms: u32,
    payload: &'a [u8],
}

impl InboundVoice<'_> {
    fn stream_key(&self) -> StreamKey {
        self.sender_user_id
            .map(StreamKey::Sender)
            .unwrap_or(StreamKey::Ssrc(self.ssrc))
    }
}

fn parse_voice_payload(d: &Bytes) -> Option<InboundVoice<'_>> {
    if d.len() < VOICE_HDR_LEN {
        return None;
    }
    if d[0] != VOICE_VERSION {
        return None;
    }
    let hdr_len = u16::from_be_bytes([d[2], d[3]]) as usize;
    if d.len() <= hdr_len {
        return None;
    }
    let ssrc = u32::from_be_bytes([d[8], d[9], d[10], d[11]]);
    let seq = u32::from_be_bytes([d[12], d[13], d[14], d[15]]);
    let ts_ms = u32::from_be_bytes([d[16], d[17], d[18], d[19]]);

    match hdr_len {
        VOICE_HDR_LEN => Some(InboundVoice {
            sender_user_id: None,
            channel_id: None,
            ssrc,
            seq,
            ts_ms,
            payload: &d[hdr_len..],
        }),
        VOICE_FORWARDED_HDR_LEN => {
            let sender_user_id = uuid::Uuid::from_slice(&d[20..36]).ok();
            let channel_id = uuid::Uuid::from_slice(&d[36..52]).ok();
            Some(InboundVoice {
                sender_user_id,
                channel_id,
                ssrc,
                seq,
                ts_ms,
                payload: &d[hdr_len..],
            })
        }
        _ => None,
    }
}

fn build_mic_test_waveform(pcm: &[i16], points: usize) -> Vec<f32> {
    if pcm.is_empty() || points == 0 {
        return Vec::new();
    }

    let chunk = (pcm.len() / points.max(1)).max(1);
    let mut out = Vec::with_capacity(points);

    for i in 0..points {
        let start = i * chunk;
        if start >= pcm.len() {
            break;
        }

        let end = ((i + 1) * chunk).min(pcm.len());
        let peak = pcm[start..end]
            .iter()
            .map(|s| (*s as f32).abs() / 32768.0)
            .fold(0.0_f32, f32::max);
        out.push(peak);
    }

    out
}

fn f32_to_u32(f: f32) -> u32 {
    f.to_bits()
}

fn u32_to_f32(u: u32) -> f32 {
    f32::from_bits(u)
}

fn unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct SessionVoiceFlag(Arc<AtomicBool>);

impl SessionVoiceFlag {
    fn new(flag: Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::Relaxed);
        Self(flag)
    }
}

impl Drop for SessionVoiceFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

struct Backoff {
    min: Duration,
    max: Duration,
    cur: Duration,
}

impl Backoff {
    fn new(min: Duration, max: Duration) -> Self {
        Self { min, max, cur: min }
    }
    fn reset(&mut self) {
        self.cur = self.min;
    }
    async fn sleep(&mut self) {
        let jitter = rand::random::<u64>() % 150;
        sleep(self.cur + Duration::from_millis(jitter)).await;
        self.cur = (self.cur * 2).min(self.max);
    }
}

fn make_endpoint_with_optional_pinning(cfg: &Config) -> Result<quinn::Endpoint> {
    if let Ok(pin_hex) = std::env::var("VP_TLS_PIN_SHA256_HEX") {
        let pin = hex_to_32(&pin_hex)?;
        return make_pinned_endpoint(pin, &cfg.alpn);
    }

    if cfg.ca_cert_pem.trim().is_empty() {
        return Err(anyhow!(
            "VP_CA_CERT_PEM (or --ca-cert-pem) is required in this build"
        ));
    }

    net::quic::make_ca_endpoint(&cfg.ca_cert_pem, &cfg.alpn)
}

fn make_pinned_endpoint(pin_sha256: [u8; 32], alpn: &str) -> Result<quinn::Endpoint> {
    use quinn::{ClientConfig, Endpoint};
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};
    use std::{net::SocketAddr, sync::Arc};

    #[derive(Debug)]
    struct Pinner {
        pin: [u8; 32],
    }

    impl ServerCertVerifier for Pinner {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> std::result::Result<ServerCertVerified, rustls::Error> {
            let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
            if digest.as_ref() == self.pin {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General("cert pin mismatch".into()))
            }
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinner { pin: pin_sha256 }))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![alpn.as_bytes().to_vec()];

    let mut endpoint = Endpoint::client("[::]:0".parse::<SocketAddr>()?)?;
    endpoint.set_default_client_config(ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    )));
    Ok(endpoint)
}

fn hex_to_32(s: &str) -> Result<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return Err(anyhow!("expected 64 hex chars, got {}", s.len()));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| anyhow!("invalid hex at byte {}", i))?;
        out[i] = b;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{apply_authoritative_snapshot, choose_initial_selected_channel};
    use crate::{
        proto::voiceplatform::v1 as pb,
        ui::{model::ChannelType, UiEvent},
    };
    use crossbeam_channel::bounded;

    #[test]
    fn choose_initial_selected_channel_preserves_requested_when_present() {
        let requested = "channel-b";
        let snapshot = pb::InitialStateSnapshot {
            channels: vec![
                pb::ChannelSnapshot {
                    info: Some(pb::ChannelInfo {
                        channel_id: Some(pb::ChannelId {
                            value: "channel-a".into(),
                        }),
                        name: "General".into(),
                        ..Default::default()
                    }),
                },
                pb::ChannelSnapshot {
                    info: Some(pb::ChannelInfo {
                        channel_id: Some(pb::ChannelId {
                            value: requested.into(),
                        }),
                        name: "Gaming".into(),
                        ..Default::default()
                    }),
                },
            ],
            default_channel_id: Some(pb::ChannelId {
                value: "channel-a".into(),
            }),
            ..Default::default()
        };

        assert_eq!(
            choose_initial_selected_channel(&snapshot, Some(requested)),
            Some(requested.to_string())
        );
    }

    #[test]
    fn apply_authoritative_snapshot_sets_channel_and_members_lists() {
        let snapshot = pb::InitialStateSnapshot {
            server_id: Some(pb::ServerId {
                value: "server-1".into(),
            }),
            self_user_id: Some(pb::UserId {
                value: "user-1".into(),
            }),
            channels: vec![pb::ChannelSnapshot {
                info: Some(pb::ChannelInfo {
                    channel_id: Some(pb::ChannelId {
                        value: "channel-a".into(),
                    }),
                    name: "General".into(),
                    ..Default::default()
                }),
            }],
            channel_members: vec![pb::ChannelMembersSnapshot {
                channel_id: Some(pb::ChannelId {
                    value: "channel-a".into(),
                }),
                members: vec![pb::ChannelMember {
                    user_id: Some(pb::UserId {
                        value: "user-2".into(),
                    }),
                    display_name: "Alice".into(),
                    ..Default::default()
                }],
            }],
            default_channel_id: Some(pb::ChannelId {
                value: "channel-a".into(),
            }),
            ..Default::default()
        };

        let (tx, rx) = bounded::<UiEvent>(16);
        apply_authoritative_snapshot(&snapshot, &tx, None);

        let events = rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|ev| matches!(
            ev,
            UiEvent::SetChannels(channels)
                if channels.len() == 1
                    && channels[0].id == "channel-a"
                    && channels[0].name == "General"
                    && matches!(channels[0].channel_type, ChannelType::Voice)
        )));
        assert!(events.iter().any(|ev| matches!(
            ev,
            UiEvent::UpdateChannelMembers { channel_id, members }
                if channel_id == "channel-a"
                    && members.len() == 1
                    && members[0].user_id == "user-2"
                    && members[0].display_name == "Alice"
        )));
    }
}
