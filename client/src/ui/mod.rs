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
pub mod sfx;
pub mod theme;
pub mod widgets;

pub use model::{ConnectionStage, UiEvent, UiIntent, UiModel};

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

use crate::BUILD_VERSION;

const TSOD_LOGO: egui::ImageSource<'static> = egui::include_image!("../../assets/tsod-logo.svg");
const THIRD_PARTY_LICENSES: &str = include_str!("../../assets/third_party_licenses.tsv");

fn share_source_card(
    ui: &mut egui::Ui,
    selection: &model::ShareSourceSelection,
    title: &str,
    subtitle: &str,
    selected_share_source: &mut Option<model::ShareSourceSelection>,
) -> bool {
    let is_selected = selected_share_source.as_ref() == Some(selection);
    let stroke = if is_selected {
        egui::Stroke::new(1.5, theme::COLOR_ONLINE)
    } else {
        egui::Stroke::new(1.0, theme::bg_medium())
    };
    let fill = if is_selected {
        theme::bg_light()
    } else {
        theme::bg_dark()
    };

    let frame = egui::Frame::new()
        .fill(fill)
        .corner_radius(8.0)
        .stroke(stroke)
        .inner_margin(10.0);
    let response = frame
        .show(ui, |ui| {
            ui.set_min_size(egui::vec2(180.0, 90.0));
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(title).strong().size(13.0));
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(subtitle)
                        .size(11.0)
                        .color(theme::text_muted()),
                );
            });
        })
        .response
        .interact(egui::Sense::click());

    if response.clicked() {
        *selected_share_source = Some(selection.clone());
        true
    } else {
        false
    }
}

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
        max_upload_mb: u64,
    ) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let mut model = UiModel::default();
        model.max_upload_bytes = max_upload_mb.saturating_mul(1024 * 1024);

        Self {
            model,
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
        self.model.settings.last_server_port = port;
        self.model.settings_draft.identity_nickname = nickname.clone();
        self.model.settings_draft.last_server_host = host.clone();
        self.model.settings_draft.last_server_port = port;
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

        // Request continuous repaint for real-time views (voice meters, telemetry, mic test)
        if self.model.connected || self.model.loopback_active {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
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
            egui::MenuBar::new().ui(ui, |ui| {
                ui.label(egui::RichText::new("TSOD").strong().size(16.0));
                ui.label(egui::RichText::new(BUILD_VERSION).size(12.0).monospace());
                ui.add_sized([6.0, 18.0], egui::Separator::default().vertical());

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
                    if ui.button("About").clicked() {
                        self.model.show_about = true;
                    }
                    if ui.button("Settings").clicked() {
                        if self.model.show_settings {
                            self.persist_settings_if_dirty();
                        }
                        self.model.show_settings = !self.model.show_settings;
                    }
                    if ui.button("Permissions").clicked() {
                        self.model.show_permissions_center = true;
                        let _ = self.tx_intent.send(model::UiIntent::PermsOpen);
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
                    let in_voice_channel = self.model.active_voice_channel_route != 0;
                    let voice_conn = if in_voice_channel && self.model.voice_session_healthy {
                        ("Voice: connected", theme::COLOR_ONLINE)
                    } else if in_voice_channel {
                        ("Voice: reconnecting", theme::COLOR_MENTION)
                    } else {
                        ("Voice: not in voice channel", theme::text_muted())
                    };
                    ui.label(
                        egui::RichText::new(voice_conn.0)
                            .small()
                            .color(voice_conn.1),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.model.ptt_enabled {
                            let (ptt_text, ptt_color) = if !in_voice_channel {
                                ("PTT: DISABLED", theme::text_muted())
                            } else if self.model.ptt_active {
                                ("PTT: ON", theme::COLOR_ONLINE)
                            } else {
                                ("PTT: OFF", theme::COLOR_OFFLINE)
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
                panels::members::show(ui, &mut self.model, &self.tx_intent);
            });

        if self.model.show_about {
            let mut open = true;
            egui::Window::new("About TSOD")
                .constrain(false)
                .open(&mut open)
                .default_width(640.0)
                .default_height(500.0)
                .min_width(520.0)
                .min_height(360.0)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.model.about_tab, 0, "About");
                        ui.selectable_value(&mut self.model.about_tab, 1, "Copyright");
                    });
                    ui.separator();

                    match self.model.about_tab {
                        0 => {
                            ui.vertical_centered(|ui| {
                                ui.add(
                                    egui::Image::new(TSOD_LOGO)
                                        .max_width(180.0)
                                        .maintain_aspect_ratio(true),
                                );
                                ui.add_space(8.0);
                                ui.heading("TSOD");
                                ui.label(egui::RichText::new(format!("Version {BUILD_VERSION}")).monospace());
                            });
                            ui.add_space(10.0);
                            ui.label("TSOD is a low-latency, TeamSpeak- and Discord-inspired voice collaboration client built for reliable channels, rich chat, permissions-aware moderation, and high quality audio/video communication across modern desktop environments.");
                        }
                        _ => {
                            ui.label(
                                egui::RichText::new(
                                    "Third-party libraries and licenses used by this project:",
                                )
                                .small(),
                            );
                            ui.add_space(6.0);
                            egui::ScrollArea::vertical()
                                .auto_shrink([false; 2])
                                .show(ui, |ui| {
                                    egui::Grid::new("about_copyright_grid")
                                        .striped(true)
                                        .num_columns(3)
                                        .show(ui, |ui| {
                                            ui.strong("Library");
                                            ui.strong("Version");
                                            ui.strong("License");
                                            ui.end_row();

                                            for line in THIRD_PARTY_LICENSES.lines().skip(1) {
                                                let mut cols = line.splitn(3, '\t');
                                                let Some(name) = cols.next() else {
                                                    continue;
                                                };
                                                let Some(version) = cols.next() else {
                                                    continue;
                                                };
                                                let Some(license) = cols.next() else {
                                                    continue;
                                                };
                                                ui.label(name);
                                                ui.label(version);
                                                ui.label(license);
                                                ui.end_row();
                                            }
                                        });
                                });
                        }
                    }
                });
            if !open {
                self.model.show_about = false;
            }
        }

        // Settings window (floating, TS3-style Options dialog)
        if self.model.show_settings {
            let mut open = true;
            egui::Window::new("Options")
                .constrain(false)
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
                .constrain(false)
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
                .constrain(false)
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
                .constrain(false)
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
                        egui::ComboBox::from_id_salt("away_message_preset")
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
                .constrain(false)
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

        if self.model.show_share_content_dialog {
            let mut open = true;
            egui::Window::new("Share content")
                .constrain(false)
                .open(&mut open)
                .default_width(760.0)
                .min_width(680.0)
                .resizable(false)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Share content");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.toggle_value(&mut self.model.share_include_audio, "Include sound");
                        });
                    });

                    ui.add_space(12.0);
                    ui.label(egui::RichText::new("Presenter mode").strong().size(16.0));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        const MODES: [(&str, &str); 4] = [
                            ("🖵", "Content only"),
                            ("🧍", "Standout"),
                            ("🧩", "Side-by-side"),
                            ("👤", "Reporter"),
                        ];

                        for (idx, (icon, label)) in MODES.iter().enumerate() {
                            let selected = self.model.share_presenter_mode == idx;
                            let mut button =
                                egui::Button::new(egui::RichText::new(*icon).size(14.0).strong())
                                    .min_size(egui::vec2(40.0, 40.0))
                                    .corner_radius(6.0);
                            if selected {
                                button = button
                                    .fill(theme::bg_light())
                                    .stroke(egui::Stroke::new(1.5, theme::COLOR_ONLINE));
                            }
                            let response = ui.add(button);
                            if response.clicked() {
                                self.model.share_presenter_mode = idx;
                            }
                            response.on_hover_text(*label);
                        }
                    });

                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(8.0);

                    let screen_options: Vec<_> = self
                        .model
                        .share_sources
                        .iter()
                        .filter(|s| s.kind == model::ShareSourceKind::Screen)
                        .cloned()
                        .collect();
                    let window_options: Vec<_> = self
                        .model
                        .share_sources
                        .iter()
                        .filter(|s| s.kind == model::ShareSourceKind::Window)
                        .cloned()
                        .collect();

                    ui.label(egui::RichText::new("Screen").strong().size(16.0));
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        for source in &screen_options {
                            ui.add_space(4.0);
                            let _ = share_source_card(
                                ui,
                                &source.selection,
                                &source.title,
                                &source.subtitle,
                                &mut self.model.selected_share_source,
                            );
                            ui.add_space(4.0);
                        }
                    });

                    ui.add_space(14.0);
                    egui::CollapsingHeader::new(
                        egui::RichText::new(format!("Window ({})", window_options.len()))
                            .strong()
                            .size(16.0),
                    )
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.add_space(6.0);
                        egui::Frame::new()
                            .fill(theme::bg_light())
                            .corner_radius(6.0)
                            .inner_margin(egui::Margin::same(8))
                            .show(ui, |ui| {
                                egui::ScrollArea::both()
                                    .auto_shrink([false, false])
                                    .max_height(220.0)
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width().max(560.0));
                                        ui.horizontal_wrapped(|ui| {
                                            for source in &window_options {
                                                ui.add_space(4.0);
                                                let _ = share_source_card(
                                                    ui,
                                                    &source.selection,
                                                    &source.title,
                                                    &source.subtitle,
                                                    &mut self.model.selected_share_source,
                                                );
                                                ui.add_space(4.0);
                                            }
                                        });
                                    });
                            });
                    });

                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        let selected = self.model.selected_share_source.clone();
                        let share_label = "Start sharing";
                        let start_btn = ui.add_enabled(
                            selected.is_some(),
                            egui::Button::new(share_label)
                                .fill(theme::COLOR_ONLINE.linear_multiply(0.2))
                                .corner_radius(6.0),
                        );
                        if start_btn.clicked() {
                            if self.model.can_start_screen_share() {
                                if let Some(selection) = selected {
                                    self.model.start_share_in_flight = true;
                                    self.model.sharing_active = true;
                                    let _ = self
                                        .tx_intent
                                        .send(UiIntent::StartScreenShare { selection });
                                    self.model.show_share_content_dialog = false;
                                }
                            }
                        }

                        if ui.button("Stop sharing").clicked() {
                            self.model.start_share_in_flight = false;
                            self.model.sharing_active = false;
                            let _ = self.tx_intent.send(UiIntent::StopScreenShare);
                        }

                        if ui.button("Cancel").clicked() {
                            self.model.show_share_content_dialog = false;
                        }
                    });
                });
            if !open {
                self.model.show_share_content_dialog = false;
            }
        }

        // ── Profile popup (user card) ──
        if self.model.show_user_popup {
            // If we have no data yet, try to build a stub from the member list.
            if self.model.profile_popup_data.is_none() && self.model.profile_popup_loading {
                if let Some(uid) = self.model.profile_popup_user_id.clone() {
                    if let Some(stub) = self.model.stub_profile_from_member(&uid) {
                        self.model.profile_popup_data = Some(stub);
                    }
                }
            }

            let mut open = true;
            let anchor = self.model.profile_popup_anchor.unwrap_or(egui::pos2(300.0, 200.0));

            egui::Window::new("User Profile")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .fixed_size(egui::vec2(340.0, 420.0))
                .default_pos(anchor)
                .title_bar(false)
                .show(ctx, |ui| {
                    if let Some(profile) = &self.model.profile_popup_data {
                        // ── Banner area ──
                        let banner_height = 100.0;
                        let (banner_rect, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), banner_height),
                            egui::Sense::hover(),
                        );
                        // Accent colour gradient (fallback when no banner image).
                        let accent = if profile.accent_color != 0 {
                            let [_, r, g, b] = profile.accent_color.to_be_bytes();
                            egui::Color32::from_rgb(r, g, b)
                        } else {
                            theme::COLOR_ACCENT
                        };
                        let mesh = {
                            let mut mesh = egui::Mesh::default();
                            let top_color = accent;
                            let bot_color = theme::bg_dark();
                            let tl = banner_rect.left_top();
                            let tr = banner_rect.right_top();
                            let bl = banner_rect.left_bottom();
                            let br = banner_rect.right_bottom();
                            mesh.colored_vertex(tl, top_color);
                            mesh.colored_vertex(tr, top_color);
                            mesh.colored_vertex(br, bot_color);
                            mesh.colored_vertex(bl, bot_color);
                            mesh.add_triangle(0, 1, 2);
                            mesh.add_triangle(0, 2, 3);
                            mesh
                        };
                        ui.painter().add(egui::Shape::mesh(mesh));

                        // ── Avatar + identity row ──
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            // Avatar circle
                            let avatar_size = 64.0;
                            let (avatar_rect, _) = ui.allocate_exact_size(
                                egui::vec2(avatar_size, avatar_size),
                                egui::Sense::hover(),
                            );
                            let center = avatar_rect.center();
                            let radius = avatar_size * 0.5;
                            // Accent ring
                            ui.painter().circle_stroke(
                                center,
                                radius + 2.0,
                                egui::Stroke::new(3.0, accent),
                            );
                            // Background fill
                            ui.painter().circle_filled(center, radius, theme::bg_light());
                            // Initial letter
                            let initial = profile
                                .display_name
                                .chars()
                                .next()
                                .unwrap_or('?')
                                .to_uppercase()
                                .to_string();
                            ui.painter().text(
                                center,
                                egui::Align2::CENTER_CENTER,
                                &initial,
                                egui::FontId::proportional(28.0),
                                theme::text_color(),
                            );

                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(&profile.display_name)
                                        .size(18.0)
                                        .strong(),
                                );
                                let status_label = match profile.status {
                                    model::OnlineStatus::Online => ("Online", theme::COLOR_ONLINE),
                                    model::OnlineStatus::Idle => ("Idle", theme::COLOR_IDLE),
                                    model::OnlineStatus::DoNotDisturb => ("Do Not Disturb", theme::COLOR_DND),
                                    model::OnlineStatus::Invisible | model::OnlineStatus::Offline => ("Offline", theme::COLOR_OFFLINE),
                                };
                                ui.colored_label(status_label.1, status_label.0);
                            });
                        });

                        ui.separator();

                        // ── Activity row (conditional) ──
                        if let Some(activity) = &profile.current_activity {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("🎮").size(14.0));
                                ui.label(
                                    egui::RichText::new(format!("Playing {}", activity.game_name))
                                        .strong()
                                        .size(13.0),
                                );
                                if !activity.details.is_empty() {
                                    ui.label(
                                        egui::RichText::new(&activity.details)
                                            .size(12.0)
                                            .color(theme::text_dim()),
                                    );
                                }
                            });
                            ui.separator();
                        }

                        // ── Custom status (conditional) ──
                        if !profile.custom_status_text.is_empty() {
                            ui.horizontal(|ui| {
                                if !profile.custom_status_emoji.is_empty() {
                                    ui.label(&profile.custom_status_emoji);
                                }
                                ui.label(
                                    egui::RichText::new(&profile.custom_status_text)
                                        .size(13.0)
                                        .color(theme::text_dim()),
                                );
                            });
                            ui.separator();
                        }

                        // ── About Me ──
                        if !profile.description.is_empty() {
                            ui.label(
                                egui::RichText::new("ABOUT ME")
                                    .small()
                                    .strong()
                                    .color(theme::text_muted()),
                            );
                            ui.add_space(2.0);
                            let desc = if profile.description.len() > 190 {
                                format!("{}...", &profile.description[..187])
                            } else {
                                profile.description.clone()
                            };
                            ui.label(
                                egui::RichText::new(&desc)
                                    .size(13.0)
                                    .color(theme::text_color()),
                            );
                            ui.separator();
                        }

                        // ── Roles ──
                        if !profile.roles.is_empty() {
                            ui.label(
                                egui::RichText::new("ROLES")
                                    .small()
                                    .strong()
                                    .color(theme::text_muted()),
                            );
                            ui.add_space(2.0);
                            ui.horizontal_wrapped(|ui| {
                                let mut sorted_roles = profile.roles.clone();
                                sorted_roles.sort_by(|a, b| b.position.cmp(&a.position));
                                for role in &sorted_roles {
                                    let role_color = if role.color != 0 {
                                        let [_, r, g, b] = role.color.to_be_bytes();
                                        egui::Color32::from_rgb(r, g, b)
                                    } else {
                                        theme::text_dim()
                                    };
                                    let pill = egui::Frame::new()
                                        .fill(role_color.linear_multiply(0.2))
                                        .corner_radius(4.0)
                                        .inner_margin(egui::Margin::symmetric(6, 2));
                                    pill.show(ui, |ui| {
                                        ui.label(
                                            egui::RichText::new(&role.name)
                                                .size(12.0)
                                                .color(role_color),
                                        );
                                    });
                                }
                            });
                            ui.separator();
                        }

                        // ── Links ──
                        if !profile.links.is_empty() {
                            ui.label(
                                egui::RichText::new("LINKS")
                                    .small()
                                    .strong()
                                    .color(theme::text_muted()),
                            );
                            ui.add_space(2.0);
                            ui.horizontal_wrapped(|ui| {
                                for link in &profile.links {
                                    let icon = match link.platform.as_str() {
                                        "steam" => "🎮",
                                        "github" => "🐙",
                                        "twitter" | "x" => "🐦",
                                        "twitch" => "📺",
                                        "youtube" => "▶",
                                        _ => "🔗",
                                    };
                                    let label = if !link.display_text.is_empty() {
                                        &link.display_text
                                    } else {
                                        &link.platform
                                    };
                                    ui.label(
                                        egui::RichText::new(format!("{icon} {label}"))
                                            .size(12.0)
                                            .color(theme::COLOR_LINK),
                                    );
                                }
                            });
                            ui.separator();
                        }

                        // ── Badges ──
                        if !profile.badges.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                for badge in &profile.badges {
                                    let badge_label = egui::Label::new(
                                        egui::RichText::new(format!("🏷️ {}", badge.label))
                                            .size(12.0)
                                            .color(theme::text_dim()),
                                    );
                                    let response = ui.add(badge_label);
                                    if !badge.tooltip.is_empty() {
                                        response.on_hover_text(&badge.tooltip);
                                    }
                                }
                            });
                            ui.separator();
                        }

                        // ── Footer: member since + action buttons ──
                        if profile.created_at > 0 {
                            let secs = profile.created_at / 1000;
                            let naive = chrono::DateTime::from_timestamp(secs, 0);
                            if let Some(dt) = naive {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Member since {}",
                                        dt.format("%b %d, %Y")
                                    ))
                                    .small()
                                    .color(theme::text_muted()),
                                );
                            }
                        }

                        ui.add_space(4.0);
                        let profile_uid = profile.user_id.clone();
                        let profile_dname = profile.display_name.clone();
                        ui.horizontal(|ui| {
                            if ui
                                .add(
                                    egui::Button::new("Poke")
                                        .corner_radius(4.0),
                                )
                                .clicked()
                            {
                                self.model.show_poke_dialog = true;
                                self.model.poke_target_user_id = profile_uid.clone();
                                self.model.poke_target_display_name = profile_dname.clone();
                                self.model.poke_message_draft = "Poke".into();
                                self.model.show_user_popup = false;
                            }
                            if ui
                                .add(
                                    egui::Button::new("Connection Info")
                                        .corner_radius(4.0),
                                )
                                .clicked()
                            {
                                self.model.open_member_connection_info_window(
                                    profile_uid.clone(),
                                    profile_dname.clone(),
                                );
                                self.model.show_user_popup = false;
                            }
                        });
                    } else {
                        // Loading state
                        ui.vertical_centered(|ui| {
                            ui.add_space(80.0);
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("Loading profile…")
                                    .color(theme::text_muted()),
                            );
                        });
                    }
                });
            if !open {
                self.model.show_user_popup = false;
                self.model.profile_popup_user_id = None;
                self.model.profile_popup_data = None;
                self.model.profile_popup_loading = false;
            }
        }

        // Create channel dialog (floating)
        panels::server_tree::show_create_channel_dialog(ctx, &mut self.model, &self.tx_intent);
        panels::server_tree::show_channel_dialogs(ctx, &mut self.model, &self.tx_intent);
        panels::permissions_center::show_permissions_center(ctx, &mut self.model, &self.tx_intent);

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

            match self.model.current_channel_type() {
                Some(model::ChannelType::Streaming) => {
                    panels::streaming::show(ui, &mut self.model);
                }
                _ => {
                    panels::chat::show(ui, &mut self.model, &self.tx_intent);
                }
            };
        });

        // Handle keyboard shortcuts
        self.handle_shortcuts(ctx);
    }
}

impl VpApp {
    fn hotkey_pressed(ctx: &egui::Context, bind: Option<model::Keybind>) -> bool {
        let Some(bind) = bind else {
            return false;
        };
        ctx.input(|i| {
            i.key_pressed(bind.key)
                && i.modifiers.ctrl == bind.ctrl
                && i.modifiers.alt == bind.alt
                && i.modifiers.shift == bind.shift
                && i.modifiers.command == bind.command
        })
    }

    fn hotkey_released(ctx: &egui::Context, bind: Option<model::Keybind>) -> bool {
        let Some(bind) = bind else {
            return false;
        };
        ctx.input(|i| i.key_released(bind.key))
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let esc_pressed = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let ptt_pressed = Self::hotkey_pressed(ctx, self.model.settings.hotkeys.ptt);
        let ptt_released = Self::hotkey_released(ctx, self.model.settings.hotkeys.ptt);
        let mute_pressed = Self::hotkey_pressed(ctx, self.model.settings.hotkeys.toggle_mute);
        let deafen_pressed = Self::hotkey_pressed(ctx, self.model.settings.hotkeys.toggle_deafen);

        // PTT: down = talk, up handled by backend with release delay.
        if self.model.ptt_enabled {
            if ptt_pressed && !self.model.chat_input_focused {
                let _ = self.tx_intent.send(UiIntent::PttDown);
                self.model.ptt_active = true;
            }
            if ptt_released && !self.model.chat_input_focused {
                let _ = self.tx_intent.send(UiIntent::PttUp);
            }
        }

        if mute_pressed && !self.model.chat_input_focused {
            self.model.self_muted = !self.model.self_muted;
            let _ = self.tx_intent.send(UiIntent::ToggleSelfMute);
        }

        if deafen_pressed && !self.model.chat_input_focused {
            self.model.self_deafened = !self.model.self_deafened;
            let _ = self.tx_intent.send(UiIntent::ToggleSelfDeafen);
        }

        if esc_pressed {
            if self.model.show_user_popup {
                self.model.show_user_popup = false;
                self.model.profile_popup_user_id = None;
                self.model.profile_popup_data = None;
                self.model.profile_popup_loading = false;
            } else {
                self.model.show_settings = false;
                self.model.show_telemetry = false;
                self.model.show_connections = false;
                self.model.show_create_channel = false;
            }
        }
    }
}
