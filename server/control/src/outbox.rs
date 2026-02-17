use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboxRecord {
    pub id: String,
    pub server_id: uuid::Uuid,
    pub topic: String,
    pub key: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[async_trait::async_trait]
pub trait OutboxPublisher: Send + Sync {
    async fn publish(&self, rec: OutboxRecord) -> anyhow::Result<()>;
}
