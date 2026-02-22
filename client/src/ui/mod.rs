//! GUI module using egui/eframe.
//!
//! Layout:
//! ┌─────────┬────────────────────────────┬──────────┐
//! │ Server  │  Chat / Voice Panel        │ Members  │
//! │ Tree    │                            │ List     │
//! │         │                            │          │
//! │         │                            │          │
//! │         ├────────────────────────────┤          │
//! │         │  Input bar                 │          │
//! │         ├────────────────────────────┤          │
//! │ ┌─────┐ │  Status bar               │          │
//! │ │User │ │                            │          │
//! │ │Panel│ │                            │          │
//! └─────────┴────────────────────────────┴──────────┘

pub mod model;
pub mod theme;
pub mod panels;
pub mod widgets;

pub use model::{UiEvent, UiIntent, UiModel};

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

/// The main application struct that implements `eframe::App`.
pub struct VpApp {
    model: UiModel,
    tx_intent: Sender<UiIntent>,
    rx_event: Receiver<UiEvent>,
}

impl VpApp {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        tx_intent: Sender<UiIntent>,
        rx_event: Receiver<UiEvent>,
    ) -> Self {
        Self {
            model: UiModel::default(),
            tx_intent,
            rx_event,
        }
    }

    /// Drain all pending backend events and apply them to the model.
    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx_event.try_recv() {
            self.model.apply_event(ev);
        }
    }
}

impl eframe::App for VpApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain backend events
        self.drain_events();

        // Request continuous repaint while connected (for real-time updates)
        if self.model.connected {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Apply theme
        theme::apply_theme(ctx);

        // Top menu bar
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.label(
                    egui::RichText::new("TSOD")
                        .strong()
                        .size(16.0),
                );
                ui.separator();

                let conn_text = if self.model.connected { "Connected" } else { "Disconnected" };
                let conn_color = if self.model.connected {
                    theme::COLOR_ONLINE
                } else {
                    theme::COLOR_OFFLINE
                };
                ui.colored_label(conn_color, conn_text);

                if self.model.authed {
                    ui.separator();
                    ui.label(format!("Logged in as {}", self.model.nick));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.model.show_settings = !self.model.show_settings;
                    }
                    if ui.button("Telemetry").clicked() {
                        self.model.show_telemetry = !self.model.show_telemetry;
                    }
                });
            });
        });

        // Status bar at bottom
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // User panel (avatar, status, mute/deafen)
                panels::user_panel::show(ui, &self.model, &self.tx_intent);
                ui.separator();
                ui.label(&self.model.status_line);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.model.ptt_enabled {
                        let ptt_text = if self.model.ptt_active { "PTT: ON" } else { "PTT: OFF" };
                        let ptt_color = if self.model.ptt_active {
                            theme::COLOR_ONLINE
                        } else {
                            theme::COLOR_OFFLINE
                        };
                        ui.colored_label(ptt_color, ptt_text);
                    }
                    if let Some(vad) = self.model.vad_level {
                        let bar_width = 60.0;
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(bar_width, 12.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(
                            rect,
                            2.0,
                            egui::Color32::from_gray(40),
                        );
                        let filled = egui::Rect::from_min_size(
                            rect.min,
                            egui::vec2(bar_width * vad, 12.0),
                        );
                        let vad_color = if vad > 0.5 {
                            theme::COLOR_ONLINE
                        } else {
                            theme::COLOR_IDLE
                        };
                        ui.painter().rect_filled(filled, 2.0, vad_color);
                    }
                });
            });
        });

        // Left panel: server/channel tree
        egui::SidePanel::left("server_tree")
            .default_width(220.0)
            .min_width(180.0)
            .show(ctx, |ui| {
                panels::server_tree::show(ui, &mut self.model, &self.tx_intent);
            });

        // Right panel: member list
        egui::SidePanel::right("members_panel")
            .default_width(200.0)
            .min_width(150.0)
            .show(ctx, |ui| {
                panels::members::show(ui, &self.model, &self.tx_intent);
            });

        // Settings window (floating)
        if self.model.show_settings {
            let mut open = true;
            egui::Window::new("Settings")
                .open(&mut open)
                .show(ctx, |ui| {
                    panels::settings::show(ui, &mut self.model, &self.tx_intent);
                });
            if !open {
                self.model.show_settings = false;
            }
        }

        // Telemetry window (floating)
        if self.model.show_telemetry {
            let mut open = true;
            egui::Window::new("Connection Telemetry")
                .open(&mut open)
                .default_width(400.0)
                .show(ctx, |ui| {
                    panels::telemetry::show(ui, &self.model);
                });
            if !open {
                self.model.show_telemetry = false;
            }
        }

        // Central panel: chat messages + input
        egui::CentralPanel::default().show(ctx, |ui| {
            panels::chat::show(ui, &mut self.model, &self.tx_intent);
        });

        // Handle keyboard shortcuts
        self.handle_shortcuts(ctx);
    }
}

impl VpApp {
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let input = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_released(egui::Key::Space),
                i.key_pressed(egui::Key::Escape),
                i.modifiers.ctrl,
            )
        });

        let (space_pressed, space_released, esc_pressed, _ctrl) = input;

        // PTT: space down = talk, space up = stop
        if self.model.ptt_enabled {
            if space_pressed && !self.model.chat_input_focused {
                let _ = self.tx_intent.send(UiIntent::PttDown);
                self.model.ptt_active = true;
            }
            if space_released && !self.model.chat_input_focused {
                let _ = self.tx_intent.send(UiIntent::PttUp);
                self.model.ptt_active = false;
            }
        }

        if esc_pressed {
            self.model.show_settings = false;
            self.model.show_telemetry = false;
        }
    }
}
