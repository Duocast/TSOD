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
                } else {
                    Err(anyhow!("invalid dev token"))
                }
            }
            _ => Err(anyhow!("unsupported auth method in dev provider")),
        }
    }
}
