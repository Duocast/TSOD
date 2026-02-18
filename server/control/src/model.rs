use crate::ids::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub channel_id: ChannelId,
    pub user_id: UserId,
    pub display_name: String,
    pub muted: bool,
    pub deafened: bool,
    pub joined_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct ChannelCreate {
    pub name: String,
    pub parent_id: Option<ChannelId>,
    pub max_members: Option<i32>,
    pub max_talkers: Option<i32>,
}

#[derive(Clone, Debug)]
pub struct JoinChannel {
    pub channel_id: ChannelId,
    pub display_name: String,
}


#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: MessageId,
    pub server_id: ServerId,
    pub channel_id: ChannelId,
    pub author_user_id: UserId,
    pub text: String,
    pub attachments: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct SendMessage {
    pub channel_id: ChannelId,
    pub text: String,
    pub attachments: serde_json::Value,
}
