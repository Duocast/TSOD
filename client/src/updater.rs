use anyhow::{Context, Result};
use axoupdater::AxoUpdater;
use serde::Deserialize;
use tracing::{info, warn};

const APP_ID: &str = "vp-client";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB_REPO: &str = "Duocast/TSOD";

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

#[derive(Debug, Clone)]
struct PortableRelease {
    version: String,
    download_url: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

pub async fn check_for_updates() -> Result<UpdateCheckResult> {
    info!("[update] checking for updates");

    let mut updater = AxoUpdater::new_for(APP_ID);
    let updater = match updater.load_receipt() {
        Ok(updater) => updater,
        Err(err) => {
            warn!("[update] no dist receipt available: {err:#}");
            #[cfg(target_os = "windows")]
            {
                return check_portable_windows_release().await;
            }
            #[cfg(not(target_os = "windows"))]
            {
                return Ok(UpdateCheckResult::UnsupportedInstallType);
            }
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
            warn!("[update] install without dist receipt: {err:#}");
            #[cfg(target_os = "windows")]
            {
                return install_portable_windows_release().await;
            }
            #[cfg(not(target_os = "windows"))]
            {
                return Ok(UpdateInstallResult::UnsupportedInstallType);
            }
        }
    };

    updater.run().await.context("install update")?;
    info!("[update] install flow completed");
    Ok(UpdateInstallResult::Installed)
}

#[cfg(target_os = "windows")]
async fn check_portable_windows_release() -> Result<UpdateCheckResult> {
    let release = fetch_latest_portable_release().await?;
    if is_newer_release(&release.version, CURRENT_VERSION) {
        info!(
            "[update] portable Windows update available: current={}, latest={}",
            CURRENT_VERSION, release.version
        );
        Ok(UpdateCheckResult::UpdateAvailable {
            version: release.version,
        })
    } else {
        info!(
            "[update] portable Windows up to date: current={}, latest={}",
            CURRENT_VERSION, release.version
        );
        Ok(UpdateCheckResult::UpToDate)
    }
}

#[cfg(target_os = "windows")]
async fn install_portable_windows_release() -> Result<UpdateInstallResult> {
    let release = fetch_latest_portable_release().await?;
    if !is_newer_release(&release.version, CURRENT_VERSION) {
        info!("[update] portable Windows install skipped; already up to date");
        return Ok(UpdateInstallResult::Installed);
    }

    let current_exe = std::env::current_exe().context("resolve current executable path")?;
    let parent_dir = current_exe
        .parent()
        .context("resolve executable parent directory")?;

    let staged_exe = parent_dir.join("tsod-updated.exe");
    download_file(&release.download_url, &staged_exe).await?;
    launch_windows_swap_script(std::process::id(), &current_exe, &staged_exe)?;

    info!(
        "[update] portable Windows update staged at {}",
        staged_exe.display()
    );
    Ok(UpdateInstallResult::Installed)
}

#[cfg(target_os = "windows")]
async fn fetch_latest_portable_release() -> Result<PortableRelease> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, "tsod-client-updater")
        .send()
        .await
        .context("request latest GitHub release")?
        .error_for_status()
        .context("latest GitHub release returned error status")?;

    let release: GithubRelease = response
        .json()
        .await
        .context("parse latest GitHub release JSON")?;

    if release.draft {
        return Err(anyhow!("latest GitHub release is draft"));
    }
    if release.prerelease {
        warn!("[update] latest GitHub release is marked prerelease");
    }

    let asset = select_windows_asset(&release.assets)
        .ok_or_else(|| anyhow!("no Windows .exe asset found in latest GitHub release"))?;

    Ok(PortableRelease {
        version: release.tag_name.trim_start_matches('v').to_string(),
        download_url: asset.browser_download_url.clone(),
    })
}

#[cfg(target_os = "windows")]
fn select_windows_asset(assets: &[GithubAsset]) -> Option<&GithubAsset> {
    assets
        .iter()
        .find(|asset| asset.name.contains("windows") && asset.name.ends_with(".exe"))
        .or_else(|| assets.iter().find(|asset| asset.name.ends_with(".exe")))
}

#[cfg(target_os = "windows")]
async fn download_file(url: &str, destination: &std::path::Path) -> Result<()> {
    let client = reqwest::Client::new();
    let bytes = client
        .get(url)
        .header(reqwest::header::USER_AGENT, "tsod-client-updater")
        .send()
        .await
        .with_context(|| format!("download update executable from {url}"))?
        .error_for_status()
        .context("download executable returned error status")?
        .bytes()
        .await
        .context("read executable download bytes")?;

    tokio::fs::write(destination, &bytes)
        .await
        .with_context(|| format!("write staged executable to {}", destination.display()))?;

    Ok(())
}

#[cfg(target_os = "windows")]
fn launch_windows_swap_script(
    pid: u32,
    current_exe: &std::path::Path,
    staged_exe: &std::path::Path,
) -> Result<()> {
    let current = current_exe.to_string_lossy().to_string();
    let staged = staged_exe.to_string_lossy().to_string();

    let script_path = std::env::temp_dir().join(format!("tsod-self-update-{pid}.cmd"));
    let script = format!(
        "@echo off\r\n:wait\r\ntasklist /FI \"PID eq {pid}\" | find \"{pid}\" >nul\r\nif not errorlevel 1 (\r\n  timeout /T 1 /NOBREAK >nul\r\n  goto wait\r\n)\r\nmove /Y \"{staged}\" \"{current}\" >nul\r\nstart \"\" \"{current}\"\r\ndel \"%~f0\"\r\n"
    );

    std::fs::write(&script_path, script)
        .with_context(|| format!("write update script at {}", script_path.display()))?;

    std::process::Command::new("cmd")
        .arg("/C")
        .arg(script_path.as_os_str())
        .spawn()
        .context("spawn detached update swap script")?;

    Ok(())
}

#[cfg(target_os = "windows")]
fn is_newer_release(latest: &str, current: &str) -> bool {
    match (
        semver::Version::parse(latest.trim()),
        semver::Version::parse(current.trim()),
    ) {
        (Ok(latest), Ok(current)) => latest > current,
        _ => latest.trim() != current.trim(),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "windows")]
    use super::is_newer_release;

    #[cfg(target_os = "windows")]
    #[test]
    fn newer_semver_is_detected() {
        assert!(is_newer_release("0.2.0", "0.1.9"));
        assert!(!is_newer_release("0.2.0", "0.2.0"));
    }
}
