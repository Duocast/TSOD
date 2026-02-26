use anyhow::{anyhow, Context, Result};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(stored) = serde_json::from_str::<StoredDeviceIdentity>(&content) {
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
        std::fs::write(&path, serde_json::to_vec_pretty(&stored)?)
            .with_context(|| format!("write identity file {}", path.display()))?;

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
