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

pub use model::{ConnectionStage, UiEvent, UiIntent, UiModel};

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

    fn launch_connect_attempt(&mut self, close_dialog_on_success: bool) -> bool {
        let host = self.model.connection_host_draft.trim().to_string();
        let port_text = self.model.connection_port_draft.trim();
        let nickname = self.model.connection_nickname_draft.trim().to_string();

        if host.is_empty() {
            self.model.connection_error = "Host/IP cannot be empty.".to_string();
            return false;
        }
        if nickname.is_empty() {
            self.model.connection_error = "Nickname cannot be empty.".to_string();
            return false;
        }

        let Ok(port) = port_text.parse::<u16>() else {
            self.model.connection_error = "Port must be a number between 1 and 65535.".to_string();
            return false;
        };

        self.model.connection_error.clear();
        self.model.settings.identity_nickname = nickname.clone();
        self.model.settings.last_server_host = host.clone();
        self.model.settings_draft.identity_nickname = nickname.clone();
        self.model.settings_draft.last_server_host = host.clone();
        let _ = crate::settings_io::save_settings(&self.model.settings);
        let _ = self.tx_intent.send(UiIntent::SaveSettings(Box::new(
            self.model.settings.clone(),
        )));

        match self.tx_intent.send(UiIntent::ConnectToServer {
            host,
            port,
            nickname,
        }) {
            Ok(()) => {
                if close_dialog_on_success {
                    self.model.show_connections = false;
                }
                true
            }
            Err(_) => {
                self.model.connection_error =
                    "Failed to start connection attempt (UI/backend channel closed).".to_string();
                false
            }
        }
    }

    fn copy_connection_details(&mut self, ctx: &egui::Context) {
        let mut lines = Vec::new();
        lines.push(format!(
            "Connection stage: {}",
            self.model.connection_stage.label()
        ));
        lines.push(format!(
            "Server: {}:{}",
            self.model.connection_host_draft.trim(),
            self.model.connection_port_draft.trim()
        ));
        lines.push("Details:".to_string());

        if self.model.connection_details.is_empty() {
            lines.push("- (no connection details yet)".to_string());
        } else {
            for detail in self.model.connection_details.iter().rev().take(8) {
                lines.push(format!("- {detail}"));
            }
        }

        ctx.copy_text(lines.join("\n"));
        self.model.notifications.push_back(model::Notification {
            text: "Connection details copied".to_string(),
            created: std::time::Instant::now(),
            kind: model::NotificationKind::Info,
        });
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

                if self.model.connection_stage.is_in_progress() {
                    ui.separator();
                    ui.label(egui::RichText::new("⏳").small());
                    ui.label(
                        egui::RichText::new(self.model.connection_stage.label())
                            .small()
                            .color(theme::COLOR_MENTION),
                    );
                    if ui.small_button("Cancel").clicked() {
                        let _ = self.tx_intent.send(UiIntent::CancelConnect);
                    }
                } else if self.model.connection_stage == ConnectionStage::Failed {
                    ui.separator();
                    ui.colored_label(theme::COLOR_DANGER, "Connection failed");
                }

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
                    if ui.button("Connections").clicked() {
                        self.model.show_connections = !self.model.show_connections;
                        self.model.connection_error.clear();
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

                // Push the user panel to the bottom of the sidebar.
                let spacer = (ui.available_height() - 112.0).max(4.0);
                ui.add_space(spacer);
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

        // Connections window (floating)
        if self.model.show_connections {
            let mut open = true;
            egui::Window::new("Connections")
                .open(&mut open)
                .default_width(360.0)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Server address:");
                    ui.horizontal(|ui| {
                        ui.label("IP / Host");
                        ui.add_sized(
                            [ui.available_width() - 70.0, 24.0],
                            egui::TextEdit::singleline(&mut self.model.connection_host_draft),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Port");
                        ui.add_sized(
                            [80.0, 24.0],
                            egui::TextEdit::singleline(&mut self.model.connection_port_draft),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Nickname");
                        ui.add_sized(
                            [ui.available_width() - 70.0, 24.0],
                            egui::TextEdit::singleline(&mut self.model.connection_nickname_draft)
                                .hint_text("Nickname used when connecting/joining channels"),
                        );
                    });
                    ui.label(
                        egui::RichText::new("Nickname used when connecting/joining channels")
                            .small()
                            .color(theme::text_muted()),
                    );
                    ui.label(
                        egui::RichText::new("Changes apply immediately when you press Connect.")
                            .small()
                            .color(theme::text_dim()),
                    );

                    if !self.model.connection_error.is_empty() {
                        ui.colored_label(theme::COLOR_DANGER, &self.model.connection_error);
                    }

                    ui.add_space(8.0);
                    let stage_color = if self.model.connection_stage == ConnectionStage::Failed {
                        theme::COLOR_DANGER
                    } else if self.model.connection_stage == ConnectionStage::Connected {
                        theme::COLOR_ONLINE
                    } else {
                        theme::text_muted()
                    };
                    ui.label(
                        egui::RichText::new(format!(
                            "Status: {}",
                            self.model.connection_stage.label()
                        ))
                        .small()
                        .color(stage_color),
                    );

                    let connect_label = if self.model.connection_stage.is_in_progress() {
                        "Reconnect"
                    } else {
                        "Connect"
                    };
                    let connect_clicked = ui.button(connect_label).clicked();
                    if connect_clicked {
                        self.launch_connect_attempt(true);
                    }

                    ui.add_space(6.0);
                    ui.collapsing("Connection details", |ui| {
                        if self.model.connection_details.is_empty() {
                            ui.label(
                                egui::RichText::new("No connection attempts yet.")
                                    .small()
                                    .color(theme::text_muted()),
                            );
                        } else {
                            for line in self.model.connection_details.iter().rev().take(8) {
                                ui.label(egui::RichText::new(line).small());
                            }
                        }
                    });
                });
            if !open {
                self.model.show_connections = false;
                self.model.connection_error.clear();
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

                    ui.label("Enter a local image path (png, jpg, jpeg, gif, or webp):");
                    ui.add_sized(
                        [ui.available_width(), 24.0],
                        egui::TextEdit::singleline(&mut self.model.avatar_path_draft),
                    );

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
        panels::server_tree::show_channel_dialogs(ctx, &mut self.model, &self.tx_intent);

        // Central panel: connection status + chat messages + input
        egui::CentralPanel::default().show(ctx, |ui| {
            let stage = self.model.connection_stage;
            let panel_visible = stage.is_in_progress()
                || stage == ConnectionStage::Failed
                || (!self.model.connected && !self.model.connection_details.is_empty());
            let stage_color = if stage == ConnectionStage::Failed {
                theme::COLOR_DANGER
            } else if stage == ConnectionStage::Connected {
                theme::COLOR_ONLINE
            } else if stage.is_in_progress() {
                theme::COLOR_MENTION
            } else {
                theme::text_muted()
            };

            if panel_visible {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(format!("Connection: {}", stage.label()))
                                .small()
                                .color(stage_color)
                                .strong(),
                        );
                        if stage.is_in_progress() {
                            ui.label(egui::RichText::new("⏳ in progress").small());
                        }
                        if ui.small_button("Open Connections").clicked() {
                            self.model.show_connections = true;
                        }
                        if stage.is_in_progress() && ui.small_button("Cancel").clicked() {
                            let _ = self.tx_intent.send(UiIntent::CancelConnect);
                        }
                        if stage == ConnectionStage::Failed && ui.small_button("Retry").clicked() {
                            let _ = self.launch_connect_attempt(false);
                        }
                        if ui.small_button("Copy details").clicked() {
                            self.copy_connection_details(ctx);
                        }
                    });

                    if stage == ConnectionStage::Failed && !self.model.connection_error.is_empty() {
                        ui.colored_label(
                            theme::COLOR_DANGER,
                            egui::RichText::new(&self.model.connection_error).small(),
                        );
                    }

                    for line in self.model.connection_details.iter().rev().take(4) {
                        ui.label(egui::RichText::new(line).small().color(theme::text_dim()));
                    }
                });
                ui.add_space(6.0);
            } else {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Connection: {}", stage.label()))
                            .small()
                            .color(theme::text_dim()),
                    );
                    if ui.small_button("Connections").clicked() {
                        self.model.show_connections = true;
                    }
                });
                ui.add_space(4.0);
            }

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
            self.model.show_connections = false;
            self.model.show_create_channel = false;
        }
    }
}
