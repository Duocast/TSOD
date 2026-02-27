//! Settings persistence: save/load AppSettings to a JSON file.

use crate::ui::model::{AppSettings, AudioDeviceId, AudioDeviceInfo};
use anyhow::Result;
use std::path::PathBuf;

/// Returns the default settings file path.
/// Linux:   ~/.config/tsod/settings.json
/// Windows: %APPDATA%\tsod\settings.json
/// macOS:   ~/Library/Application Support/tsod/settings.json
pub fn settings_path() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    } else if cfg!(target_os = "macos") {
        dirs_fallback_home().join("Library/Application Support")
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs_fallback_home().join(".config"))
    };
    base.join("tsod").join("settings.json")
}

fn dirs_fallback_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Load settings from disk. Returns default if file doesn't exist or is invalid.
pub fn load_settings() -> AppSettings {
    let path = settings_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to parse settings file {}: {e}", path.display());
                AppSettings::default()
            }
        },
        Err(_) => AppSettings::default(),
    }
}

/// Save settings to disk.
pub fn save_settings(settings: &AppSettings) -> Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, json)?;
    tracing::info!("settings saved to {}", path.display());
    Ok(())
}

pub fn migrate_audio_device_ids(
    settings: &mut AppSettings,
    input_devices: &[AudioDeviceInfo],
    output_devices: &[AudioDeviceInfo],
) {
    settings.capture_device = migrate_device(
        &settings.capture_device,
        input_devices,
        AudioDeviceId::default_input(),
    );
    settings.playback_device = migrate_device(
        &settings.playback_device,
        output_devices,
        AudioDeviceId::default_output(),
    );
}

fn migrate_device(
    current: &AudioDeviceId,
    devices: &[AudioDeviceInfo],
    default_id: AudioDeviceId,
) -> AudioDeviceId {
    if current.is_default() {
        return default_id;
    }

    if devices.iter().any(|d| d.key == *current) {
        return current.clone();
    }

    let legacy_name = current.id.trim();
    if legacy_name.is_empty() || legacy_name == "(system default)" {
        return default_id;
    }

    let mut matches: Vec<&AudioDeviceInfo> = devices
        .iter()
        .filter(|d| d.label == legacy_name || d.display_label == legacy_name)
        .collect();

    if matches.is_empty() {
        return default_id;
    }

    matches.sort_by_key(|d| (!d.is_default, d.label.clone()));
    matches[0].key.clone()
}
