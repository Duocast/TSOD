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
        cc: &eframe::CreationContext<'_>,
        tx_intent: Sender<UiIntent>,
        rx_event: Receiver<UiEvent>,
    ) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);

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
        let _ = self.tx_intent.try_send(UiIntent::Quit);
    }

    fn persist_settings_if_dirty(&mut self) {
        if !self.model.settings_dirty {
            return;
        }

        self.model.settings = self.model.settings_draft.clone();
        self.model.settings_dirty = false;
        self.model.sync_settings_to_runtime();

        let _ = self.tx_intent.try_send(UiIntent::ApplySettings(Box::new(
            self.model.settings.clone(),
        )));
        let _ = self.tx_intent.try_send(UiIntent::SaveSettings(Box::new(
            self.model.settings.clone(),
        )));
    }
}

impl eframe::App for VpApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist_settings_if_dirty();
        self.signal_quit();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain backend events
        self.drain_events();

        // Request continuous repaint for real-time views (connection telemetry or mic test)
        if self.model.connected || self.model.loopback_active {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        // Apply UI scale from settings.
        let ui_scale = self.model.settings.ui_scale.clamp(0.75, 2.0);
        if (ctx.pixels_per_point() - ui_scale).abs() > f32::EPSILON {
            ctx.set_pixels_per_point(ui_scale);
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
                        if self.model.show_settings {
                            self.persist_settings_if_dirty();
                        }
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
                            .color(theme::text_dim()),
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

        // Right panel: member list
        egui::SidePanel::right("members_panel")
            .default_width(200.0)
            .min_width(150.0)
            .show(ctx, |ui| {
                panels::members::show(ui, &self.model, &self.tx_intent);
            });

        // Settings window (floating, TS3-style Options dialog)
        if self.model.show_settings {
            let mut open = true;
            egui::Window::new("Options")
                .open(&mut open)
                .default_width(750.0)
                .default_height(550.0)
                .min_width(600.0)
                .min_height(400.0)
                .collapsible(false)
                .show(ctx, |ui| {
                    panels::settings::show(ui, &mut self.model, &self.tx_intent);
                });
            if !open {
                // Auto-apply pending settings when closing the window.
                if self.model.settings_dirty {
                    self.model.settings = self.model.settings_draft.clone();
                    self.model.settings_dirty = false;
                    self.model.sync_settings_to_runtime();
                    let _ = self.tx_intent.send(UiIntent::ApplySettings(Box::new(
                        self.model.settings.clone(),
                    )));
                    let _ = self.tx_intent.send(UiIntent::SaveSettings(Box::new(
                        self.model.settings.clone(),
                    )));
                    let _ = crate::settings_io::save_settings(&self.model.settings);
                }
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

        // Away message dialog (TS3-style)
        if self.model.show_away_message_dialog {
            let mut open = true;
            egui::Window::new("Set Away Message")
                .open(&mut open)
                .default_width(360.0)
                .min_width(320.0)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Message:");
                        ui.add_sized(
                            [ui.available_width(), 24.0],
                            egui::TextEdit::singleline(&mut self.model.away_message_draft),
                        );
                    });

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label("Preset:");
                        egui::ComboBox::from_id_source("away_message_preset")
                            .selected_text("<None>")
                            .show_ui(ui, |ui| {
                                for preset in &self.model.away_message_presets {
                                    if ui.selectable_label(false, preset).clicked() {
                                        self.model.away_message_draft = preset.clone();
                                    }
                                }
                            });
                    });

                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let apply_away = |model: &mut UiModel, tx_intent: &Sender<UiIntent>| {
                            model.away_message = model.away_message_draft.trim().to_string();
                            let _ = tx_intent.send(UiIntent::SetAwayMessage {
                                message: model.away_message.clone(),
                            });
                        };

                        if ui.button("OK").clicked() {
                            apply_away(&mut self.model, &self.tx_intent);
                            self.model.show_away_message_dialog = false;
                        }
                        if ui.button("Save").clicked() {
                            apply_away(&mut self.model, &self.tx_intent);
                        }
                        if ui.button("Cancel").clicked() {
                            self.model.away_message_draft = self.model.away_message.clone();
                            self.model.show_away_message_dialog = false;
                        }
                    });
                });
            if !open {
                self.model.show_away_message_dialog = false;
            }
        }

        if self.model.show_set_avatar_dialog {
            let mut open = true;
            egui::Window::new("Set Avatar")
                .open(&mut open)
                .default_width(420.0)
                .min_width(380.0)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Choose an image file for your avatar.");
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        if ui.button("Add Image").clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("Image", &["png", "jpg", "jpeg", "gif", "webp"])
                                .pick_file()
                            {
                                self.model.avatar_path_draft = path.display().to_string();
                            }
                        }

                        let selected = if self.model.avatar_path_draft.is_empty() {
                            "No image selected".to_string()
                        } else {
                            self.model.avatar_path_draft.clone()
                        };
                        ui.label(
                            egui::RichText::new(selected)
                                .small()
                                .color(theme::text_muted()),
                        );
                    });

                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Apply").clicked() {
                            let chosen = self.model.avatar_path_draft.trim().to_string();
                            if !chosen.is_empty() {
                                self.model.avatar_url = Some(format!("file://{chosen}"));
                                let _ = self.tx_intent.send(UiIntent::SetAvatar { path: chosen });
                                self.model.show_set_avatar_dialog = false;
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            self.model.show_set_avatar_dialog = false;
                        }
                    });
                });
            if !open {
                self.model.show_set_avatar_dialog = false;
            }
        }

        // Create channel dialog (floating)
        panels::server_tree::show_create_channel_dialog(ctx, &mut self.model, &self.tx_intent);

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
                i.key_pressed(egui::Key::M),
                i.key_pressed(egui::Key::D),
            )
        });

        let (space_pressed, space_released, esc_pressed, ctrl, m_pressed, d_pressed) = input;

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

        // Ctrl+M = toggle mute
        if ctrl && m_pressed && !self.model.chat_input_focused {
            self.model.self_muted = !self.model.self_muted;
            let _ = self.tx_intent.send(UiIntent::ToggleSelfMute);
        }

        // Ctrl+D = toggle deafen
        if ctrl && d_pressed && !self.model.chat_input_focused {
            self.model.self_deafened = !self.model.self_deafened;
            let _ = self.tx_intent.send(UiIntent::ToggleSelfDeafen);
        }

        if esc_pressed {
            self.model.show_settings = false;
            self.model.show_telemetry = false;
            self.model.show_create_channel = false;
        }
    }
}
