use anyhow::{anyhow, Context, Result};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    pub device_id: String,
    pub public_key: Vec<u8>,
    pkcs8: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredDeviceIdentity {
    device_id: String,
    pkcs8: Vec<u8>,
}

impl DeviceIdentity {
    pub fn load_or_create() -> Result<Self> {
        let path = identity_path();
        if let Ok(content) = std::fs::read(&path) {
            if let Ok(stored) = serde_json::from_slice::<StoredDeviceIdentity>(&content) {
                let key = Ed25519KeyPair::from_pkcs8(&stored.pkcs8)
                    .map_err(|_| anyhow!("invalid stored device key"))?;
                return Ok(Self {
                    device_id: stored.device_id,
                    public_key: key.public_key().as_ref().to_vec(),
                    pkcs8: stored.pkcs8,
                });
            }
        }

        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| anyhow!("failed to generate device keypair"))?;
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|_| anyhow!("failed to decode generated keypair"))?;
        let stored = StoredDeviceIdentity {
            device_id: uuid::Uuid::new_v4().to_string(),
            pkcs8: pkcs8.as_ref().to_vec(),
        };

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create identity dir {}", parent.display()))?;
        }
        write_identity_file_atomically(&path, &stored)?;

        Ok(Self {
            device_id: stored.device_id,
            public_key: key.public_key().as_ref().to_vec(),
            pkcs8: stored.pkcs8,
        })
    }

    pub fn sign_challenge(&self, challenge: &[u8], session_id: &str) -> Result<Vec<u8>> {
        let key = Ed25519KeyPair::from_pkcs8(&self.pkcs8)
            .map_err(|_| anyhow!("invalid device key material"))?;
        let mut payload = Vec::with_capacity(challenge.len() + session_id.len());
        payload.extend_from_slice(challenge);
        payload.extend_from_slice(session_id.as_bytes());
        Ok(key.sign(&payload).as_ref().to_vec())
    }
}

fn write_identity_file_atomically(path: &Path, stored: &StoredDeviceIdentity) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(stored)?;
    let pid = std::process::id();
    let suffix = uuid::Uuid::new_v4();
    let tmp_path = path.with_file_name(format!("device_identity.json.{pid}.{suffix}.tmp"));

    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .with_context(|| format!("open temp identity file {}", tmp_path.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("write temp identity file {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temp identity file {}", tmp_path.display()))?;
    }

    #[cfg(not(unix))]
    {
        std::fs::write(&tmp_path, &bytes)
            .with_context(|| format!("write temp identity file {}", tmp_path.display()))?;
    }

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename identity file {}", path.display()))?;

    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let dir = std::fs::File::open(parent)
            .with_context(|| format!("open identity dir {}", parent.display()))?;
        dir.sync_all()
            .with_context(|| format!("sync identity dir {}", parent.display()))?;
    }
    Ok(())
}

fn identity_path() -> PathBuf {
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
    base.join("tsod").join("device_identity.json")
}

fn dirs_fallback_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
