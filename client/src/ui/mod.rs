//! GUI module using iced runtime/view/subscription plumbing.

pub mod model;
pub mod sfx;
pub mod widgets;

pub use model::{UiEvent, UiIntent, UiModel};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use iced::widget::{
    button, column, container, horizontal_rule, row, scrollable, text, text_input, vertical_rule,
};
use iced::{time, Center, Element, Fill, Length, Subscription, Task};
use std::time::{Duration, Instant};

use crate::BUILD_VERSION;

#[derive(Debug, Clone)]
enum Message {
    Tick(Instant),
    HostChanged(String),
    PortChanged(String),
    NickChanged(String),
    ConnectPressed,
    ToggleConnections,
    SelectChannel(String),
    ChatInputChanged(String),
    SendChat,
    ToggleSelfMute,
    ToggleSelfDeafen,
}

pub struct VpApp {
    model: UiModel,
    tx_intent: Sender<UiIntent>,
    rx_event: Receiver<UiEvent>,
}

impl Drop for VpApp {
    fn drop(&mut self) {
        let _ = self.tx_intent.try_send(UiIntent::Quit);
    }
}

impl VpApp {
    fn new(tx_intent: Sender<UiIntent>, rx_event: Receiver<UiEvent>) -> (Self, Task<Message>) {
        (
            Self {
                model: UiModel::default(),
                tx_intent,
                rx_event,
            },
            Task::none(),
        )
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx_event.try_recv() {
            self.model.apply_event(ev);
        }
    }

    fn launch_connect_attempt(&mut self) {
        let host = self.model.connection_host_draft.trim().to_string();
        let port_text = self.model.connection_port_draft.trim();
        let nickname = self.model.connection_nickname_draft.trim().to_string();

        if host.is_empty() {
            self.model.connection_error = "Host/IP cannot be empty.".to_string();
            return;
        }
        if nickname.is_empty() {
            self.model.connection_error = "Nickname cannot be empty.".to_string();
            return;
        }

        let Ok(port) = port_text.parse::<u16>() else {
            self.model.connection_error = "Port must be a number between 1 and 65535.".to_string();
            return;
        };

        self.model.connection_error.clear();
        let _ = self.tx_intent.send(UiIntent::ConnectToServer {
            host,
            port,
            nickname,
        });
    }

    fn title(&self) -> String {
        format!("TSOD {BUILD_VERSION}")
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick(_) => self.drain_events(),
            Message::HostChanged(v) => self.model.connection_host_draft = v,
            Message::PortChanged(v) => self.model.connection_port_draft = v,
            Message::NickChanged(v) => self.model.connection_nickname_draft = v,
            Message::ConnectPressed => self.launch_connect_attempt(),
            Message::ToggleConnections => {
                self.model.show_connections = !self.model.show_connections;
            }
            Message::SelectChannel(channel_id) => {
                self.model.selected_channel = Some(channel_id.clone());
                self.model.selected_channel_name = self
                    .model
                    .channels
                    .iter()
                    .find(|channel| channel.id == channel_id)
                    .map(|channel| channel.name.clone())
                    .unwrap_or(channel_id.clone());
                let _ = self.tx_intent.send(UiIntent::JoinChannel { channel_id });
            }
            Message::ChatInputChanged(value) => {
                self.model.chat_composer.set_text(&value);
            }
            Message::SendChat => {
                let text = self.model.chat_composer.text().trim().to_string();
                if !text.is_empty() {
                    let _ = self.tx_intent.send(UiIntent::SendChat {
                        text,
                        attachments: Vec::new(),
                    });
                    self.model.chat_composer.clear();
                }
            }
            Message::ToggleSelfMute => {
                self.model.self_muted = !self.model.self_muted;
                let _ = self.tx_intent.send(UiIntent::ToggleSelfMute);
            }
            Message::ToggleSelfDeafen => {
                self.model.self_deafened = !self.model.self_deafened;
                let _ = self.tx_intent.send(UiIntent::ToggleSelfDeafen);
            }
        }

        Task::none()
    }

    fn view(&self) -> Element<Message> {
        let status = if self.model.connected {
            "Connected"
        } else {
            "Disconnected"
        };

        let header = row![
            text("TSOD").size(28),
            text(BUILD_VERSION).size(16),
            text(status).size(16),
            text(if self.model.authed {
                format!("Logged in as {}", self.model.nick)
            } else {
                String::new()
            })
            .size(16),
            row![
                button("Telemetry"),
                button("Permissions"),
                button("Settings"),
                button("About")
            ]
            .spacing(8),
            button(if self.model.show_connections {
                "Hide connect"
            } else {
                "Show connect"
            })
            .on_press(Message::ToggleConnections)
        ]
        .spacing(16)
        .align_y(Center);

        let connection_panel = if self.model.show_connections {
            column![
                text_input("Host", &self.model.connection_host_draft)
                    .on_input(Message::HostChanged),
                text_input("Port", &self.model.connection_port_draft)
                    .on_input(Message::PortChanged),
                text_input("Nickname", &self.model.connection_nickname_draft)
                    .on_input(Message::NickChanged),
                button("Connect").on_press(Message::ConnectPressed),
                text(&self.model.connection_error),
            ]
            .spacing(8)
        } else {
            column![]
        };

        let channels_list = self.model.channels.iter().fold(
            column![
                row![text("Channels").size(32), button("+")].spacing(8),
                horizontal_rule(1)
            ]
            .spacing(8),
            |col, channel| {
                let label = if self
                    .model
                    .selected_channel
                    .as_ref()
                    .is_some_and(|selected| selected == &channel.id)
                {
                    format!("▶ {}", channel.name)
                } else {
                    channel.name.clone()
                };
                col.push(
                    button(text(label).size(22))
                        .on_press(Message::SelectChannel(channel.id.clone())),
                )
            },
        );

        let messages = self.model.current_messages().map_or_else(
            || column![text("No messages yet. Start the conversation!").size(28)],
            |items| {
                items
                    .iter()
                    .rev()
                    .take(200)
                    .fold(column![], |col, message| {
                        col.push(
                            text(format!("{}: {}", message.author_name, message.text)).size(18),
                        )
                    })
            },
        );

        let composer_text = self.model.chat_composer.text();

        let center_panel = column![
            row![
                text(format!("Connection: {}", status)).size(14),
                button("Connections").on_press(Message::ToggleConnections)
            ]
            .spacing(8),
            text(if self.model.selected_channel_name.is_empty() {
                "Select a channel".to_string()
            } else {
                self.model.selected_channel_name.clone()
            })
            .size(40),
            horizontal_rule(1),
            scrollable(messages).height(Length::FillPortion(8)),
            text_input("Type a message...", &composer_text)
                .on_input(Message::ChatInputChanged)
                .on_submit(Message::SendChat)
                .padding(10)
                .size(18),
            row![button("Send").on_press(Message::SendChat), button("+")].spacing(8),
            text(&self.model.status_line).size(16)
        ]
        .spacing(8)
        .width(Length::FillPortion(5));

        let members_list = self.model.current_members().iter().fold(
            column![text("Members").size(32), horizontal_rule(1)].spacing(8),
            |col, member| col.push(text(member.display_name.clone()).size(22)),
        );

        let user_panel = column![
            text(&self.model.nick).size(24),
            text(if self.model.connected {
                "Online"
            } else {
                "Offline"
            })
            .size(16),
            row![
                button(if self.model.self_muted {
                    "Unmute"
                } else {
                    "Mute"
                })
                .on_press(Message::ToggleSelfMute),
                button(if self.model.self_deafened {
                    "Undeafen"
                } else {
                    "Deafen"
                })
                .on_press(Message::ToggleSelfDeafen)
            ]
            .spacing(8)
        ]
        .spacing(8);

        let body = row![
            column![
                container(scrollable(channels_list).height(Fill)).height(Length::FillPortion(3)),
                horizontal_rule(1),
                container(user_panel).height(Length::FillPortion(1))
            ]
            .width(Length::FillPortion(2))
            .spacing(8),
            vertical_rule(1),
            center_panel,
            vertical_rule(1),
            container(scrollable(members_list).height(Fill)).width(Length::FillPortion(2))
        ]
        .height(Fill)
        .spacing(12);

        let logs = self.model.log.iter().rev().take(15).fold(
            column![text("Recent logs").size(18)],
            |col: iced::widget::Column<'_, Message>, line| col.push(text(line).size(12)),
        );

        let content = column![
            header,
            connection_panel,
            body,
            scrollable(logs).height(Length::FillPortion(1))
        ]
        .spacing(12)
        .padding(16);

        container(content).width(Fill).height(Fill).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        time::every(Duration::from_millis(16)).map(Message::Tick)
    }
}

pub fn run_ui(tx_intent: Sender<UiIntent>, rx_event: Receiver<UiEvent>) -> Result<()> {
    iced::application(
        move || VpApp::new(tx_intent.clone(), rx_event.clone()),
        VpApp::update,
        VpApp::view,
    )
    .title(VpApp::title)
    .subscription(VpApp::subscription)
    .window(iced::window::Settings {
        size: iced::Size::new(1200.0, 800.0),
        min_size: Some(iced::Size::new(800.0, 500.0)),
        ..Default::default()
    })
    .antialiasing(true)
    .run()?;
    Ok(())
}
