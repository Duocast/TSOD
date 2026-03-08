//! GUI module using iced runtime/view/subscription plumbing.

pub mod model;
pub mod sfx;
pub mod widgets;

pub use model::{UiEvent, UiIntent, UiModel};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{time, Center, Element, Fill, Subscription, Task};
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
            button(if self.model.show_connections { "Hide connect" } else { "Show connect" })
                .on_press(Message::ToggleConnections)
        ]
        .spacing(16)
        .align_y(Center);

        let connection_panel = if self.model.show_connections {
            column![
                text_input("Host", &self.model.connection_host_draft).on_input(Message::HostChanged),
                text_input("Port", &self.model.connection_port_draft).on_input(Message::PortChanged),
                text_input("Nickname", &self.model.connection_nickname_draft)
                    .on_input(Message::NickChanged),
                button("Connect").on_press(Message::ConnectPressed),
                text(&self.model.connection_error),
            ]
            .spacing(8)
        } else {
            column![]
        };

        let logs = self
            .model
            .log
            .iter()
            .rev()
            .take(100)
            .fold(column![text("Recent logs").size(20)], |col: iced::widget::Column<'_, Message>, line| {
                col.push(text(line).size(14))
            });

        let content = column![header, connection_panel, scrollable(logs).height(Fill)]
            .spacing(12)
            .padding(16);

        container(content)
            .width(Fill)
            .height(Fill)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        time::every(Duration::from_millis(16)).map(Message::Tick)
    }
}

pub fn run_ui(tx_intent: Sender<UiIntent>, rx_event: Receiver<UiEvent>) -> Result<()> {
    iced::application(move || VpApp::new(tx_intent.clone(), rx_event.clone()), VpApp::update, VpApp::view)
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
