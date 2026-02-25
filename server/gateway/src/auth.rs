use anyhow::{anyhow, Result};

use crate::proto::voiceplatform::v1 as pb;

#[derive(Debug, Clone)]
pub struct AuthedIdentity {
    pub user_id: String,      // UUID string
    pub server_id: String,    // UUID string
    pub display_name: String, // user-visible name
    pub is_admin: bool,
}

pub trait AuthProvider: Send + Sync + 'static {
    fn authenticate(&self, req: &pb::AuthRequest) -> Result<AuthedIdentity>;
}

#[derive(Debug, Clone)]
pub struct DevAuthProvider;

impl AuthProvider for DevAuthProvider {
    fn authenticate(&self, req: &pb::AuthRequest) -> Result<AuthedIdentity> {
        match req.method.as_ref() {
            Some(pb::auth_request::Method::DevToken(m)) => {
                if m.token == "dev" {
                    Ok(AuthedIdentity {
                        user_id: "00000000-0000-0000-0000-000000000001".to_string(),
                        server_id: "00000000-0000-0000-0000-0000000000aa".to_string(),
                        display_name: "dev".to_string(),
                        is_admin: true,
                    })
                } else if let Some(raw) = m.token.strip_prefix("dev:") {
                    let key = raw.trim();
                    if key.is_empty() {
                        return Err(anyhow!("invalid dev token"));
                    }
                    let user = deterministic_dev_user_id(key);
                    Ok(AuthedIdentity {
                        user_id: user,
                        server_id: "00000000-0000-0000-0000-0000000000aa".to_string(),
                        display_name: key.to_string(),
                        is_admin: true,
                    })
                } else {
                    Err(anyhow!("invalid dev token"))
                }
            }
            _ => Err(anyhow!("unsupported auth method in dev provider")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthProvider, DevAuthProvider};
    use crate::proto::voiceplatform::v1 as pb;

    #[test]
    fn dev_colon_token_produces_distinct_user_ids() {
        let provider = DevAuthProvider;
        let req_a = pb::AuthRequest {
            method: Some(pb::auth_request::Method::DevToken(pb::DevTokenAuth {
                token: "dev:alice".into(),
            })),
            ..Default::default()
        };
        let req_b = pb::AuthRequest {
            method: Some(pb::auth_request::Method::DevToken(pb::DevTokenAuth {
                token: "dev:bob".into(),
            })),
            ..Default::default()
        };

        let a = provider.authenticate(&req_a).expect("alice auth");
        let b = provider.authenticate(&req_b).expect("bob auth");
        assert_ne!(a.user_id, b.user_id);
    }
}

fn deterministic_dev_user_id(key: &str) -> String {
    use std::hash::{Hash, Hasher};

    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h1);
    let a = h1.finish() as u128;

    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    "vp-dev".hash(&mut h2);
    key.hash(&mut h2);
    let b = h2.finish() as u128;

    let raw = (a << 64) | b;
    let bytes = raw.to_be_bytes();
    let uuid = uuid::Uuid::from_bytes(bytes);
    uuid.to_string()
}
