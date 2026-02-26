use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::proto::voiceplatform::v1 as pb;

#[derive(Debug, Clone)]
pub struct AuthedIdentity {
    pub user_id: String,
    pub server_id: String,
    pub display_name: String,
    pub is_admin: bool,
}

pub trait AuthProvider: Send + Sync + 'static {
    fn authenticate(
        &self,
        req: &pb::AuthRequest,
        session_id: &str,
        auth_challenge: &[u8],
    ) -> Result<AuthedIdentity>;
}

#[derive(Debug, Clone)]
struct DeviceRecord {
    user_id: String,
    device_id: String,
}

#[derive(Debug, Default)]
struct DeviceRegistry {
    by_pubkey: HashMap<Vec<u8>, DeviceRecord>,
}

#[derive(Debug, Default)]
pub struct DevAuthProvider {
    registry: Mutex<DeviceRegistry>,
}

impl AuthProvider for DevAuthProvider {
    fn authenticate(
        &self,
        req: &pb::AuthRequest,
        session_id: &str,
        auth_challenge: &[u8],
    ) -> Result<AuthedIdentity> {
        match req.method.as_ref() {
            Some(pb::auth_request::Method::Device(device)) => {
                let device_id = device
                    .device_id
                    .as_ref()
                    .map(|d| d.value.trim())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("missing device_id"))?;
                if uuid::Uuid::parse_str(device_id).is_err() {
                    return Err(anyhow!("invalid device_id"));
                }
                if device.device_pubkey.len() != 32 {
                    return Err(anyhow!("invalid device_pubkey"));
                }
                if device.signature.len() != 64 {
                    return Err(anyhow!("invalid signature"));
                }

                let mut signed = Vec::with_capacity(auth_challenge.len() + session_id.len());
                signed.extend_from_slice(auth_challenge);
                signed.extend_from_slice(session_id.as_bytes());

                let verifier = ring::signature::UnparsedPublicKey::new(
                    &ring::signature::ED25519,
                    &device.device_pubkey,
                );
                verifier
                    .verify(&signed, &device.signature)
                    .map_err(|_| anyhow!("invalid device signature"))?;

                let mut registry = self
                    .registry
                    .lock()
                    .map_err(|_| anyhow!("registry lock poisoned"))?;
                let rec = registry
                    .by_pubkey
                    .entry(device.device_pubkey.clone())
                    .or_insert_with(|| DeviceRecord {
                        user_id: uuid::Uuid::new_v4().to_string(),
                        device_id: device_id.to_string(),
                    })
                    .clone();

                if rec.device_id != device_id {
                    return Err(anyhow!("device_id mismatch for registered key"));
                }

                Ok(AuthedIdentity {
                    user_id: rec.user_id,
                    server_id: "00000000-0000-0000-0000-0000000000aa".to_string(),
                    display_name: format!("guest-{}", &rec.device_id[..8]),
                    is_admin: true,
                })
            }
            _ => Err(anyhow!("unsupported auth method in dev provider")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthProvider, DevAuthProvider};
    use crate::proto::voiceplatform::v1 as pb;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn sign_payload(key: &Ed25519KeyPair, challenge: &[u8], session_id: &str) -> Vec<u8> {
        let mut payload = Vec::with_capacity(challenge.len() + session_id.len());
        payload.extend_from_slice(challenge);
        payload.extend_from_slice(session_id.as_bytes());
        key.sign(&payload).as_ref().to_vec()
    }

    #[test]
    fn same_device_gets_same_user_id() {
        let provider = DevAuthProvider::default();
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("pkcs8");
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("key");
        let challenge = b"abc123";
        let session_id = "session-1";
        let device_id = "11111111-1111-1111-1111-111111111111";

        let req = pb::AuthRequest {
            method: Some(pb::auth_request::Method::Device(pb::DeviceAuth {
                device_id: Some(pb::DeviceId {
                    value: device_id.to_string(),
                }),
                device_pubkey: key.public_key().as_ref().to_vec(),
                signature: sign_payload(&key, challenge, session_id),
            })),
            ..Default::default()
        };

        let a = provider
            .authenticate(&req, session_id, challenge)
            .expect("auth a");
        let b = provider
            .authenticate(&req, session_id, challenge)
            .expect("auth b");
        assert_eq!(a.user_id, b.user_id);
    }

    #[test]
    fn distinct_devices_get_distinct_user_ids() {
        let provider = DevAuthProvider::default();
        let rng = SystemRandom::new();
        let pkcs8_a = Ed25519KeyPair::generate_pkcs8(&rng).expect("pkcs8 a");
        let key_a = Ed25519KeyPair::from_pkcs8(pkcs8_a.as_ref()).expect("key a");
        let pkcs8_b = Ed25519KeyPair::generate_pkcs8(&rng).expect("pkcs8 b");
        let key_b = Ed25519KeyPair::from_pkcs8(pkcs8_b.as_ref()).expect("key b");
        let challenge = b"nonce";
        let session_id = "session-2";

        let req_a = pb::AuthRequest {
            method: Some(pb::auth_request::Method::Device(pb::DeviceAuth {
                device_id: Some(pb::DeviceId {
                    value: "22222222-2222-2222-2222-222222222222".to_string(),
                }),
                device_pubkey: key_a.public_key().as_ref().to_vec(),
                signature: sign_payload(&key_a, challenge, session_id),
            })),
            ..Default::default()
        };
        let req_b = pb::AuthRequest {
            method: Some(pb::auth_request::Method::Device(pb::DeviceAuth {
                device_id: Some(pb::DeviceId {
                    value: "33333333-3333-3333-3333-333333333333".to_string(),
                }),
                device_pubkey: key_b.public_key().as_ref().to_vec(),
                signature: sign_payload(&key_b, challenge, session_id),
            })),
            ..Default::default()
        };

        let a = provider
            .authenticate(&req_a, session_id, challenge)
            .expect("auth a");
        let b = provider
            .authenticate(&req_b, session_id, challenge)
            .expect("auth b");
        assert_ne!(a.user_id, b.user_id);
    }
}
