//! Settings persistence: save/load AppSettings to a JSON file.

use crate::ui::model::AppSettings;
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
