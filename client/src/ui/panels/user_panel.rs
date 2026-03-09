//! User panel at the bottom of the left sidebar: avatar, name, mute/deafen/settings buttons.

use crate::ui::model::{UiIntent, UiModel};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

/// Renders the user panel as a self-contained vertical section.
/// Designed to sit at the bottom of the left sidebar panel.
pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    let panel_width = ui.available_width();

    // Background frame for the entire user panel
    egui::Frame::new()
        .fill(theme::bg_dark())
        .inner_margin(8.0)
        .outer_margin(egui::Margin::ZERO)
        .corner_radius(0.0)
        .show(ui, |ui: &mut egui::Ui| {
            ui.set_min_width(panel_width - 16.0);

            // Top row: avatar + name + status
            ui.horizontal(|ui: &mut egui::Ui| {
                // Clickable avatar button
                let initial = model
                    .nick
                    .chars()
                    .next()
                    .unwrap_or('?')
                    .to_uppercase()
                    .to_string();
                let status_color = if model.connected {
                    theme::COLOR_ONLINE
                } else {
                    theme::COLOR_OFFLINE
                };

                // Draw avatar as a proper button
                let avatar_size = egui::vec2(36.0, 36.0);
                let (rect, response) = ui.allocate_exact_size(avatar_size, egui::Sense::click());

                // Avatar circle background
                let bg_color = if response.hovered() {
                    theme::bg_input()
                } else {
                    theme::bg_light()
                };
                ui.painter().circle_filled(rect.center(), 16.0, bg_color);

                if let Some(avatar_url) = &model.avatar_url {
                    let image_rect =
                        egui::Rect::from_center_size(rect.center(), egui::vec2(30.0, 30.0));
                    ui.put(
                        image_rect,
                        egui::Image::from_uri(avatar_url).fit_to_exact_size(egui::vec2(30.0, 30.0)),
                    );
                } else {
                    // Initial letter
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        &initial,
                        egui::FontId::proportional(15.0),
                        theme::text_color(),
                    );
                }

                // Status indicator dot
                let dot_pos = rect.center() + egui::vec2(10.0, 10.0);
                ui.painter().circle_filled(dot_pos, 6.0, theme::bg_dark());
                ui.painter().circle_filled(dot_pos, 4.0, status_color);

                if response.clicked() {
                    // Open the full profile edit modal on the Avatar tab.
                    crate::ui::panels::profile_edit::init_draft_from_profile(model);
                    model.edit_profile_tab = crate::ui::model::ProfileEditTab::Avatar;
                    model.show_edit_profile = true;
                    // Request self profile if not yet loaded.
                    if model.self_profile.is_none() {
                        let _ = tx_intent.send(UiIntent::FetchSelfProfile);
                    }
                }
                response.on_hover_text("Edit Profile");

                // Name and status text
                ui.vertical(|ui: &mut egui::Ui| {
                    let display_name = model
                        .self_profile
                        .as_ref()
                        .map(|p| p.display_name.clone())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| model.nick.clone());
                    ui.label(
                        egui::RichText::new(&display_name)
                            .strong()
                            .size(13.0)
                            .color(theme::text_color()),
                    );

                    // Clickable status/custom status area
                    let status_text = build_status_text(model);
                    let status_resp = ui.add(
                        egui::Label::new(
                            egui::RichText::new(&status_text)
                                .size(11.0)
                                .color(theme::text_muted()),
                        )
                        .sense(egui::Sense::click()),
                    );
                    if status_resp.clicked() {
                        // Open custom status popover
                        if let Some(ref p) = model.self_profile {
                            model.custom_status_text_draft = p.custom_status_text.clone();
                            model.custom_status_emoji_draft = p.custom_status_emoji.clone();
                        }
                        model.show_custom_status_popover = !model.show_custom_status_popover;
                    }
                    status_resp.on_hover_text("Set custom status");
                });
            });

            ui.add_space(6.0);

            let in_voice_channel = model.active_voice_channel_route != 0;
            let _voice_state = if !model.connected {
                ("Voice: disconnected", theme::COLOR_OFFLINE)
            } else if in_voice_channel {
                ("Voice: connected", theme::COLOR_ONLINE)
            } else {
                ("Voice: not in voice channel", theme::text_muted())
            };
            // Keep voice status and controls on one row so controls remain visible.
            ui.horizontal(|ui: &mut egui::Ui| {
                let btn_size = egui::vec2(30.0, 24.0);

                let away_btn = ui.add_sized(
                    btn_size,
                    egui::Button::new(
                        egui::RichText::new("🌙")
                            .size(13.0)
                            .color(theme::text_color())
                            .strong(),
                    )
                    .fill(theme::bg_light())
                    .corner_radius(4.0),
                );
                if away_btn.clicked() {
                    model.show_away_message_dialog = true;
                    model.away_message_draft = model.away_message.clone();
                }
                away_btn.on_hover_text("Set Away Message");

                ui.add_space(2.0);

                // Mute button
                let mute_label = if model.self_muted { "Unmute" } else { "Mute" };
                let mute_fill = if model.self_muted {
                    theme::COLOR_DANGER.linear_multiply(0.3)
                } else {
                    theme::bg_light()
                };
                let mute_text_color = if model.self_muted {
                    theme::COLOR_DANGER
                } else {
                    theme::text_color()
                };
                let mute_icon = "🎤";

                let mute_btn = ui.add_enabled_ui(in_voice_channel, |ui| {
                    ui.add_sized(
                        btn_size,
                        egui::Button::new(
                            egui::RichText::new(mute_icon)
                                .size(12.0)
                                .color(mute_text_color)
                                .strong(),
                        )
                        .fill(mute_fill)
                        .corner_radius(4.0),
                    )
                });
                let mute_btn = mute_btn.inner;
                if mute_btn.clicked() {
                    model.self_muted = !model.self_muted;
                    let _ = tx_intent.send(UiIntent::ToggleSelfMute);
                }
                mute_btn.on_hover_text(if in_voice_channel {
                    mute_label
                } else {
                    "Join a voice channel to use mute"
                });

                ui.add_space(2.0);

                // Deafen button
                let deafen_label = if model.self_deafened {
                    "Undeafen"
                } else {
                    "Deafen"
                };
                let deafen_fill = if model.self_deafened {
                    theme::COLOR_DANGER.linear_multiply(0.3)
                } else {
                    theme::bg_light()
                };
                let deafen_text_color = if model.self_deafened {
                    theme::COLOR_DANGER
                } else {
                    theme::text_color()
                };
                let deafen_icon = if model.self_deafened { "🔇" } else { "🔊" };

                let deafen_btn = ui.add_enabled_ui(in_voice_channel, |ui| {
                    ui.add_sized(
                        btn_size,
                        egui::Button::new(
                            egui::RichText::new(deafen_icon)
                                .size(12.0)
                                .color(deafen_text_color)
                                .strong(),
                        )
                        .fill(deafen_fill)
                        .corner_radius(4.0),
                    )
                });
                let deafen_btn = deafen_btn.inner;
                if deafen_btn.clicked() {
                    model.self_deafened = !model.self_deafened;
                    let _ = tx_intent.send(UiIntent::ToggleSelfDeafen);
                }
                deafen_btn.on_hover_text(if in_voice_channel {
                    deafen_label
                } else {
                    "Join a voice channel to use deafen"
                });

                ui.add_space(2.0);

                let share_fill = if model.sharing_active {
                    theme::COLOR_ONLINE.linear_multiply(0.25)
                } else {
                    theme::bg_light()
                };
                let share_text_color = if model.sharing_active {
                    theme::COLOR_ONLINE
                } else {
                    theme::text_color()
                };

                let share_btn = ui.add_enabled_ui(in_voice_channel, |ui| {
                    ui.add_sized(
                        btn_size,
                        egui::Button::new(
                            egui::RichText::new("🖥")
                                .size(12.0)
                                .color(share_text_color)
                                .strong(),
                        )
                        .fill(share_fill)
                        .corner_radius(4.0),
                    )
                });
                let share_btn = share_btn.inner;
                if share_btn.clicked() {
                    model.share_sources = crate::ui::model::enumerate_share_sources();
                    if model
                        .selected_share_source
                        .as_ref()
                        .is_some_and(|selected| {
                            !model
                                .share_sources
                                .iter()
                                .any(|source| &source.selection == selected)
                        })
                    {
                        model.selected_share_source = None;
                    }
                    model.show_share_content_dialog = true;
                }
                share_btn.on_hover_text(if in_voice_channel {
                    if model.sharing_active {
                        "Change shared content"
                    } else {
                        "Share screen or window"
                    }
                } else {
                    "Join a voice channel to share"
                });
            });

            // VAD level bar (when voice is active)
            if let Some(vad) = model.vad_level {
                ui.add_space(4.0);
                let bar_width = ui.available_width();
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(bar_width, 4.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 2.0, theme::bg_medium());
                let filled_width = bar_width * vad;
                let filled = egui::Rect::from_min_size(rect.min, egui::vec2(filled_width, 4.0));
                let color = if vad > 0.5 {
                    theme::COLOR_ONLINE
                } else {
                    theme::COLOR_IDLE
                };
                ui.painter().rect_filled(filled, 2.0, color);
            }
        });
}

fn build_status_text(model: &crate::ui::model::UiModel) -> String {
    // Show custom status if set.
    if let Some(ref p) = model.self_profile {
        let mut parts = String::new();
        if !p.custom_status_emoji.is_empty() {
            parts.push_str(&p.custom_status_emoji);
            parts.push(' ');
        }
        if !p.custom_status_text.is_empty() {
            parts.push_str(&p.custom_status_text);
            return parts;
        }
    }
    // Fall back to away message or connection state.
    if !model.away_message.is_empty() {
        return format!("Away: {}", model.away_message);
    }
    if model.connected {
        "Online".to_string()
    } else {
        "Offline".to_string()
    }
}
