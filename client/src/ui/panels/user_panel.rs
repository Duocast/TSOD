//! User panel at the bottom of the left sidebar: avatar, name, mute/deafen/settings buttons.

use crate::ui::model::{OnlineStatus, UiIntent, UiModel};
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

            // ── Top row: avatar + name/status/activity + settings button ──
            ui.horizontal(|ui: &mut egui::Ui| {
                let initial = model
                    .nick
                    .chars()
                    .next()
                    .unwrap_or('?')
                    .to_uppercase()
                    .to_string();

                let status_color = online_status_color(model);

                // Accent ring color from profile, fallback to theme accent.
                let accent = model
                    .self_profile
                    .as_ref()
                    .map(|p| accent_to_color32(p.accent_color))
                    .unwrap_or(theme::COLOR_ACCENT);

                // ── Avatar (40×40) with accent ring + circular clip ──────
                let avatar_size = egui::vec2(40.0, 40.0);
                let (rect, response) =
                    ui.allocate_exact_size(avatar_size, egui::Sense::click());

                // Accent color ring (outermost)
                ui.painter().circle_filled(rect.center(), 19.0, accent);

                // Dark gap between ring and avatar
                ui.painter()
                    .circle_filled(rect.center(), 17.5, theme::bg_dark());

                // Avatar background circle
                let bg_color = if response.hovered() {
                    theme::bg_input()
                } else {
                    theme::bg_light()
                };
                ui.painter().circle_filled(rect.center(), 17.0, bg_color);

                if let Some(avatar_url) = &model.avatar_url {
                    let image_rect =
                        egui::Rect::from_center_size(rect.center(), egui::vec2(34.0, 34.0));
                    ui.put(
                        image_rect,
                        egui::Image::from_uri(avatar_url)
                            .fit_to_exact_size(egui::vec2(34.0, 34.0))
                            .corner_radius(17.0),
                    );
                } else {
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        &initial,
                        egui::FontId::proportional(15.0),
                        theme::text_color(),
                    );
                }

                // Status indicator dot (bottom-right of avatar)
                let dot_pos = rect.center() + egui::vec2(12.0, 12.0);
                ui.painter()
                    .circle_filled(dot_pos, 6.0, theme::bg_dark());
                ui.painter().circle_filled(dot_pos, 4.0, status_color);

                // Left-click: open profile edit modal on the Avatar tab.
                if response.clicked() {
                    crate::ui::panels::profile_edit::init_draft_from_profile(model);
                    model.edit_profile_tab = crate::ui::model::ProfileEditTab::Avatar;
                    model.show_edit_profile = true;
                    if model.self_profile.is_none() {
                        let _ = tx_intent.send(UiIntent::FetchSelfProfile);
                    }
                }

                // Right-click context menu — use local flags to avoid nested
                // mutable-borrow conflict with `model`.  Chain on_hover_text
                // and context_menu since both consume `response` by value.
                let user_id_for_copy = model.user_id.clone();
                let mut do_edit_profile = false;
                let mut do_set_status = false;

                response.on_hover_text("Edit Profile").context_menu(|ui| {
                    if ui.button("Edit Profile").clicked() {
                        do_edit_profile = true;
                        ui.close();
                    }
                    if ui.button("Set Status").clicked() {
                        do_set_status = true;
                        ui.close();
                    }
                    if ui.button("Copy User ID").clicked() {
                        ui.ctx().copy_text(user_id_for_copy.clone());
                        ui.close();
                    }
                });

                if do_edit_profile {
                    crate::ui::panels::profile_edit::init_draft_from_profile(model);
                    model.edit_profile_tab = crate::ui::model::ProfileEditTab::Profile;
                    model.show_edit_profile = true;
                    if model.self_profile.is_none() {
                        let _ = tx_intent.send(UiIntent::FetchSelfProfile);
                    }
                }
                if do_set_status {
                    if let Some(ref p) = model.self_profile {
                        model.custom_status_text_draft = p.custom_status_text.clone();
                        model.custom_status_emoji_draft = p.custom_status_emoji.clone();
                    }
                    model.show_custom_status_popover = !model.show_custom_status_popover;
                }

                // ── Name / status / activity column ─────────────────────
                // Reserve space for the settings button (≈26 px) on the right.
                let name_col_width = (ui.available_width() - 28.0).max(40.0);
                ui.vertical(|ui: &mut egui::Ui| {
                    ui.set_max_width(name_col_width);

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

                    // Clickable status / custom-status area → opens popover.
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
                        if let Some(ref p) = model.self_profile {
                            model.custom_status_text_draft = p.custom_status_text.clone();
                            model.custom_status_emoji_draft = p.custom_status_emoji.clone();
                        }
                        model.show_custom_status_popover = !model.show_custom_status_popover;
                    }
                    status_resp.on_hover_text("Set custom status");

                    // Activity row (conditional) — shown when the user is
                    // currently running a tracked game.
                    if let Some(activity) = model
                        .self_profile
                        .as_ref()
                        .and_then(|p| p.current_activity.as_ref())
                    {
                        let activity_text = format!("🎮 {}", activity.game_name);
                        ui.label(
                            egui::RichText::new(&activity_text)
                                .size(11.0)
                                .color(theme::text_muted()),
                        );
                    }
                });

                // ── Settings button (top-right of info row) ─────────────
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let settings_btn = ui.add(
                        egui::Button::new(
                            egui::RichText::new("⚙")
                                .size(14.0)
                                .color(theme::text_dim()),
                        )
                        .fill(egui::Color32::TRANSPARENT)
                        .frame(false)
                        .min_size(egui::vec2(22.0, 22.0)),
                    );
                    if settings_btn.clicked() {
                        model.show_settings = true;
                    }
                    settings_btn.on_hover_text("Settings");
                });
            });

            ui.add_space(6.0);

            // ── Bottom row: voice controls + settings button ─────────────
            let in_voice_channel = model.active_voice_channel_route != 0;

            ui.horizontal(|ui: &mut egui::Ui| {
                let btn_size = egui::vec2(28.0, 24.0);

                // Away
                let away_btn = ui.add_sized(
                    btn_size,
                    egui::Button::new(
                        egui::RichText::new("🌙")
                            .size(12.0)
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

                // Mute
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

                let mute_btn = ui.add_enabled_ui(in_voice_channel, |ui| {
                    ui.add_sized(
                        btn_size,
                        egui::Button::new(
                            egui::RichText::new("🎤")
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

                // Deafen
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

                // Screen share
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

                ui.add_space(2.0);

                // Settings button (⚙️) in the voice controls row
                let settings_btn = ui.add_sized(
                    btn_size,
                    egui::Button::new(
                        egui::RichText::new("⚙")
                            .size(12.0)
                            .color(theme::text_color())
                            .strong(),
                    )
                    .fill(theme::bg_light())
                    .corner_radius(4.0),
                );
                if settings_btn.clicked() {
                    model.show_settings = true;
                }
                settings_btn.on_hover_text("Settings");
            });

            // VAD level bar (when voice is active)
            if let Some(vad) = model.vad_level {
                ui.add_space(4.0);
                let bar_width = ui.available_width();
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(bar_width, 4.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 2.0, theme::bg_medium());
                let filled_width = bar_width * vad;
                let filled =
                    egui::Rect::from_min_size(rect.min, egui::vec2(filled_width, 4.0));
                let color = if vad > 0.5 {
                    theme::COLOR_ONLINE
                } else {
                    theme::COLOR_IDLE
                };
                ui.painter().rect_filled(filled, 2.0, color);
            }
        });
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Map the user's OnlineStatus (from their profile) to a status-dot color.
/// Falls back to connected/disconnected if no profile is loaded yet.
fn online_status_color(model: &UiModel) -> egui::Color32 {
    if let Some(ref p) = model.self_profile {
        return match p.status {
            OnlineStatus::Online => theme::COLOR_ONLINE,
            OnlineStatus::Idle => theme::COLOR_IDLE,
            OnlineStatus::DoNotDisturb => theme::COLOR_DND,
            OnlineStatus::Invisible | OnlineStatus::Offline => theme::COLOR_OFFLINE,
        };
    }
    if model.connected {
        theme::COLOR_ONLINE
    } else {
        theme::COLOR_OFFLINE
    }
}

/// Convert a packed `0xRRGGBB` accent color to `egui::Color32`.
/// Returns the theme accent color when the value is zero (unset).
fn accent_to_color32(argb: u32) -> egui::Color32 {
    if argb == 0 {
        return theme::COLOR_ACCENT;
    }
    let r = ((argb >> 16) & 0xFF) as u8;
    let g = ((argb >> 8) & 0xFF) as u8;
    let b = (argb & 0xFF) as u8;
    egui::Color32::from_rgb(r, g, b)
}

fn build_status_text(model: &UiModel) -> String {
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
