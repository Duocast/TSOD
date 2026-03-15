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
    pub channel_type: i32,
    pub description: String,
    pub bitrate_bps: i32,
    pub opus_profile: i32,
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
    pub channel_type: i32,
    pub description: String,
    pub bitrate_bps: i32,
    pub opus_profile: i32,
}

/// Create channel input
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelCreate {
    pub name: String,
    pub parent_id: Option<ChannelId>,
    pub max_members: Option<i32>,
    pub max_talkers: Option<i32>,
    pub channel_type: i32,
    pub description: String,
    pub bitrate_bps: i32,
    pub opus_profile: i32,
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
    pub custom_status_text: String,
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
    /// Optional attachments from the gateway. Only asset_id is trusted.
    pub attachments: Option<Json>,
}

/// Canonical attachment row loaded from storage.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attachment {
    pub id: uuid::Uuid,
    pub server_id: ServerId,
    pub channel_id: ChannelId,
    pub uploader_user_id: UserId,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub sha256: Option<String>,
    pub quarantined: bool,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermRoleRecord {
    pub role_id: String,
    pub name: String,
    pub color: i32,
    pub role_position: i32,
    pub is_everyone: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermChannelOverrideRecord {
    pub channel_id: ChannelId,
    pub role_id: Option<String>,
    pub user_id: Option<UserId>,
    pub cap: String,
    pub effect: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermAuditRow {
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermUserSummaryRecord {
    pub user_id: UserId,
    pub display_name: String,
    pub joined_at: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
    pub highest_role_position: i32,
    pub role_ids: Vec<String>,
    pub is_admin: bool,
}
/// User profile row returned from the database.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserProfileRow {
    pub user_id: UserId,
    pub server_id: ServerId,
    pub display_name: String,
    pub description: String,
    pub accent_color: i32,
    pub custom_status_text: String,
    pub custom_status_emoji: String,
    pub avatar_asset_url: String,
    pub banner_asset_url: String,
    pub links: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Badge row from user_badges joined with badge_definitions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserBadgeRow {
    pub badge_id: String,
    pub label: String,
    pub icon_url: String,
    pub tooltip: String,
}

/// Badge definition row from badge_definitions table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BadgeDefinitionRow {
    pub id: String,
    pub server_id: ServerId,
    pub label: String,
    pub icon_url: String,
    pub tooltip: String,
    pub position: i32,
}

/// Role display info for a user.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserRoleRow {
    pub role_id: String,
    pub name: String,
    pub color: i32,
    pub position: i32,
}

/// In-progress profile asset upload session.
#[derive(Clone, Debug)]
pub struct AssetUploadSession {
    pub session_id: uuid::Uuid,
    pub user_id: UserId,
    pub server_id: ServerId,
    pub purpose: String,
    pub mime_type: String,
    pub byte_length: i64,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
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
