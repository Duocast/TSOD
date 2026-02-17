use anyhow::{anyhow, Result};

use crate::proto::voiceplatform::v1 as pb;

#[derive(Debug, Clone)]
pub struct AuthedIdentity {
    pub user_id: String,
    pub server_id: String,
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
                        user_id: "dev-user".to_string(),
                        server_id: "dev-server".to_string(),
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
