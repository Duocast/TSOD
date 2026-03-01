//! Application state model for the GUI.

use std::collections::{HashMap, HashSet, VecDeque};
use tracing::debug;

/// Maximum number of chat messages to retain per channel.
const MAX_MESSAGES_PER_CHANNEL: usize = 500;

/// Maximum number of log lines.
const MAX_LOG_LINES: usize = 1000;

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub enum AudioBackend {
    Auto,
    Wasapi,
    #[serde(alias = "PulseAudio")]
    Pulse,
    PipeWire,
    CoreAudio,
    Alsa,
    Unknown,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub enum AudioDirection {
    Input,
    Output,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub struct AudioDeviceId {
    pub backend: AudioBackend,
    pub direction: AudioDirection,
    pub id: String,
}

impl AudioDeviceId {
    pub const DEFAULT_ID: &'static str = "default";

    pub fn default_input() -> Self {
        Self {
            backend: AudioBackend::Auto,
            direction: AudioDirection::Input,
            id: Self::DEFAULT_ID.to_string(),
        }
    }

    pub fn default_output() -> Self {
        Self {
            backend: AudioBackend::Auto,
            direction: AudioDirection::Output,
            id: Self::DEFAULT_ID.to_string(),
        }
    }

    pub fn is_default(&self) -> bool {
        self.id == Self::DEFAULT_ID
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioDeviceInfo {
    pub key: AudioDeviceId,
    pub label: String,
    #[serde(default)]
    pub display_label: String,
    pub is_default: bool,
}

pub fn disambiguate_display_labels(devices: &mut [AudioDeviceInfo]) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for device in devices.iter() {
        *counts.entry(device.label.clone()).or_insert(0) += 1;
    }

    for device in devices.iter_mut() {
        let short_id = short_device_id(&device.key.id);
        if counts.get(&device.label).copied().unwrap_or_default() > 1 {
            device.display_label = format!("{} — {}", device.label, short_id);
        } else {
            device.display_label = device.label.clone();
        }
    }
}

fn short_device_id(id: &str) -> String {
    let compact: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>();
    if compact.len() >= 8 {
        compact[compact.len() - 8..].to_string()
    } else if compact.is_empty() {
        "unknown".to_string()
    } else {
        compact
    }
}

fn deserialize_input_device_id<'de, D>(deserializer: D) -> Result<AudioDeviceId, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum LegacyOrNew {
        Legacy(String),
        New(AudioDeviceId),
    }

    match <LegacyOrNew as serde::Deserialize>::deserialize(deserializer)? {
        LegacyOrNew::Legacy(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed == "(system default)" {
                Ok(AudioDeviceId::default_input())
            } else {
                Ok(AudioDeviceId {
                    backend: AudioBackend::Unknown,
                    direction: AudioDirection::Input,
                    id: trimmed.to_string(),
                })
            }
        }
        LegacyOrNew::New(id) => Ok(id),
    }
}

fn deserialize_output_device_id<'de, D>(deserializer: D) -> Result<AudioDeviceId, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum LegacyOrNew {
        Legacy(String),
        New(AudioDeviceId),
    }

    match <LegacyOrNew as serde::Deserialize>::deserialize(deserializer)? {
        LegacyOrNew::Legacy(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed == "(system default)" {
                Ok(AudioDeviceId::default_output())
            } else {
                Ok(AudioDeviceId {
                    backend: AudioBackend::Unknown,
                    direction: AudioDirection::Output,
                    id: trimmed.to_string(),
                })
            }
        }
        LegacyOrNew::New(id) => Ok(id),
    }
}

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
    SetAwayMessage(String),
    SetServerAddress {
        host: String,
        port: u16,
    },
    SetConnectionStage {
        stage: ConnectionStage,
        detail: String,
    },

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
    ClearPendingAttachments,
    AttachmentUploadError {
        path: String,
        error: String,
    },
    MemberVoiceStateUpdated {
        channel_id: String,
        user_id: String,
        muted: bool,
        deafened: bool,
        self_muted: bool,
        self_deafened: bool,
        streaming: bool,
    },
    SetActiveVoiceRoute(u32),
    VoiceSessionHealth(bool),
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
    MemberAwayMessageUpdated {
        user_id: String,
        away_message: String,
    },

    // Voice
    VadLevel(f32),
    MicTestWaveform(Vec<f32>),
    VoiceActivity {
        user_id: String,
        speaking: bool,
    },
    VoiceMeter {
        user_id: String,
        level: f32,
    },
    StreamDebugUpdate(StreamDebugView),
    StreamFrame(StreamFrameView),

    // Telemetry
    TelemetryUpdate(TelemetryData),

    // Poke
    PokeReceived {
        from_name: String,
        message: String,
    },

    // User profile
    UserProfileLoaded(UserProfileData),

    // Self state
    SetSelfMuted(bool),
    SetSelfDeafened(bool),

    // Audio devices
    SetAudioDevices {
        input_devices: Vec<AudioDeviceInfo>,
        output_devices: Vec<AudioDeviceInfo>,
        playback_modes: Vec<String>,
    },

    // Channel management
    ChannelCreated(ChannelEntry),
    ChannelRenamed(ChannelEntry),
    ChannelDeleted {
        channel_id: String,
    },
    SetLastEventSeq(u64),

    // Loopback
    SetLoopbackActive(bool),
    SetDefaultChannelId(Option<String>),

    // Settings loaded from disk
    SettingsLoaded(Box<AppSettings>),
    PermissionsMembersLoaded {
        members: Vec<MemberPermissionDraft>,
        current_user_max_role: usize,
        can_moderate_members: bool,
    },
    PermissionsRolesLoaded {
        roles: Vec<RoleDraft>,
    },
    PermissionsChannelOverridesLoaded {
        channel_id: String,
        role_overrides: Vec<PermissionOverrideDraft>,
        member_overrides: Vec<PermissionOverrideDraft>,
    },
    PermissionsAuditLoaded {
        rows: Vec<PermissionAuditRow>,
    },
}

// ── Intents from UI to backend ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum UiIntent {
    Quit,
    SendChat {
        text: String,
        attachments: Vec<AttachmentData>,
    },
    JoinChannel {
        channel_id: String,
    },
    LeaveChannel,
    CreateChannel {
        name: String,
        description: String,
        channel_type: u8,
        codec: u8,
        quality: u32,
        user_limit: u32,
        parent_channel_id: Option<String>,
    },
    RenameChannel {
        channel_id: String,
        new_name: String,
    },
    DeleteChannel {
        channel_id: String,
    },
    TogglePtt,
    PttDown,
    PttUp,
    ToggleSelfMute,
    ToggleSelfDeafen,
    Help,
    SetAwayMessage {
        message: String,
    },
    ConnectToServer {
        host: String,
        port: u16,
        nickname: String,
    },
    CancelConnect,

    // Chat
    EditMessage {
        message_id: String,
        new_text: String,
    },
    DeleteMessage {
        message_id: String,
    },
    AddReaction {
        message_id: String,
        emoji: String,
    },
    RemoveReaction {
        message_id: String,
        emoji: String,
    },
    SendTyping,

    // Moderation
    KickUser {
        user_id: String,
        reason: String,
    },
    BanUser {
        user_id: String,
        reason: String,
        duration: u32,
    },
    MuteUser {
        user_id: String,
        muted: bool,
    },
    DeafenUser {
        user_id: String,
        deafened: bool,
    },
    MoveUser {
        user_id: String,
        target_channel_id: String,
    },
    TimeoutUser {
        user_id: String,
        duration: u32,
        reason: String,
    },

    // Poke
    PokeUser {
        user_id: String,
        message: String,
    },

    // Whisper
    SetWhisperTargets {
        targets: Vec<String>,
    },

    // File upload
    UploadFile {
        path: String,
    },
    SetAvatar {
        path: String,
    },

    // Settings: Audio
    SetNoiseSuppression(bool),
    SetAgcEnabled(bool),
    SetAgcTargetDb(f32),
    SetEchoCancellation(bool),
    SetTypingAttenuation(bool),
    SetFecMode(FecMode),
    SetFecStrength(u8),
    SetVadThreshold(f32),
    SetInputDevice(AudioDeviceId),
    SetOutputDevice(AudioDeviceId),
    SetPlaybackMode(String),
    SetInputGain(f32),
    SetOutputGain(f32),
    SetOutputAutoLevel(bool),
    SetMonoExpansion(bool),
    SetComfortNoise(bool),
    SetComfortNoiseLevel(f32),
    SetDuckingEnabled(bool),
    SetDuckingAttenuationDb(i32),
    SetUserOutputGain {
        user_id: String,
        gain: f32,
    },
    SetUserLocalMute {
        user_id: String,
        muted: bool,
    },
    ToggleLoopback,
    StartScreenShare {
        source_id: String,
    },
    StopScreenShare,

    // Settings: Apply all (sent after settings are saved)
    ApplySettings(Box<AppSettings>),

    // Settings: Save to disk
    SaveSettings(Box<AppSettings>),
    PermsOpen,
    PermsSaveRoleEdits {
        role_id: String,
        name: String,
        color: u32,
        position: u32,
        caps: Vec<(String, String)>,
    },
    PermsDeleteRole {
        role_id: String,
    },
    PermsAssignRoles {
        user_id: String,
        role_ids: Vec<String>,
    },
    PermsSetChannelOverride {
        channel_id: String,
        role_id: Option<String>,
        user_id: Option<String>,
        cap: String,
        effect: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct StreamDebugView {
    pub active_stream_tags: Vec<u64>,
    pub video_datagrams_per_sec: u64,
    pub completed_frames_per_sec: u64,
    pub dropped_no_subscription: u64,
    pub dropped_channel_full: u64,
    pub last_frame_size_bytes: usize,
    pub last_frame_seq: u32,
    pub last_frame_ts_ms: u32,
}

#[derive(Debug, Clone, Default)]
pub struct StreamFrameView {
    pub stream_tag: u64,
    pub frame_seq: u32,
    pub ts_ms: u32,
    pub payload: Vec<u8>,
}

// ── Persisted application settings ────────────────────────────────────

/// All user-configurable settings. Persisted to JSON on disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppSettings {
    // ─── Capture ───
    #[serde(
        default = "AudioDeviceId::default_input",
        deserialize_with = "deserialize_input_device_id"
    )]
    pub capture_device: AudioDeviceId,
    pub capture_mode: CaptureMode,
    pub ptt_key: String,
    pub ptt_delay_ms: u32,
    pub vad_threshold: f32,
    pub input_gain: f32,
    pub noise_suppression: bool,
    pub agc_enabled: bool,
    pub agc_target_db: f32,
    pub echo_cancellation: bool,
    pub denoise_attenuation_db: i32,
    pub typing_attenuation: bool,
    pub fec_mode: FecMode,
    pub fec_strength: u8,

    // ─── Playback ───
    #[serde(
        default = "AudioDeviceId::default_output",
        deserialize_with = "deserialize_output_device_id"
    )]
    pub playback_device: AudioDeviceId,
    pub playback_mode: String,
    pub output_gain: f32,
    pub per_user_audio: HashMap<String, PerUserAudioSettings>,
    pub output_auto_level: bool,
    pub mono_expansion: bool,
    pub comfort_noise: bool,
    pub comfort_noise_level: f32,
    pub ducking_enabled: bool,
    pub ducking_attenuation_db: i32,

    // ─── Notifications ───
    pub notify_user_joined: bool,
    pub notify_user_left: bool,
    pub notify_poke: bool,
    pub notify_chat_message: bool,
    pub sound_pack: String,
    pub notification_volume: f32,

    // ─── Chat ───
    pub chat_show_timestamps: bool,
    pub chat_show_join_leave: bool,
    pub chat_max_lines: u32,
    pub chat_font_size: f32,
    pub chat_log_to_file: bool,
    pub chat_log_directory: String,

    // ─── Hotkeys ───
    pub hotkeys: Vec<HotkeyBinding>,

    // ─── Whisper ───
    pub whisper_allow_all: bool,
    pub whisper_allowed_users: Vec<String>,
    pub whisper_notify: bool,

    // ─── Security ───
    pub identity_nickname: String,
    pub last_server_host: String,
    pub last_server_port: u16,
    pub auto_connect: bool,
    pub auto_reconnect: bool,
    pub reconnect_delay_sec: u32,

    // ─── Application ───
    pub start_minimized: bool,
    pub minimize_to_tray: bool,
    pub check_for_updates: bool,
    pub language: String,
    pub theme: String,
    pub ui_scale: f32,

    // ─── Screen Share (modern) ───
    pub screen_share_fps: u32,
    pub screen_share_max_bitrate_kbps: u32,
    pub screen_share_codec: String,
    pub screen_share_capture_audio: bool,

    // ─── Video Call (modern) ───
    pub video_device: String,
    pub video_resolution: String,
    pub video_fps: u32,
    pub video_max_bitrate_kbps: u32,

    // ─── Downloads / File Sharing ───
    pub download_directory: String,
    pub max_download_size_mb: u32,
    pub auto_download_images: bool,
    pub auto_download_files: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            // Capture
            capture_device: AudioDeviceId::default_input(),
            capture_mode: CaptureMode::PushToTalk,
            ptt_key: "Space".into(),
            ptt_delay_ms: 300,
            vad_threshold: 0.5,
            input_gain: 1.0,
            noise_suppression: true,
            agc_enabled: true,
            agc_target_db: -18.0,
            echo_cancellation: false,
            denoise_attenuation_db: -30,
            typing_attenuation: true,
            fec_mode: FecMode::Auto,
            fec_strength: 50,

            // Playback
            playback_device: AudioDeviceId::default_output(),
            playback_mode: "Automatically use best mode".into(),
            output_gain: 1.0,
            per_user_audio: HashMap::new(),
            output_auto_level: false,
            mono_expansion: false,
            comfort_noise: false,
            comfort_noise_level: 0.02,
            ducking_enabled: false,
            ducking_attenuation_db: -20,

            // Notifications
            notify_user_joined: true,
            notify_user_left: true,
            notify_poke: true,
            notify_chat_message: true,
            sound_pack: "Default".into(),
            notification_volume: 0.8,

            // Chat
            chat_show_timestamps: true,
            chat_show_join_leave: true,
            chat_max_lines: 500,
            chat_font_size: 13.0,
            chat_log_to_file: false,
            chat_log_directory: String::new(),

            // Hotkeys
            hotkeys: default_hotkeys(),

            // Whisper
            whisper_allow_all: true,
            whisper_allowed_users: Vec::new(),
            whisper_notify: true,

            // Security
            identity_nickname: String::new(),
            last_server_host: String::new(),
            last_server_port: 4433,
            auto_connect: false,
            auto_reconnect: true,
            reconnect_delay_sec: 5,

            // Application
            start_minimized: false,
            minimize_to_tray: true,
            check_for_updates: true,
            language: "English".into(),
            theme: "Dark".into(),
            ui_scale: 1.0,

            // Screen Share
            screen_share_fps: 30,
            screen_share_max_bitrate_kbps: 3000,
            screen_share_codec: "AV1".into(),
            screen_share_capture_audio: true,

            // Video Call
            video_device: "(system default)".into(),
            video_resolution: "720p".into(),
            video_fps: 30,
            video_max_bitrate_kbps: 1500,

            // Downloads
            download_directory: String::new(),
            max_download_size_mb: 100,
            auto_download_images: true,
            auto_download_files: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FecMode {
    Off,
    Auto,
    On,
}

impl FecMode {
    pub const ALL: [FecMode; 3] = [FecMode::Off, FecMode::Auto, FecMode::On];

    pub fn label(self) -> &'static str {
        match self {
            FecMode::Off => "Off",
            FecMode::Auto => "Auto (recommended)",
            FecMode::On => "On",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CaptureMode {
    PushToTalk,
    VoiceActivation,
    Continuous,
}

impl CaptureMode {
    pub const ALL: [CaptureMode; 3] = [
        CaptureMode::PushToTalk,
        CaptureMode::VoiceActivation,
        CaptureMode::Continuous,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CaptureMode::PushToTalk => "Push-to-Talk",
            CaptureMode::VoiceActivation => "Voice Activation",
            CaptureMode::Continuous => "Continuous Transmission",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HotkeyBinding {
    pub action: HotkeyAction,
    pub key: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PerUserAudioSettings {
    pub gain: f32,
    pub muted: bool,
}

impl Default for PerUserAudioSettings {
    fn default() -> Self {
        Self {
            gain: 1.0,
            muted: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HotkeyAction {
    ToggleMute,
    ToggleDeafen,
    PushToTalk,
    ToggleScreenShare,
    ToggleVideo,
    FocusChat,
    Disconnect,
}

impl HotkeyAction {
    pub fn label(self) -> &'static str {
        match self {
            HotkeyAction::ToggleMute => "Toggle Mute",
            HotkeyAction::ToggleDeafen => "Toggle Deafen",
            HotkeyAction::PushToTalk => "Push-to-Talk",
            HotkeyAction::ToggleScreenShare => "Toggle Screen Share",
            HotkeyAction::ToggleVideo => "Toggle Video",
            HotkeyAction::FocusChat => "Focus Chat Input",
            HotkeyAction::Disconnect => "Disconnect from Server",
        }
    }
}

pub fn default_hotkeys() -> Vec<HotkeyBinding> {
    vec![
        HotkeyBinding {
            action: HotkeyAction::ToggleMute,
            key: "Ctrl+M".into(),
            enabled: true,
        },
        HotkeyBinding {
            action: HotkeyAction::ToggleDeafen,
            key: "Ctrl+D".into(),
            enabled: true,
        },
        HotkeyBinding {
            action: HotkeyAction::PushToTalk,
            key: "Space".into(),
            enabled: true,
        },
        HotkeyBinding {
            action: HotkeyAction::ToggleScreenShare,
            key: "Ctrl+Shift+S".into(),
            enabled: true,
        },
        HotkeyBinding {
            action: HotkeyAction::ToggleVideo,
            key: "Ctrl+Shift+V".into(),
            enabled: true,
        },
        HotkeyBinding {
            action: HotkeyAction::FocusChat,
            key: "Ctrl+T".into(),
            enabled: true,
        },
        HotkeyBinding {
            action: HotkeyAction::Disconnect,
            key: "Ctrl+Q".into(),
            enabled: true,
        },
    ]
}

// ── Settings page enum ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    Application,
    Capture,
    Playback,
    Hotkeys,
    Chat,
    Downloads,
    Notifications,
    Whisper,
    ScreenShare,
    VideoCall,
    Security,
}

impl SettingsPage {
    pub const ALL: [SettingsPage; 11] = [
        SettingsPage::Application,
        SettingsPage::Capture,
        SettingsPage::Playback,
        SettingsPage::Hotkeys,
        SettingsPage::Chat,
        SettingsPage::Downloads,
        SettingsPage::Notifications,
        SettingsPage::Whisper,
        SettingsPage::ScreenShare,
        SettingsPage::VideoCall,
        SettingsPage::Security,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsPage::Application => "Application",
            SettingsPage::Capture => "Capture",
            SettingsPage::Playback => "Playback",
            SettingsPage::Hotkeys => "Hotkeys",
            SettingsPage::Chat => "Chat",
            SettingsPage::Downloads => "Downloads",
            SettingsPage::Notifications => "Notifications",
            SettingsPage::Whisper => "Whisper",
            SettingsPage::ScreenShare => "Screen Share",
            SettingsPage::VideoCall => "Video Call",
            SettingsPage::Security => "Security",
        }
    }
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
    pub description: String,
    pub bitrate_bps: u32,
    pub opus_profile: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelType {
    Text,
    Voice,
    Streaming,
    Category,
}

#[derive(Debug, Clone)]
pub struct MemberEntry {
    pub user_id: String,
    pub display_name: String,
    pub away_message: String,
    pub muted: bool,
    pub deafened: bool,
    pub self_muted: bool,
    pub self_deafened: bool,
    pub streaming: bool,
    pub speaking: bool,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub message_id: String,
    pub channel_id: String,
    pub author_id: String,
    pub author_name: String,
    pub text: String,
    pub timestamp: i64, // unix millis
    pub attachments: Vec<AttachmentData>,
    pub reply_to: Option<String>,
    pub reactions: Vec<ReactionData>,
    pub pinned: bool,
    pub edited: bool,
}

#[derive(Debug, Clone)]
pub struct AttachmentData {
    pub asset_id: String,
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
    pub rx_bitrate_bps: u32,
    pub tx_bitrate_bps: u32,
    pub rx_pps: u32,
    pub tx_pps: u32,
    pub jitter_buffer_depth: u32,
    pub late_packets: u32,
    pub lost_packets: u32,
    pub concealment_frames: u32,
    pub peak_stream_level: f32,
    pub send_queue_drop_count: u32,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareSourceKind {
    Screen,
    Window,
}

#[derive(Debug, Clone)]
pub struct ShareSourceOption {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub kind: ShareSourceKind,
}

fn fallback_share_sources() -> Vec<ShareSourceOption> {
    vec![ShareSourceOption {
        id: "screen-1".into(),
        title: "Screen 1".into(),
        subtitle: "Primary display".into(),
        kind: ShareSourceKind::Screen,
    }]
}

#[cfg(target_os = "windows")]
pub fn enumerate_share_sources() -> Vec<ShareSourceOption> {
    use std::cmp::Ordering;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
    };

    unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let windows = &mut *(lparam.0 as *mut Vec<(String, String)>);
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return BOOL(1);
        }

        let title_len = unsafe { GetWindowTextLengthW(hwnd) };
        if title_len <= 0 {
            return BOOL(1);
        }

        let mut title_buf = vec![0u16; title_len as usize + 1];
        let copied = unsafe { GetWindowTextW(hwnd, &mut title_buf) };
        if copied <= 0 {
            return BOOL(1);
        }

        let title = String::from_utf16_lossy(&title_buf[..copied as usize])
            .trim()
            .to_string();
        if title.is_empty() {
            return BOOL(1);
        }

        windows.push((format!("window-hwnd-{}", hwnd.0 as isize), title));
        BOOL(1)
    }

    unsafe extern "system" fn enum_monitors_callback(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let screens = &mut *(lparam.0 as *mut Vec<ShareSourceOption>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

        if unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo as *mut _ as *mut _) }.as_bool()
        {
            let name_len = info
                .szDevice
                .iter()
                .position(|&ch| ch == 0)
                .unwrap_or(info.szDevice.len());
            let monitor_name = String::from_utf16_lossy(&info.szDevice[..name_len]);
            let id = SCREEN_COUNTER.fetch_add(1, AtomicOrdering::Relaxed) + 1;
            screens.push(ShareSourceOption {
                id: format!("screen-{id}"),
                title: format!("Screen {id}"),
                subtitle: monitor_name,
                kind: ShareSourceKind::Screen,
            });
        }

        BOOL(1)
    }

    static SCREEN_COUNTER: AtomicUsize = AtomicUsize::new(0);
    SCREEN_COUNTER.store(0, AtomicOrdering::Relaxed);

    let mut screens: Vec<ShareSourceOption> = Vec::new();
    let mut windows_found: Vec<(String, String)> = Vec::new();

    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitors_callback),
            LPARAM(&mut screens as *mut _ as isize),
        )
    };

    let _ = unsafe {
        EnumWindows(
            Some(enum_windows_callback),
            LPARAM(&mut windows_found as *mut _ as isize),
        )
    };

    windows_found.sort_by(|a, b| {
        if a.1.eq_ignore_ascii_case(&b.1) {
            a.0.cmp(&b.0)
        } else if a.1.to_ascii_lowercase() < b.1.to_ascii_lowercase() {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    });

    let mut sources = screens;
    sources.extend(
        windows_found
            .into_iter()
            .map(|(id, title)| ShareSourceOption {
                id,
                title,
                subtitle: "Application window".into(),
                kind: ShareSourceKind::Window,
            }),
    );

    if sources.is_empty() {
        fallback_share_sources()
    } else {
        sources
    }
}

#[cfg(not(target_os = "windows"))]
pub fn enumerate_share_sources() -> Vec<ShareSourceOption> {
    fallback_share_sources()
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

    // Members (keyed by channel_id, with each channel list deduped by user_id)
    pub members: HashMap<String, Vec<MemberEntry>>,
    pub speaking_users: HashMap<String, bool>,
    pub voice_levels: HashMap<String, f32>,

    // Chat (keyed by channel_id)
    pub messages: HashMap<String, VecDeque<ChatMessage>>,
    pub chat_input: String,
    pub chat_input_focused: bool,
    pub pending_attachments: Vec<PendingAttachment>,
    pub typing_users: HashMap<String, Vec<(String, std::time::Instant)>>,

    // Per-channel drafts (text + attachments preserved on channel switch)
    pub drafts: HashMap<String, DraftState>,

    // Drag-and-drop overlay state
    pub drag_hovering: bool,
    pub drag_overlay_until: Option<std::time::Instant>,

    // Voice (runtime state, not persisted)
    pub ptt_enabled: bool,
    pub ptt_active: bool,
    pub self_muted: bool,
    pub self_deafened: bool,
    pub vad_level: Option<f32>,
    pub active_voice_channel_route: u32,
    pub voice_session_healthy: bool,
    pub connection_established_at: Option<std::time::Instant>,
    pub member_first_seen_at: HashMap<String, std::time::Instant>,
    pub member_last_active_at: HashMap<String, std::time::Instant>,

    // Log
    pub log: VecDeque<String>,

    // Telemetry
    pub telemetry: TelemetryData,

    // UI toggles
    pub show_settings: bool,
    pub show_telemetry: bool,
    pub show_connections: bool,
    pub show_member_connection_info: bool,
    pub connection_info_target_user_id: String,
    pub connection_info_target_display_name: String,
    pub status_line: String,
    pub connection_host_draft: String,
    pub connection_port_draft: String,
    pub connection_nickname_draft: String,
    pub connection_error: String,
    pub connection_stage: ConnectionStage,
    pub connection_details: VecDeque<String>,

    // Audio devices (enumerated at runtime)
    pub input_devices: Vec<AudioDeviceInfo>,
    pub output_devices: Vec<AudioDeviceInfo>,
    pub playback_modes: Vec<String>,

    // Mic test loopback (runtime)
    pub loopback_active: bool,
    pub mic_test_waveform: Vec<f32>,

    // Create channel dialog
    pub show_create_channel: bool,
    pub create_channel_name: String,
    pub create_channel_description: String,
    pub create_channel_type: usize,
    pub create_channel_codec: usize,
    pub create_channel_quality: u32,
    pub create_channel_user_limit: u32,
    pub create_channel_tab: usize,
    pub create_channel_parent_id: Option<String>,
    pub rename_channel_target_id: Option<String>,
    pub rename_channel_name: String,
    pub show_rename_channel: bool,
    pub delete_channel_target_id: Option<String>,
    pub show_delete_channel_confirm: bool,
    pub show_channel_info: bool,
    pub channel_info_target_id: Option<String>,
    pub channel_collapsed: HashMap<String, bool>,
    pub default_channel_id: Option<String>,
    pub last_event_seq: u64,

    // User popup
    pub show_user_popup: bool,
    pub show_away_message_dialog: bool,
    pub show_set_avatar_dialog: bool,
    pub show_share_content_dialog: bool,
    pub share_include_audio: bool,
    pub share_presenter_mode: usize,
    pub share_sources: Vec<ShareSourceOption>,
    pub selected_share_source: Option<String>,
    pub sharing_active: bool,
    pub start_share_in_flight: bool,
    pub stream_debug: StreamDebugView,
    pub latest_stream_frame: Option<StreamFrameView>,
    pub avatar_path_draft: String,
    pub show_poke_dialog: bool,
    pub poke_target_user_id: String,
    pub poke_target_display_name: String,
    pub poke_message_draft: String,
    pub avatar_url: Option<String>,
    pub away_message: String,
    pub away_message_draft: String,
    pub away_message_presets: Vec<String>,

    // Notifications
    pub notifications: VecDeque<Notification>,

    // ── Settings system ──
    pub settings: AppSettings,
    pub settings_draft: AppSettings,
    pub settings_page: SettingsPage,
    pub settings_dirty: bool,

    // Permissions Center
    pub show_permissions_center: bool,
    pub permissions_tab: PermissionsTab,
    pub permissions_selected_role: usize,
    pub permissions_search: String,
    pub permissions_current_user_max_role: usize,
    pub permissions_channel_scope_name: String,
    pub permissions_can_moderate_members: bool,
    pub permissions_roles: Vec<RoleDraft>,
    pub permissions_selected_channel_id: Option<String>,
    pub permissions_private_channel: bool,
    pub permissions_override_tab: PermissionOverrideTab,
    pub permissions_role_overrides: Vec<PermissionOverrideDraft>,
    pub permissions_member_overrides: Vec<PermissionOverrideDraft>,
    pub permissions_view_as_mode: PermissionViewAsMode,
    pub permissions_view_as_name: String,
    pub permissions_member_search: String,
    pub permissions_members: Vec<MemberPermissionDraft>,
    pub permissions_audit_rows: Vec<PermissionAuditRow>,
    pub permissions_selected_member: usize,
    pub permissions_advanced_enabled: bool,
    pub permissions_actor_power: PermissionPowerDraft,
    pub permissions_target_needed_power: PermissionPowerDraft,
    pub permissions_actor_preview: usize,
    pub permissions_target_preview: usize,
}

#[derive(Debug, Clone)]
pub struct PendingAttachment {
    pub path: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub error: Option<String>,
}

/// Per-channel draft state preserving unsent text and attachments across channel switches.
#[derive(Debug, Clone, Default)]
pub struct DraftState {
    pub text: String,
    pub attachments: Vec<PendingAttachment>,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub text: String,
    pub created: std::time::Instant,
    pub kind: NotificationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionStage {
    #[default]
    Idle,
    Resolving,
    Handshaking,
    Authenticating,
    Syncing,
    Connected,
    Failed,
}

impl ConnectionStage {
    pub fn is_in_progress(self) -> bool {
        matches!(
            self,
            ConnectionStage::Resolving
                | ConnectionStage::Handshaking
                | ConnectionStage::Authenticating
                | ConnectionStage::Syncing
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            ConnectionStage::Idle => "Idle",
            ConnectionStage::Resolving => "Resolving host",
            ConnectionStage::Handshaking => "Establishing QUIC/TLS",
            ConnectionStage::Authenticating => "Authenticating",
            ConnectionStage::Syncing => "Syncing initial state",
            ConnectionStage::Connected => "Connected",
            ConnectionStage::Failed => "Failed",
        }
    }
}

#[derive(Debug, Clone)]
pub enum NotificationKind {
    Info,
    Poke,
    Mention,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionsTab {
    Roles,
    Channels,
    Members,
    AuditLog,
    Advanced,
}

impl PermissionsTab {
    pub const ALL: [PermissionsTab; 5] = [
        PermissionsTab::Roles,
        PermissionsTab::Channels,
        PermissionsTab::Members,
        PermissionsTab::AuditLog,
        PermissionsTab::Advanced,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PermissionsTab::Roles => "Roles",
            PermissionsTab::Channels => "Channels",
            PermissionsTab::Members => "Members",
            PermissionsTab::AuditLog => "Audit Log",
            PermissionsTab::Advanced => "Advanced",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoleDraft {
    pub role_id: String,
    pub name: String,
    pub color_hex: String,
    pub member_count: u32,
    pub hoist: bool,
    pub mentionable: bool,
    pub protected: bool,
    pub administrative: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOverrideTab {
    Roles,
    Members,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionViewAsMode {
    Role,
    Member,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionValue {
    Inherit,
    Allow,
    Deny,
}

impl PermissionValue {
    pub fn cycle(self) -> Self {
        match self {
            PermissionValue::Inherit => PermissionValue::Allow,
            PermissionValue::Allow => PermissionValue::Deny,
            PermissionValue::Deny => PermissionValue::Inherit,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PermissionOverrideDraft {
    pub role_id: Option<String>,
    pub user_id: Option<String>,
    pub subject_name: String,
    pub capabilities: Vec<PermissionValue>,
}

#[derive(Debug, Clone)]
pub struct PermissionPowerDraft {
    pub mute_power: i32,
    pub move_power: i32,
    pub kick_power: i32,
    pub manage_roles_power: i32,
}

#[derive(Debug, Clone)]
pub struct MemberPermissionDraft {
    pub display_name: String,
    pub user_id: String,
    pub highest_role_index: usize,
    pub role_assignments: Vec<bool>,
    pub role_ids: Vec<String>,
    pub can_mute_members: bool,
    pub can_deafen_members: bool,
    pub can_move_members: bool,
    pub can_kick_members: bool,
}

#[derive(Debug, Clone)]
pub struct PermissionAuditRow {
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub created_at_unix_millis: Option<i64>,
}

impl Default for UiModel {
    fn default() -> Self {
        let settings = AppSettings::default();
        let settings_draft = settings.clone();
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
            voice_levels: HashMap::new(),
            messages: HashMap::new(),
            chat_input: String::new(),
            chat_input_focused: false,
            pending_attachments: Vec::new(),
            typing_users: HashMap::new(),
            drafts: HashMap::new(),
            drag_hovering: false,
            drag_overlay_until: None,
            ptt_enabled: true,
            ptt_active: false,
            self_muted: false,
            self_deafened: false,
            vad_level: None,
            active_voice_channel_route: 0,
            voice_session_healthy: false,
            connection_established_at: None,
            member_first_seen_at: HashMap::new(),
            member_last_active_at: HashMap::new(),
            log: VecDeque::new(),
            telemetry: TelemetryData::default(),
            show_settings: false,
            show_telemetry: false,
            show_connections: false,
            show_member_connection_info: false,
            connection_info_target_user_id: String::new(),
            connection_info_target_display_name: String::new(),
            status_line: String::new(),
            connection_host_draft: "127.0.0.1".into(),
            connection_port_draft: "4433".into(),
            connection_nickname_draft: String::new(),
            connection_error: String::new(),
            connection_stage: ConnectionStage::Idle,
            connection_details: VecDeque::new(),
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            playback_modes: Vec::new(),
            loopback_active: false,
            mic_test_waveform: Vec::new(),
            show_create_channel: false,
            create_channel_name: String::new(),
            create_channel_description: String::new(),
            create_channel_type: 0,
            create_channel_codec: 0,
            create_channel_quality: 64,
            create_channel_user_limit: 0,
            create_channel_tab: 0,
            create_channel_parent_id: None,
            rename_channel_target_id: None,
            rename_channel_name: String::new(),
            show_rename_channel: false,
            delete_channel_target_id: None,
            show_delete_channel_confirm: false,
            show_channel_info: false,
            channel_info_target_id: None,
            channel_collapsed: HashMap::new(),
            default_channel_id: None,
            last_event_seq: 0,
            show_user_popup: false,
            show_away_message_dialog: false,
            show_set_avatar_dialog: false,
            show_share_content_dialog: false,
            share_include_audio: true,
            share_presenter_mode: 0,
            share_sources: enumerate_share_sources(),
            selected_share_source: None,
            sharing_active: false,
            start_share_in_flight: false,
            stream_debug: StreamDebugView::default(),
            latest_stream_frame: None,
            avatar_path_draft: String::new(),
            show_poke_dialog: false,
            poke_target_user_id: String::new(),
            poke_target_display_name: String::new(),
            poke_message_draft: "Poke".into(),
            avatar_url: None,
            away_message: String::new(),
            away_message_draft: String::new(),
            away_message_presets: vec![
                "Out to lunch".into(),
                "Back in 5".into(),
                "In a meeting".into(),
            ],
            notifications: VecDeque::new(),
            settings,
            settings_draft,
            settings_page: SettingsPage::Capture,
            settings_dirty: false,
            show_permissions_center: false,
            permissions_tab: PermissionsTab::Roles,
            permissions_selected_role: 0,
            permissions_search: String::new(),
            permissions_current_user_max_role: 0,
            permissions_channel_scope_name: "General".into(),
            permissions_can_moderate_members: false,
            permissions_roles: vec![],
            permissions_selected_channel_id: None,
            permissions_private_channel: false,
            permissions_override_tab: PermissionOverrideTab::Roles,
            permissions_role_overrides: vec![],
            permissions_member_overrides: vec![],
            permissions_view_as_mode: PermissionViewAsMode::Role,
            permissions_view_as_name: "@everyone".into(),
            permissions_member_search: String::new(),
            permissions_members: vec![],
            permissions_audit_rows: vec![],
            permissions_selected_member: 0,
            permissions_advanced_enabled: false,
            permissions_actor_power: PermissionPowerDraft {
                mute_power: 75,
                move_power: 75,
                kick_power: 100,
                manage_roles_power: 50,
            },
            permissions_target_needed_power: PermissionPowerDraft {
                mute_power: 50,
                move_power: 50,
                kick_power: 75,
                manage_roles_power: 75,
            },
            permissions_actor_preview: 0,
            permissions_target_preview: 1,
        }
    }
}

impl UiModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear the current channel's draft after a successful send.
    pub fn clear_current_draft(&mut self) {
        if let Some(ref ch) = self.selected_channel {
            self.drafts.remove(ch);
        }
    }

    pub fn can_start_screen_share(&self) -> bool {
        !self.start_share_in_flight && !self.sharing_active
    }

    pub fn apply_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::SetConnected(c) => {
                self.connected = c;
                self.connection_established_at = c.then(std::time::Instant::now);
            }
            UiEvent::SetAuthed(a) => self.authed = a,
            UiEvent::SetChannelName(n) => {
                // Save current channel's draft before switching
                if let Some(ref old_ch) = self.selected_channel {
                    if !self.chat_input.is_empty() || !self.pending_attachments.is_empty() {
                        self.drafts.insert(
                            old_ch.clone(),
                            DraftState {
                                text: std::mem::take(&mut self.chat_input),
                                attachments: std::mem::take(&mut self.pending_attachments),
                            },
                        );
                    } else {
                        self.drafts.remove(old_ch);
                    }
                }
                // Load new channel's draft
                if let Some(draft) = self.drafts.remove(&n) {
                    self.chat_input = draft.text;
                    self.pending_attachments = draft.attachments;
                } else {
                    self.chat_input.clear();
                    self.pending_attachments.clear();
                }
                self.selected_channel = Some(n.clone());
                self.selected_channel_name =
                    self.channel_name_for_id(&n).map(str::to_owned).unwrap_or(n);
            }
            UiEvent::SetNick(n) => {
                self.nick = n.clone();
                if !n.trim().is_empty() {
                    self.connection_nickname_draft = n;
                }
            }
            UiEvent::SetUserId(id) => self.user_id = id,
            UiEvent::AppendLog(line) => {
                self.log.push_back(line);
                if self.log.len() > MAX_LOG_LINES {
                    self.log.pop_front();
                }
            }
            UiEvent::SetStatus(s) => self.status_line = s,
            UiEvent::StreamDebugUpdate(snapshot) => {
                self.stream_debug = snapshot;
            }
            UiEvent::StreamFrame(frame) => {
                self.latest_stream_frame = Some(frame);
            }
            UiEvent::SetAwayMessage(message) => {
                self.away_message = message.clone();
                for members in self.members.values_mut() {
                    if let Some(member) = members.iter_mut().find(|m| m.user_id == self.user_id) {
                        member.away_message = message.clone();
                    }
                }
            }
            UiEvent::SetServerAddress { host, port } => {
                self.connection_host_draft = host;
                self.connection_port_draft = port.to_string();
                self.connection_error.clear();
            }
            UiEvent::SetConnectionStage { stage, detail } => {
                self.connection_stage = stage;
                self.status_line = format!("Connection: {}", stage.label());
                if stage == ConnectionStage::Failed {
                    self.connection_error = detail.clone();
                } else if stage != ConnectionStage::Idle {
                    self.connection_error.clear();
                }
                self.connection_details.push_back(detail);
                if self.connection_details.len() > 64 {
                    self.connection_details.pop_front();
                }
            }
            UiEvent::SetChannels(chs) => {
                self.channels = chs;
                let live_ids: HashSet<_> = self.channels.iter().map(|c| c.id.clone()).collect();
                self.channel_collapsed
                    .retain(|channel_id, _| live_ids.contains(channel_id));
                for channel_id in live_ids {
                    self.channel_collapsed.entry(channel_id).or_insert(false);
                }
                self.refresh_selected_channel_name();
            }
            UiEvent::UpdateChannelMembers {
                channel_id,
                members,
            } => {
                let now = std::time::Instant::now();
                for member in &members {
                    self.member_first_seen_at
                        .entry(member.user_id.clone())
                        .or_insert(now);
                }
                self.members.insert(channel_id, members);
            }
            UiEvent::MessageReceived(mut msg) => {
                msg.author_name = self.resolve_message_author_name(
                    &msg.channel_id,
                    &msg.author_id,
                    &msg.author_name,
                );
                let local_user_id = self.user_id.clone();
                let ch = msg.channel_id.clone();
                let msgs = self.messages.entry(ch).or_default();

                if !msg.message_id.trim().is_empty()
                    && msgs
                        .iter()
                        .any(|existing| existing.message_id == msg.message_id)
                {
                    debug!(
                        message_id = %msg.message_id,
                        author_user_id = %msg.author_id,
                        channel_id = %msg.channel_id,
                        "chat dedupe hit (existing canonical message_id)"
                    );
                    return;
                }

                if !msg.message_id.starts_with("local-") {
                    if let Some(local_idx) =
                        Self::find_matching_optimistic_index(msgs, &msg, &local_user_id)
                    {
                        debug!(
                            message_id = %msg.message_id,
                            author_user_id = %msg.author_id,
                            channel_id = %msg.channel_id,
                            "chat reconcile optimistic local echo with canonical message"
                        );
                        msgs[local_idx] = msg;
                        return;
                    }
                }

                debug!(
                    message_id = %msg.message_id,
                    author_user_id = %msg.author_id,
                    channel_id = %msg.channel_id,
                    "chat dedupe miss (appending message)"
                );
                msgs.push_back(msg);
                if msgs.len() > MAX_MESSAGES_PER_CHANNEL {
                    msgs.pop_front();
                }
            }
            UiEvent::MessageEdited {
                channel_id,
                message_id,
                new_text,
            } => {
                if let Some(msgs) = self.messages.get_mut(&channel_id) {
                    if let Some(msg) = msgs.iter_mut().find(|m| m.message_id == message_id) {
                        msg.text = new_text;
                        msg.edited = true;
                    }
                }
            }
            UiEvent::MessageDeleted {
                channel_id,
                message_id,
            } => {
                if let Some(msgs) = self.messages.get_mut(&channel_id) {
                    msgs.retain(|m| m.message_id != message_id);
                }
            }
            UiEvent::ClearPendingAttachments => {
                self.pending_attachments.clear();
                if let Some(ref ch) = self.selected_channel {
                    if let Some(draft) = self.drafts.get_mut(ch) {
                        draft.attachments.clear();
                    }
                }
            }
            UiEvent::AttachmentUploadError { path, error } => {
                if let Some(att) = self.pending_attachments.iter_mut().find(|a| a.path == path) {
                    att.error = Some(error.clone());
                }
                self.notifications.push_back(Notification {
                    text: format!("Upload failed: {error}"),
                    created: std::time::Instant::now(),
                    kind: NotificationKind::Error,
                });
            }
            UiEvent::TypingIndicator {
                channel_id,
                user_name,
            } => {
                let typers = self.typing_users.entry(channel_id).or_default();
                typers.retain(|(name, _)| name != &user_name);
                typers.push((user_name, std::time::Instant::now()));
            }
            UiEvent::MemberJoined { channel_id, member } => {
                let now = std::time::Instant::now();
                self.member_first_seen_at
                    .entry(member.user_id.clone())
                    .or_insert(now);
                let members = self.members.entry(channel_id).or_default();
                if let Some(existing) = members.iter_mut().find(|m| m.user_id == member.user_id) {
                    *existing = member;
                } else {
                    members.push(member);
                }
            }
            UiEvent::MemberLeft {
                channel_id,
                user_id,
            } => {
                if let Some(members) = self.members.get_mut(&channel_id) {
                    members.retain(|m| m.user_id != user_id);
                }
                if user_id == self.user_id
                    && self
                        .selected_channel
                        .as_ref()
                        .is_some_and(|c| c == &channel_id)
                {
                    self.selected_channel = self
                        .default_channel_id
                        .clone()
                        .filter(|candidate| self.channels.iter().any(|ch| ch.id == *candidate))
                        .or_else(|| self.channels.first().map(|ch| ch.id.clone()));
                    self.refresh_selected_channel_name();
                }
            }
            UiEvent::MemberAwayMessageUpdated {
                user_id,
                away_message,
            } => {
                for members in self.members.values_mut() {
                    if let Some(member) = members.iter_mut().find(|m| m.user_id == user_id) {
                        member.away_message = away_message.clone();
                    }
                }
            }
            UiEvent::MemberVoiceStateUpdated {
                channel_id,
                user_id,
                muted,
                deafened,
                self_muted,
                self_deafened,
                streaming,
            } => {
                if let Some(members) = self.members.get_mut(&channel_id) {
                    if let Some(member) = members.iter_mut().find(|m| m.user_id == user_id) {
                        member.muted = muted;
                        member.deafened = deafened;
                        member.self_muted = self_muted;
                        member.self_deafened = self_deafened;
                        member.streaming = streaming;
                    }
                }
            }
            UiEvent::SetActiveVoiceRoute(route) => self.active_voice_channel_route = route,
            UiEvent::VoiceSessionHealth(healthy) => self.voice_session_healthy = healthy,
            UiEvent::VadLevel(v) => self.vad_level = Some(v),
            UiEvent::MicTestWaveform(samples) => self.mic_test_waveform = samples,
            UiEvent::VoiceActivity { user_id, speaking } => {
                if speaking {
                    self.member_last_active_at
                        .insert(user_id.clone(), std::time::Instant::now());
                }
                self.speaking_users.insert(user_id, speaking);
            }
            UiEvent::VoiceMeter { user_id, level } => {
                self.voice_levels.insert(user_id, level.clamp(0.0, 1.0));
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
            UiEvent::UserProfileLoaded(_profile) => {}
            UiEvent::SetSelfMuted(m) => self.self_muted = m,
            UiEvent::SetSelfDeafened(d) => self.self_deafened = d,
            UiEvent::SetAudioDevices {
                input_devices,
                output_devices,
                playback_modes,
            } => {
                self.input_devices = input_devices;
                self.output_devices = output_devices;
                self.playback_modes = playback_modes;
            }
            UiEvent::ChannelCreated(entry) => {
                if let Some(existing) = self.channels.iter_mut().find(|ch| ch.id == entry.id) {
                    *existing = entry;
                } else {
                    self.channel_collapsed
                        .entry(entry.id.clone())
                        .or_insert(false);
                    self.channels.push(entry);
                }
                self.refresh_selected_channel_name();
            }
            UiEvent::ChannelRenamed(entry) => {
                if let Some(existing) = self.channels.iter_mut().find(|ch| ch.id == entry.id) {
                    *existing = entry;
                } else {
                    self.channel_collapsed
                        .entry(entry.id.clone())
                        .or_insert(false);
                    self.channels.push(entry);
                }
                self.refresh_selected_channel_name();
            }
            UiEvent::ChannelDeleted { channel_id } => {
                let mut removed = HashSet::new();
                removed.insert(channel_id.clone());
                loop {
                    let mut changed = false;
                    for channel in &self.channels {
                        if let Some(parent_id) = &channel.parent_id {
                            if removed.contains(parent_id) && removed.insert(channel.id.clone()) {
                                changed = true;
                            }
                        }
                    }
                    if !changed {
                        break;
                    }
                }

                self.channels.retain(|ch| !removed.contains(&ch.id));
                for removed_id in &removed {
                    self.members.remove(removed_id);
                    self.messages.remove(removed_id);
                    self.typing_users.remove(removed_id);
                    self.channel_collapsed.remove(removed_id);
                }

                if self
                    .selected_channel
                    .as_ref()
                    .is_some_and(|selected| removed.contains(selected))
                {
                    self.selected_channel = self
                        .default_channel_id
                        .clone()
                        .filter(|candidate| self.channels.iter().any(|ch| ch.id == *candidate))
                        .or_else(|| self.channels.first().map(|ch| ch.id.clone()));
                    self.refresh_selected_channel_name();
                }
            }
            UiEvent::SetLastEventSeq(seq) => {
                self.last_event_seq = seq;
            }
            UiEvent::SetLoopbackActive(active) => {
                self.loopback_active = active;
                if !active {
                    self.mic_test_waveform.clear();
                }
            }
            UiEvent::SetDefaultChannelId(channel_id) => {
                self.default_channel_id = channel_id;
            }
            UiEvent::SettingsLoaded(s) => {
                self.settings = *s.clone();
                self.settings_draft = *s;
                self.sync_settings_to_runtime();
            }
            UiEvent::PermissionsMembersLoaded {
                members,
                current_user_max_role,
                can_moderate_members,
            } => {
                self.permissions_members = members;
                self.permissions_current_user_max_role = current_user_max_role;
                self.permissions_can_moderate_members = can_moderate_members;
                self.permissions_selected_member = 0;
            }
            UiEvent::PermissionsRolesLoaded { roles } => {
                self.permissions_roles = roles;
                self.permissions_selected_role = 0;
            }
            UiEvent::PermissionsChannelOverridesLoaded {
                channel_id,
                role_overrides,
                member_overrides,
            } => {
                self.permissions_selected_channel_id = Some(channel_id);
                self.permissions_role_overrides = role_overrides;
                self.permissions_member_overrides = member_overrides;
            }
            UiEvent::PermissionsAuditLoaded { rows } => {
                self.permissions_audit_rows = rows;
            }
        }

        // Expire old typing indicators (>5s)
        let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(5);
        for typers in self.typing_users.values_mut() {
            typers.retain(|(_, t)| *t > cutoff);
        }

        // Expire old notifications (>5s)
        let notif_cutoff = std::time::Instant::now() - std::time::Duration::from_secs(5);
        while self
            .notifications
            .front()
            .is_some_and(|n| n.created < notif_cutoff)
        {
            self.notifications.pop_front();
        }
    }

    fn channel_name_for_id(&self, channel_id: &str) -> Option<&str> {
        self.channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .map(|channel| channel.name.as_str())
    }

    fn refresh_selected_channel_name(&mut self) {
        if let Some(selected_channel_id) = self.selected_channel.clone() {
            self.selected_channel_name = self
                .channel_name_for_id(&selected_channel_id)
                .map(str::to_owned)
                .unwrap_or(selected_channel_id);
        }
    }

    fn find_matching_optimistic_index(
        messages: &VecDeque<ChatMessage>,
        incoming: &ChatMessage,
        local_user_id: &str,
    ) -> Option<usize> {
        const OPTIMISTIC_RECONCILE_WINDOW_MS: i64 = 30_000;

        if local_user_id.trim().is_empty()
            || incoming.author_id.trim().is_empty()
            || incoming.author_id != local_user_id
        {
            return None;
        }

        messages.iter().position(|existing| {
            existing.message_id.starts_with("local-")
                && existing.channel_id == incoming.channel_id
                && existing.author_id == incoming.author_id
                && existing.text == incoming.text
                && (incoming.timestamp - existing.timestamp).abs() <= OPTIMISTIC_RECONCILE_WINDOW_MS
        })
    }

    fn resolve_message_author_name(
        &self,
        channel_id: &str,
        author_id: &str,
        fallback_author_name: &str,
    ) -> String {
        if !author_id.trim().is_empty() {
            if let Some(display_name) = self.members.get(channel_id).and_then(|members| {
                members
                    .iter()
                    .find(|member| {
                        member.user_id == author_id && !member.display_name.trim().is_empty()
                    })
                    .map(|member| member.display_name.trim())
            }) {
                return display_name.to_string();
            }

            if !self.user_id.is_empty() && self.user_id == author_id && !self.nick.trim().is_empty()
            {
                return self.nick.trim().to_string();
            }
        }

        if !fallback_author_name.trim().is_empty() {
            return fallback_author_name.trim().to_string();
        }

        if !self.nick.trim().is_empty() {
            return self.nick.trim().to_string();
        }

        "User".to_string()
    }

    /// Sync persisted settings into runtime model state.
    pub fn sync_settings_to_runtime(&mut self) {
        self.ptt_enabled = self.settings.capture_mode == CaptureMode::PushToTalk;

        let nick = self.settings.identity_nickname.trim();
        if !nick.is_empty() {
            self.nick = nick.to_string();
            self.connection_nickname_draft = nick.to_string();
        }

        let host = self.settings.last_server_host.trim();
        if !host.is_empty() {
            self.connection_host_draft = host.to_string();
        }

        self.connection_port_draft = self.settings.last_server_port.to_string();
    }

    pub fn user_output_gain(&self, user_id: &str) -> f32 {
        self.settings
            .per_user_audio
            .get(user_id)
            .map(|s| s.gain.clamp(0.0, 2.0))
            .unwrap_or(1.0)
    }

    pub fn user_locally_muted(&self, user_id: &str) -> bool {
        self.settings
            .per_user_audio
            .get(user_id)
            .map(|s| s.muted)
            .unwrap_or(false)
    }

    pub fn current_channel_type(&self) -> Option<ChannelType> {
        self.selected_channel.as_ref().and_then(|selected| {
            self.channels
                .iter()
                .find(|channel| &channel.id == selected)
                .map(|channel| channel.channel_type)
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_start_screen_share_is_debounced() {
        let mut model = UiModel::default();
        assert!(model.can_start_screen_share());
        model.start_share_in_flight = true;
        assert!(!model.can_start_screen_share());
        model.start_share_in_flight = false;
        model.sharing_active = true;
        assert!(!model.can_start_screen_share());
    }

    #[test]
    fn sync_settings_updates_nick_and_connection_nickname() {
        let mut model = UiModel::new();
        model.settings.identity_nickname = "Overdose".into();
        model.settings.last_server_host = "192.168.1.120".into();
        model.settings.last_server_port = 6000;

        model.sync_settings_to_runtime();

        assert_eq!(model.nick, "Overdose");
        assert_eq!(model.connection_nickname_draft, "Overdose");
        assert_eq!(model.connection_host_draft, "192.168.1.120");
        assert_eq!(model.connection_port_draft, "6000");
    }

    #[test]
    fn resolves_author_name_from_channel_member_then_fallback() {
        let mut model = UiModel::new();
        model.nick = "LocalNick".into();
        model.user_id = "local-user".into();
        model.members.insert(
            "lounge-1".into(),
            vec![MemberEntry {
                user_id: "user-1".into(),
                display_name: "Overdose".into(),
                away_message: String::new(),
                muted: false,
                deafened: false,
                self_muted: false,
                self_deafened: false,
                streaming: false,
                speaking: false,
                avatar_url: None,
            }],
        );

        model.apply_event(UiEvent::MessageReceived(ChatMessage {
            message_id: "m1".into(),
            channel_id: "lounge-1".into(),
            author_id: "user-1".into(),
            author_name: "user-1".into(),
            text: "hello".into(),
            timestamp: 1_710_000_000_000,
            attachments: vec![],
            reply_to: None,
            reactions: vec![],
            pinned: false,
            edited: false,
        }));

        let name = model
            .messages
            .get("lounge-1")
            .and_then(|msgs| msgs.back())
            .map(|m| m.author_name.clone())
            .unwrap();
        assert_eq!(name, "Overdose");

        model.apply_event(UiEvent::MessageReceived(ChatMessage {
            message_id: "m2".into(),
            channel_id: "lounge-1".into(),
            author_id: "unknown".into(),
            author_name: "".into(),
            text: "fallback".into(),
            timestamp: 1_710_000_000_100,
            attachments: vec![],
            reply_to: None,
            reactions: vec![],
            pinned: false,
            edited: false,
        }));

        let fallback = model
            .messages
            .get("lounge-1")
            .and_then(|msgs| msgs.back())
            .map(|m| m.author_name.clone())
            .unwrap();
        assert_eq!(fallback, "LocalNick");
    }
    #[test]
    fn member_joined_distinct_user_ids_are_kept_as_two_members() {
        let mut model = UiModel::new();
        model.apply_event(UiEvent::MemberJoined {
            channel_id: "c1".into(),
            member: MemberEntry {
                user_id: "u-overdose".into(),
                display_name: "Overdose".into(),
                away_message: String::new(),
                muted: false,
                deafened: false,
                self_muted: false,
                self_deafened: false,
                streaming: false,
                speaking: false,
                avatar_url: None,
            },
        });
        model.apply_event(UiEvent::MemberJoined {
            channel_id: "c1".into(),
            member: MemberEntry {
                user_id: "u-dresk".into(),
                display_name: "Dresk".into(),
                away_message: String::new(),
                muted: false,
                deafened: false,
                self_muted: false,
                self_deafened: false,
                streaming: false,
                speaking: false,
                avatar_url: None,
            },
        });

        let members = model.members.get("c1").expect("members");
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn set_away_message_updates_local_member_entry() {
        let mut model = UiModel::new();
        model.user_id = "local-user".into();
        model.apply_event(UiEvent::MemberJoined {
            channel_id: "c1".into(),
            member: MemberEntry {
                user_id: "local-user".into(),
                display_name: "Me".into(),
                away_message: String::new(),
                muted: false,
                deafened: false,
                self_muted: false,
                self_deafened: false,
                streaming: false,
                speaking: false,
                avatar_url: None,
            },
        });

        model.apply_event(UiEvent::SetAwayMessage("brb".into()));

        let member = &model.members["c1"][0];
        assert_eq!(member.away_message, "brb");
    }

    #[test]
    fn member_away_message_event_updates_matching_member() {
        let mut model = UiModel::new();
        model.apply_event(UiEvent::MemberJoined {
            channel_id: "c1".into(),
            member: MemberEntry {
                user_id: "u1".into(),
                display_name: "Other".into(),
                away_message: String::new(),
                muted: false,
                deafened: false,
                self_muted: false,
                self_deafened: false,
                streaming: false,
                speaking: false,
                avatar_url: None,
            },
        });

        model.apply_event(UiEvent::MemberAwayMessageUpdated {
            user_id: "u1".into(),
            away_message: "Lunch".into(),
        });

        let member = &model.members["c1"][0];
        assert_eq!(member.away_message, "Lunch");
    }
    #[test]
    fn member_joined_updates_existing_member_instead_of_dup() {
        let mut model = UiModel::new();
        model.apply_event(UiEvent::MemberJoined {
            channel_id: "c1".into(),
            member: MemberEntry {
                user_id: "u1".into(),
                display_name: "Old".into(),
                away_message: String::new(),
                muted: false,
                deafened: false,
                self_muted: false,
                self_deafened: false,
                streaming: false,
                speaking: false,
                avatar_url: None,
            },
        });
        model.apply_event(UiEvent::MemberJoined {
            channel_id: "c1".into(),
            member: MemberEntry {
                user_id: "u1".into(),
                display_name: "New".into(),
                away_message: String::new(),
                muted: false,
                deafened: false,
                self_muted: false,
                self_deafened: false,
                streaming: false,
                speaking: false,
                avatar_url: None,
            },
        });

        let members = model.members.get("c1").expect("members");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].display_name, "New");
    }

    #[test]
    fn channel_created_updates_existing_channel_instead_of_dup() {
        let mut model = UiModel::new();
        model.apply_event(UiEvent::ChannelCreated(ChannelEntry {
            id: "c1".into(),
            name: "General".into(),
            channel_type: ChannelType::Voice,
            parent_id: None,
            position: 0,
            member_count: 0,
            user_limit: 0,
            description: String::new(),
            bitrate_bps: 64_000,
            opus_profile: 1,
        }));
        model.apply_event(UiEvent::ChannelCreated(ChannelEntry {
            id: "c1".into(),
            name: "General-2".into(),
            channel_type: ChannelType::Voice,
            parent_id: None,
            position: 0,
            member_count: 0,
            user_limit: 0,
            description: String::new(),
            bitrate_bps: 64_000,
            opus_profile: 1,
        }));

        assert_eq!(model.channels.iter().filter(|c| c.id == "c1").count(), 1);
        assert_eq!(
            model.channels.iter().find(|c| c.id == "c1").unwrap().name,
            "General-2"
        );
    }

    #[test]
    fn channel_renamed_updates_name_without_duplicate() {
        let mut model = UiModel::new();
        model.apply_event(UiEvent::SetChannels(vec![ChannelEntry {
            id: "c1".into(),
            name: "General".into(),
            channel_type: ChannelType::Voice,
            parent_id: None,
            position: 0,
            member_count: 0,
            user_limit: 0,
            description: String::new(),
            bitrate_bps: 64_000,
            opus_profile: 1,
        }]));

        model.apply_event(UiEvent::ChannelRenamed(ChannelEntry {
            id: "c1".into(),
            name: "Lobby".into(),
            channel_type: ChannelType::Voice,
            parent_id: None,
            position: 0,
            member_count: 0,
            user_limit: 0,
            description: String::new(),
            bitrate_bps: 64_000,
            opus_profile: 1,
        }));

        assert_eq!(model.channels.len(), 1);
        assert_eq!(model.channels[0].name, "Lobby");
    }

    #[test]
    fn channel_delete_removes_and_falls_back_selection() {
        let mut model = UiModel::new();
        model.apply_event(UiEvent::SetChannels(vec![
            ChannelEntry {
                id: "default".into(),
                name: "Default".into(),
                channel_type: ChannelType::Voice,
                parent_id: None,
                position: 0,
                member_count: 0,
                user_limit: 0,
                description: String::new(),
                bitrate_bps: 64_000,
                opus_profile: 1,
            },
            ChannelEntry {
                id: "c1".into(),
                name: "General".into(),
                channel_type: ChannelType::Voice,
                parent_id: None,
                position: 0,
                member_count: 0,
                user_limit: 0,
                description: String::new(),
                bitrate_bps: 64_000,
                opus_profile: 1,
            },
            ChannelEntry {
                id: "c1-child".into(),
                name: "General Child".into(),
                channel_type: ChannelType::Voice,
                parent_id: Some("c1".into()),
                position: 0,
                member_count: 0,
                user_limit: 0,
                description: String::new(),
                bitrate_bps: 64_000,
                opus_profile: 1,
            },
        ]));
        model.apply_event(UiEvent::SetDefaultChannelId(Some("default".into())));
        model.apply_event(UiEvent::SetChannelName("c1".into()));

        model.apply_event(UiEvent::ChannelDeleted {
            channel_id: "c1".into(),
        });

        assert!(model.channels.iter().all(|ch| ch.id != "c1"));
        assert!(model.channels.iter().all(|ch| ch.id != "c1-child"));
        assert_eq!(model.selected_channel.as_deref(), Some("default"));
    }

    #[test]
    fn sub_channel_keeps_parent_relationship_and_collapse_state_persists() {
        let mut model = UiModel::new();
        model.apply_event(UiEvent::SetChannels(vec![ChannelEntry {
            id: "parent".into(),
            name: "Parent".into(),
            channel_type: ChannelType::Category,
            parent_id: None,
            position: 0,
            member_count: 0,
            user_limit: 0,
            description: String::new(),
            bitrate_bps: 64_000,
            opus_profile: 1,
        }]));
        model.channel_collapsed.insert("parent".into(), true);

        model.apply_event(UiEvent::ChannelCreated(ChannelEntry {
            id: "child".into(),
            name: "Child".into(),
            channel_type: ChannelType::Voice,
            parent_id: Some("parent".into()),
            position: 0,
            member_count: 0,
            user_limit: 0,
            description: String::new(),
            bitrate_bps: 64_000,
            opus_profile: 1,
        }));

        assert_eq!(
            model
                .channels
                .iter()
                .find(|ch| ch.id == "child")
                .and_then(|ch| ch.parent_id.as_deref()),
            Some("parent")
        );
        assert_eq!(model.channel_collapsed.get("parent"), Some(&true));
    }

    #[test]
    fn snapshot_then_rename_delete_yields_expected_channels() {
        let mut model = UiModel::new();
        model.apply_event(UiEvent::SetChannels(vec![
            ChannelEntry {
                id: "c1".into(),
                name: "General".into(),
                channel_type: ChannelType::Voice,
                parent_id: None,
                position: 0,
                member_count: 0,
                user_limit: 0,
                description: String::new(),
                bitrate_bps: 64_000,
                opus_profile: 1,
            },
            ChannelEntry {
                id: "c2".into(),
                name: "Music".into(),
                channel_type: ChannelType::Voice,
                parent_id: None,
                position: 0,
                member_count: 0,
                user_limit: 0,
                description: String::new(),
                bitrate_bps: 64_000,
                opus_profile: 1,
            },
        ]));

        model.apply_event(UiEvent::ChannelRenamed(ChannelEntry {
            id: "c1".into(),
            name: "Lobby".into(),
            channel_type: ChannelType::Voice,
            parent_id: None,
            position: 0,
            member_count: 0,
            user_limit: 0,
            description: String::new(),
            bitrate_bps: 64_000,
            opus_profile: 1,
        }));
        model.apply_event(UiEvent::ChannelDeleted {
            channel_id: "c2".into(),
        });

        assert_eq!(model.channels.len(), 1);
        assert_eq!(model.channels[0].id, "c1");
        assert_eq!(model.channels[0].name, "Lobby");
    }

    #[test]
    fn dedupes_by_canonical_message_id() {
        let mut model = UiModel::new();
        model.user_id = "local-user".into();

        let message = ChatMessage {
            message_id: "msg-1".into(),
            channel_id: "lounge-1".into(),
            author_id: "remote-user".into(),
            author_name: "Dresk".into(),
            text: "hello".into(),
            timestamp: 1_710_000_000_000,
            attachments: vec![],
            reply_to: None,
            reactions: vec![],
            pinned: false,
            edited: false,
        };

        model.apply_event(UiEvent::MessageReceived(message.clone()));
        model.apply_event(UiEvent::MessageReceived(message));

        assert_eq!(model.messages.get("lounge-1").unwrap().len(), 1);
    }

    #[test]
    fn reconciles_optimistic_local_echo_with_server_message() {
        let mut model = UiModel::new();
        model.user_id = "local-user".into();

        model.apply_event(UiEvent::MessageReceived(ChatMessage {
            message_id: "local-1".into(),
            channel_id: "lounge-1".into(),
            author_id: "local-user".into(),
            author_name: "Overdose".into(),
            text: "indeed".into(),
            timestamp: 1_710_000_000_000,
            attachments: vec![],
            reply_to: None,
            reactions: vec![],
            pinned: false,
            edited: false,
        }));

        model.apply_event(UiEvent::MessageReceived(ChatMessage {
            message_id: "msg-123".into(),
            channel_id: "lounge-1".into(),
            author_id: "local-user".into(),
            author_name: "Overdose".into(),
            text: "indeed".into(),
            timestamp: 1_710_000_005_000,
            attachments: vec![],
            reply_to: None,
            reactions: vec![],
            pinned: false,
            edited: false,
        }));

        let messages = model.messages.get("lounge-1").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_id, "msg-123");
    }

    #[test]
    fn keeps_intentional_duplicate_text_messages_as_separate_entries() {
        let mut model = UiModel::new();
        model.user_id = "local-user".into();

        model.apply_event(UiEvent::MessageReceived(ChatMessage {
            message_id: "msg-1".into(),
            channel_id: "lounge-1".into(),
            author_id: "local-user".into(),
            author_name: "Overdose".into(),
            text: "indeed".into(),
            timestamp: 1_710_000_000_000,
            attachments: vec![],
            reply_to: None,
            reactions: vec![],
            pinned: false,
            edited: false,
        }));
        model.apply_event(UiEvent::MessageReceived(ChatMessage {
            message_id: "msg-2".into(),
            channel_id: "lounge-1".into(),
            author_id: "local-user".into(),
            author_name: "Overdose".into(),
            text: "indeed".into(),
            timestamp: 1_710_000_001_000,
            attachments: vec![],
            reply_to: None,
            reactions: vec![],
            pinned: false,
            edited: false,
        }));

        assert_eq!(model.messages.get("lounge-1").unwrap().len(), 2);
    }

    #[test]
    fn set_channel_name_prefers_human_readable_channel_name() {
        let mut model = UiModel::new();
        model.channels.push(ChannelEntry {
            id: "430e12d2-4547-411f-b2b9-434644d8abe0".into(),
            name: "Lounge 1".into(),
            channel_type: ChannelType::Text,
            parent_id: None,
            position: 0,
            member_count: 0,
            user_limit: 0,
            description: String::new(),
            bitrate_bps: 64_000,
            opus_profile: 1,
        });

        model.apply_event(UiEvent::SetChannelName(
            "430e12d2-4547-411f-b2b9-434644d8abe0".into(),
        ));

        assert_eq!(model.selected_channel_name, "Lounge 1");
    }
}
