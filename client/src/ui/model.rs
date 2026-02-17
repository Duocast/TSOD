use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct UiModel {
    pub title: String,

    pub connected: bool,
    pub authed: bool,
    pub channel_name: String,
    pub nick: String,

    pub channels: Vec<String>,
    pub selected_channel: usize,

    pub log: VecDeque<String>,
    pub input: String,

    pub ptt_enabled: bool,
    pub ptt_active: bool,

    pub status_line: String,
}

impl Default for UiModel {
    fn default() -> Self {
        Self {
            title: "vp-client".into(),
            connected: false,
            authed: false,
            channel_name: "-".into(),
            nick: "user".into(),
            channels: vec!["Lobby".into()],
            selected_channel: 0,
            log: VecDeque::with_capacity(500),
            input: String::new(),
            ptt_enabled: true,
            ptt_active: false,
            status_line: "F1 help | Tab focus | Enter send | Space PTT | q quit".into(),
        }
    }
}

impl UiModel {
    pub fn push_log(&mut self, line: impl Into<String>) {
        if self.log.len() >= 500 {
            self.log.pop_front();
        }
        self.log.push_back(line.into());
    }
}

/// Events from UI thread to app thread (high-level).
#[derive(Clone, Debug)]
pub enum UiIntent {
    Quit,
    SendChat { text: String },
    JoinChannel { name: String },
    TogglePtt,
    PttDown,
    PttUp,
    SelectNextChannel,
    SelectPrevChannel,
    Help,
}

/// Events from app thread to UI thread (state updates).
#[derive(Clone, Debug)]
pub enum UiEvent {
    SetConnected(bool),
    SetAuthed(bool),
    SetChannelName(String),
    AppendLog(String),
    SetStatus(String),
    SetChannels(Vec<String>),
}
