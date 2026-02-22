//! Application state model for the GUI.

use std::collections::{HashMap, VecDeque};

/// Maximum number of chat messages to retain per channel.
const MAX_MESSAGES_PER_CHANNEL: usize = 500;

/// Maximum number of log lines.
const MAX_LOG_LINES: usize = 1000;

// ── Events from backend to UI ──────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum UiEvent {
    // Connection state
    SetConnected(bool),
    SetAuthed(bool),
    SetChannelName(String),
    SetNick(String),
    SetUserId(String),
    AppendLog(String),
    SetStatus(String),

    // Channel list
    SetChannels(Vec<ChannelEntry>),
    UpdateChannelMembers {
        channel_id: String,
        members: Vec<MemberEntry>,
    },

    // Chat
    MessageReceived(ChatMessage),
    MessageEdited {
        channel_id: String,
        message_id: String,
        new_text: String,
    },
    MessageDeleted {
        channel_id: String,
        message_id: String,
    },
    TypingIndicator {
        channel_id: String,
        user_name: String,
    },

    // Members
    MemberJoined {
        channel_id: String,
        member: MemberEntry,
    },
    MemberLeft {
        channel_id: String,
        user_id: String,
    },

    // Voice
    VadLevel(f32),
    VoiceActivity {
        user_id: String,
        speaking: bool,
    },

    // Telemetry
    TelemetryUpdate(TelemetryData),

    // Poke
    PokeReceived {
        from_name: String,
        message: String,
    },

    // User profile
    UserProfileLoaded(UserProfileData),
}

// ── Intents from UI to backend ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum UiIntent {
    Quit,
    SendChat { text: String },
    JoinChannel { channel_id: String },
    LeaveChannel,
    CreateChannel { name: String, channel_type: u8 },
    TogglePtt,
    PttDown,
    PttUp,
    ToggleSelfMute,
    ToggleSelfDeafen,
    Help,

    // Chat
    EditMessage { message_id: String, new_text: String },
    DeleteMessage { message_id: String },
    AddReaction { message_id: String, emoji: String },
    RemoveReaction { message_id: String, emoji: String },
    SendTyping,

    // Moderation
    KickUser { user_id: String, reason: String },
    BanUser { user_id: String, reason: String, duration: u32 },
    MuteUser { user_id: String, muted: bool },
    DeafenUser { user_id: String, deafened: bool },
    MoveUser { user_id: String, target_channel_id: String },
    TimeoutUser { user_id: String, duration: u32, reason: String },

    // Poke
    PokeUser { user_id: String, message: String },

    // Whisper
    SetWhisperTargets { targets: Vec<String> },

    // File upload
    UploadFile { path: String },

    // Settings
    SetNoiseSuppression(bool),
    SetAgcEnabled(bool),
    SetVadThreshold(f32),
    SetInputDevice(String),
    SetOutputDevice(String),
}

// ── Data types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ChannelEntry {
    pub id: String,
    pub name: String,
    pub channel_type: ChannelType,
    pub parent_id: Option<String>,
    pub position: u32,
    pub member_count: u32,
    pub user_limit: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelType {
    Text,
    Voice,
    Category,
}

#[derive(Debug, Clone)]
pub struct MemberEntry {
    pub user_id: String,
    pub display_name: String,
    pub muted: bool,
    pub deafened: bool,
    pub self_muted: bool,
    pub self_deafened: bool,
    pub streaming: bool,
    pub speaking: bool,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub message_id: String,
    pub channel_id: String,
    pub author_id: String,
    pub author_name: String,
    pub text: String,
    pub timestamp: i64,    // unix millis
    pub attachments: Vec<AttachmentData>,
    pub reply_to: Option<String>,
    pub reactions: Vec<ReactionData>,
    pub pinned: bool,
    pub edited: bool,
}

#[derive(Debug, Clone)]
pub struct AttachmentData {
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub download_url: String,
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReactionData {
    pub emoji: String,
    pub count: u32,
    pub me: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TelemetryData {
    pub rtt_ms: u32,
    pub loss_rate: f32,
    pub jitter_ms: u32,
    pub goodput_bps: u32,
    pub playout_delay_ms: u32,
    pub agc_gain_db: f32,
    pub vad_probability: f32,
}

#[derive(Debug, Clone)]
pub struct UserProfileData {
    pub user_id: String,
    pub display_name: String,
    pub description: String,
    pub status: String,
    pub badges: Vec<String>,
}

// ── Main UI model ──────────────────────────────────────────────────────

pub struct UiModel {
    // Connection
    pub connected: bool,
    pub authed: bool,
    pub nick: String,
    pub user_id: String,

    // Channels
    pub channels: Vec<ChannelEntry>,
    pub selected_channel: Option<String>,
    pub selected_channel_name: String,

    // Members (keyed by channel_id)
    pub members: HashMap<String, Vec<MemberEntry>>,
    pub speaking_users: HashMap<String, bool>,

    // Chat (keyed by channel_id)
    pub messages: HashMap<String, VecDeque<ChatMessage>>,
    pub chat_input: String,
    pub chat_input_focused: bool,
    pub typing_users: HashMap<String, Vec<(String, std::time::Instant)>>,

    // Voice
    pub ptt_enabled: bool,
    pub ptt_active: bool,
    pub self_muted: bool,
    pub self_deafened: bool,
    pub vad_level: Option<f32>,

    // Log
    pub log: VecDeque<String>,

    // Telemetry
    pub telemetry: TelemetryData,

    // UI toggles
    pub show_settings: bool,
    pub show_telemetry: bool,
    pub status_line: String,

    // Settings
    pub noise_suppression_enabled: bool,
    pub agc_enabled: bool,
    pub vad_threshold: f32,

    // Notifications
    pub notifications: VecDeque<Notification>,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub text: String,
    pub created: std::time::Instant,
    pub kind: NotificationKind,
}

#[derive(Debug, Clone)]
pub enum NotificationKind {
    Info,
    Poke,
    Mention,
    Error,
}

impl Default for UiModel {
    fn default() -> Self {
        Self {
            connected: false,
            authed: false,
            nick: "User".into(),
            user_id: String::new(),
            channels: Vec::new(),
            selected_channel: None,
            selected_channel_name: String::new(),
            members: HashMap::new(),
            speaking_users: HashMap::new(),
            messages: HashMap::new(),
            chat_input: String::new(),
            chat_input_focused: false,
            typing_users: HashMap::new(),
            ptt_enabled: true,
            ptt_active: false,
            self_muted: false,
            self_deafened: false,
            vad_level: None,
            log: VecDeque::new(),
            telemetry: TelemetryData::default(),
            show_settings: false,
            show_telemetry: false,
            status_line: String::new(),
            noise_suppression_enabled: true,
            agc_enabled: true,
            vad_threshold: 0.5,
            notifications: VecDeque::new(),
        }
    }
}

impl UiModel {
    pub fn apply_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::SetConnected(c) => self.connected = c,
            UiEvent::SetAuthed(a) => self.authed = a,
            UiEvent::SetChannelName(n) => self.selected_channel_name = n,
            UiEvent::SetNick(n) => self.nick = n,
            UiEvent::SetUserId(id) => self.user_id = id,
            UiEvent::AppendLog(line) => {
                self.log.push_back(line);
                if self.log.len() > MAX_LOG_LINES {
                    self.log.pop_front();
                }
            }
            UiEvent::SetStatus(s) => self.status_line = s,
            UiEvent::SetChannels(chs) => self.channels = chs,
            UiEvent::UpdateChannelMembers { channel_id, members } => {
                self.members.insert(channel_id, members);
            }
            UiEvent::MessageReceived(msg) => {
                let ch = msg.channel_id.clone();
                let msgs = self.messages.entry(ch).or_default();
                msgs.push_back(msg);
                if msgs.len() > MAX_MESSAGES_PER_CHANNEL {
                    msgs.pop_front();
                }
            }
            UiEvent::MessageEdited { channel_id, message_id, new_text } => {
                if let Some(msgs) = self.messages.get_mut(&channel_id) {
                    if let Some(msg) = msgs.iter_mut().find(|m| m.message_id == message_id) {
                        msg.text = new_text;
                        msg.edited = true;
                    }
                }
            }
            UiEvent::MessageDeleted { channel_id, message_id } => {
                if let Some(msgs) = self.messages.get_mut(&channel_id) {
                    msgs.retain(|m| m.message_id != message_id);
                }
            }
            UiEvent::TypingIndicator { channel_id, user_name } => {
                let typers = self.typing_users.entry(channel_id).or_default();
                typers.retain(|(name, _)| name != &user_name);
                typers.push((user_name, std::time::Instant::now()));
            }
            UiEvent::MemberJoined { channel_id, member } => {
                let members = self.members.entry(channel_id).or_default();
                members.push(member);
            }
            UiEvent::MemberLeft { channel_id, user_id } => {
                if let Some(members) = self.members.get_mut(&channel_id) {
                    members.retain(|m| m.user_id != user_id);
                }
            }
            UiEvent::VadLevel(v) => self.vad_level = Some(v),
            UiEvent::VoiceActivity { user_id, speaking } => {
                self.speaking_users.insert(user_id, speaking);
            }
            UiEvent::TelemetryUpdate(t) => self.telemetry = t,
            UiEvent::PokeReceived { from_name, message } => {
                let text = if message.is_empty() {
                    format!("{from_name} poked you!")
                } else {
                    format!("{from_name} poked you: {message}")
                };
                self.notifications.push_back(Notification {
                    text,
                    created: std::time::Instant::now(),
                    kind: NotificationKind::Poke,
                });
            }
            UiEvent::UserProfileLoaded(_profile) => {
                // TODO: store in profile cache
            }
        }

        // Expire old typing indicators (>5s)
        let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(5);
        for typers in self.typing_users.values_mut() {
            typers.retain(|(_, t)| *t > cutoff);
        }

        // Expire old notifications (>5s)
        let notif_cutoff = std::time::Instant::now() - std::time::Duration::from_secs(5);
        while self.notifications.front().is_some_and(|n| n.created < notif_cutoff) {
            self.notifications.pop_front();
        }
    }

    /// Get messages for the currently selected channel.
    pub fn current_messages(&self) -> Option<&VecDeque<ChatMessage>> {
        self.selected_channel
            .as_ref()
            .and_then(|ch| self.messages.get(ch))
    }

    /// Get members for the currently selected channel.
    pub fn current_members(&self) -> &[MemberEntry] {
        self.selected_channel
            .as_ref()
            .and_then(|ch| self.members.get(ch))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Get typing users for the currently selected channel.
    pub fn current_typing_users(&self) -> Vec<&str> {
        self.selected_channel
            .as_ref()
            .and_then(|ch| self.typing_users.get(ch))
            .map(|typers| typers.iter().map(|(name, _)| name.as_str()).collect())
            .unwrap_or_default()
    }
}
