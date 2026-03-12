use anyhow::{Context, Result};
use axoupdater::AxoUpdater;
use tracing::{info, warn};

const APP_ID: &str = "vp-client";

#[derive(Debug, Clone)]
pub enum UpdateCheckResult {
    UpToDate,
    UpdateAvailable { version: String },
    UnsupportedInstallType,
}

#[derive(Debug, Clone)]
pub enum UpdateInstallResult {
    Installed,
    UnsupportedInstallType,
}

pub async fn check_for_updates() -> Result<UpdateCheckResult> {
    info!("[update] checking for updates");

    let mut updater = AxoUpdater::new_for(APP_ID);
    let updater = match updater.load_receipt() {
        Ok(updater) => updater,
        Err(err) => {
            warn!("[update] no dist receipt available; updater unsupported: {err:#}");
            return Ok(UpdateCheckResult::UnsupportedInstallType);
        }
    };

    if updater
        .is_update_needed()
        .await
        .context("query update state")?
    {
        info!("[update] update available");
        Ok(UpdateCheckResult::UpdateAvailable {
            version: "new version available".to_string(),
        })
    } else {
        info!("[update] no update available");
        Ok(UpdateCheckResult::UpToDate)
    }
}

pub async fn install_update() -> Result<UpdateInstallResult> {
    info!("[update] install requested");

    let mut updater = AxoUpdater::new_for(APP_ID);
    let updater = match updater.load_receipt() {
        Ok(updater) => updater,
        Err(err) => {
            warn!("[update] install unsupported; missing dist receipt: {err:#}");
            return Ok(UpdateInstallResult::UnsupportedInstallType);
        }
    };

    updater.run().await.context("install update")?;
    info!("[update] install flow completed");
    Ok(UpdateInstallResult::Installed)
}
