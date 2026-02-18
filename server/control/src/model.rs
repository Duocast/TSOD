use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::{
    ids::{AuditId, ChannelId, MessageId, OutboxId, ServerId, UserId},
    perms::Capability,
};

/// Channel row (matches your compiler: requires created_at + updated_at)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub server_id: ServerId,
    pub name: String,
    pub parent_id: Option<ChannelId>,
    pub max_members: Option<i32>,
    pub max_talkers: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Lightweight list item for UI
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelListItem {
    pub id: ChannelId,
    pub name: String,
    pub parent_id: Option<ChannelId>,
    pub max_members: Option<i32>,
    pub max_talkers: Option<i32>,
}

/// Create channel input
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelCreate {
    pub name: String,
    pub parent_id: Option<ChannelId>,
    pub max_members: Option<i32>,
    pub max_talkers: Option<i32>,
}

/// Join channel input
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JoinChannel {
    pub channel_id: ChannelId,
    pub display_name: String,
}

/// Member state (NO server_id, has joined_at per your compiler)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub channel_id: ChannelId,
    pub user_id: UserId,
    pub display_name: String,
    pub muted: bool,
    pub deafened: bool,
    pub joined_at: DateTime<Utc>,
}

/// Chat message (NO Default; uses author_user_id + attachments + created_at)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: MessageId,
    pub server_id: ServerId,
    pub channel_id: ChannelId,
    pub author_user_id: UserId,
    pub text: String,
    pub attachments: Json,
    pub created_at: DateTime<Utc>,
}

/// Send message input
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendMessage {
    pub channel_id: ChannelId,
    pub text: String,
    /// Optional attachments in a JSON envelope (URLs, inline metadata, etc.)
    pub attachments: Option<Json>,
}

/// Outbox event to be persisted
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboxEvent {
    pub id: OutboxId,
    pub server_id: ServerId,
    pub topic: String,
    pub payload_json: Json,
}

/// Outbox row returned to gateway poller
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboxEventRow {
    pub id: OutboxId,
    pub server_id: ServerId,
    pub topic: String,
    pub payload_json: Json,
}

/// Audit entry (insert-only)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: AuditId,
    pub server_id: ServerId,
    pub actor_user_id: Option<UserId>,
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub context_json: Json,
    pub created_at: DateTime<Utc>,
}

impl AuditEntry {
    pub fn new(
        server_id: ServerId,
        actor_user_id: Option<UserId>,
        action: impl Into<String>,
        target_type: impl Into<String>,
        target_id: impl Into<String>,
        context_json: Json,
    ) -> Self {
        Self {
            id: AuditId(uuid::Uuid::new_v4()),
            server_id,
            actor_user_id,
            action: action.into(),
            target_type: target_type.into(),
            target_id: target_id.into(),
            context_json,
            created_at: Utc::now(),
        }
    }
}

/// Permission check request (repo decides allow/deny)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub server_id: ServerId,
    pub user_id: UserId,
    pub is_admin: bool,
    pub capability: Capability,
    pub channel_id: Option<ChannelId>,
    pub target_user_id: Option<UserId>,
}
