use crate::{ChannelId, MemberState, UserId};

#[derive(Clone, Debug)]
pub enum ControlEvent {
    Presence { channel_id: ChannelId, kind: PresenceKind },
    // In the future: Chat, Moderation, etc.
}

#[derive(Clone, Debug)]
pub enum PresenceKind {
    MemberJoined(MemberState),
    MemberLeft(UserId),
    VoiceStateChanged { user_id: UserId, muted: bool, deafened: bool },
}
