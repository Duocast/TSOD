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
//! ├─────────┼────────────────────────────┤          │
//! │ User    │  Status bar                │          │
//! │ Panel   │                            │          │
//! └─────────┴────────────────────────────┴──────────┘

pub mod model;
pub mod panels;
pub mod theme;
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

impl VpApp {
    fn signal_quit(&self) {
        let _ = self.tx_intent.send(UiIntent::Quit);
    }
}

impl eframe::App for VpApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.signal_quit();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain backend events
        self.drain_events();

        // Request continuous repaint while connected (for real-time updates)
        if self.model.connected {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Apply theme
        theme::apply_theme(ctx, &self.model.settings.theme);

        // Top menu bar
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.label(egui::RichText::new("TSOD").strong().size(16.0));
                ui.separator();

                let conn_text = if self.model.connected {
                    "Connected"
                } else {
                    "Disconnected"
                };
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

        // Status bar at bottom (simplified — user panel moved to left sidebar)
        egui::TopBottomPanel::bottom("status_bar")
            .max_height(24.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&self.model.status_line)
                            .small()
                            .color(theme::COLOR_TEXT_DIM),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.model.ptt_enabled {
                            let ptt_text = if self.model.ptt_active {
                                "PTT: ON"
                            } else {
                                "PTT: OFF"
                            };
                            let ptt_color = if self.model.ptt_active {
                                theme::COLOR_ONLINE
                            } else {
                                theme::COLOR_OFFLINE
                            };
                            ui.colored_label(ptt_color, egui::RichText::new(ptt_text).small());
                        }
                        if self.model.loopback_active {
                            ui.colored_label(
                                theme::COLOR_MENTION,
                                egui::RichText::new("LOOPBACK").small().strong(),
                            );
                        }
                        if self.model.self_muted {
                            ui.colored_label(
                                theme::COLOR_DANGER,
                                egui::RichText::new("MUTED").small(),
                            );
                        }
                        if self.model.self_deafened {
                            ui.colored_label(
                                theme::COLOR_DANGER,
                                egui::RichText::new("DEAFENED").small(),
                            );
                        }
                    });
                });
            });

        // Left panel: server/channel tree + user panel at bottom
        egui::SidePanel::left("server_tree")
            .default_width(220.0)
            .min_width(180.0)
            .show(ctx, |ui| {
                let total_height = ui.available_height();
                let user_panel_height = 100.0;
                let tree_height = (total_height - user_panel_height).max(100.0);

                // Channel tree (scrollable, takes most space)
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), tree_height),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        panels::server_tree::show(ui, &mut self.model, &self.tx_intent);
                    },
                );

                // Separator between channel tree and user panel
                ui.separator();

                // User panel at the bottom of the sidebar
                panels::user_panel::show(ui, &mut self.model, &self.tx_intent);
            });
