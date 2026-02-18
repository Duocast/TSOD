use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    JoinChannel,
    Speak,
    Stream,
    Upload,
    SendMessage,
    CreateChannel,
    ManageChannel,
    ModerateMembers,
    ManageRoles,
    MuteVoice,
}

impl Capability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Capability::JoinChannel => "join_channel",
            Capability::Speak => "speak",
            Capability::Stream => "stream",
            Capability::Upload => "upload",
            Capability::SendMessage => "send_message",
            Capability::CreateChannel => "create_channel",
            Capability::ManageChannel => "manage_channel",
            Capability::ModerateMembers => "moderate_members",
            Capability::ManageRoles => "manage_roles",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "join_channel" => Capability::JoinChannel,
            "speak" => Capability::Speak,
            "stream" => Capability::Stream,
            "upload" => Capability::Upload,
            "send_message" => Capability::SendMessage,
            "create_channel" => Capability::CreateChannel,
            "manage_channel" => Capability::ManageChannel,
            "moderate_members" => Capability::ModerateMembers,
            "manage_roles" => Capability::ManageRoles,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    Grant,
    Deny,
}

impl Effect {
    pub fn as_str(&self) -> &'static str {
        match self {
            Effect::Grant => "grant",
            Effect::Deny => "deny",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "grant" => Effect::Grant,
            "deny" => Effect::Deny,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny,
}
