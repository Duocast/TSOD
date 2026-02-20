use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutboxId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AuditId(pub Uuid);

impl ServerId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl UserId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl ChannelId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
}

impl MessageId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl OutboxId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl AuditId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
