use crate::{errors::ControlResult, ids::*, repo::ControlRepo};
use serde_json::json;
use ulid::Ulid;

#[derive(Clone)]
pub struct AuditWriter;

impl AuditWriter {
    pub async fn write<R: ControlRepo + ?Sized>(
        &self,
        repo: &R,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        server: ServerId,
        actor: Option<UserId>,
        action: &str,
        target_type: &str,
        target_id: &str,
        context: serde_json::Value,
    ) -> ControlResult<()> {
        repo.insert_audit(
            tx,
            Ulid::new().to_string(),
            server,
            actor,
            action,
            target_type,
            target_id,
            context,
        )
        .await
    }

    pub fn ctx_kv(k: &str, v: impl Into<serde_json::Value>) -> serde_json::Value {
        json!({ k: v.into() })
    }
}
