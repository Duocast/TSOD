//! User panel at the bottom of the left sidebar: avatar, name, mute/deafen/settings buttons.

use crate::ui::model::{OnlineStatus, UiIntent, UiModel};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

/// Renders the user panel as a self-contained vertical section.
/// Designed to sit at the bottom of the left sidebar panel.
pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    let panel_width = ui.available_width();
    let card_bg = egui::Color32::from_rgb(29, 34, 44);
    let control_bg = egui::Color32::from_rgb(58, 66, 84);
    let control_hover = egui::Color32::from_rgb(70, 78, 98);
    let control_active = egui::Color32::from_rgb(83, 92, 114);

    egui::Frame::new()
        .fill(card_bg)
        .inner_margin(egui::Margin::same(16))
        .corner_radius(16.0)
        .shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: egui::Color32::from_black_alpha(120),
        })
        .show(ui, |ui: &mut egui::Ui| {
            ui.set_min_width(panel_width - 8.0);

            // ── Top section: avatar + identity/status block ───────────────────
            ui.horizontal(|ui: &mut egui::Ui| {
                let preferred_name = model
                    .self_profile
                    .as_ref()
                    .map(|p| p.display_name.clone())
                    .filter(|name| !should_prefer_fallback_name(name))
                    .unwrap_or_else(|| model.nick.clone());
                let initials = initials_from_name(&preferred_name);

                let status_color = online_status_color(model);

                let avatar_size = egui::vec2(68.0, 68.0);
                let (rect, response) = ui.allocate_exact_size(avatar_size, egui::Sense::click());

                ui.painter().circle_filled(
                    rect.center(),
                    32.0,
                    egui::Color32::from_rgb(238, 94, 55),
                );

                if let Some(avatar_url) = &model.avatar_url {
                    let image_rect =
                        egui::Rect::from_center_size(rect.center(), egui::vec2(64.0, 64.0));
                    ui.put(
                        image_rect,
                        egui::Image::from_uri(avatar_url)
                            .fit_to_exact_size(egui::vec2(64.0, 64.0))
                            .corner_radius(32.0),
                    );
                } else {
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        &initials,
                        egui::FontId::proportional(30.0),
                        egui::Color32::WHITE,
                    );
                }

                // Status indicator badge
                let dot_pos = rect.center() + egui::vec2(21.0, 21.0);
                ui.painter()
                    .circle_filled(dot_pos, 10.0, egui::Color32::from_rgb(23, 28, 36));
                ui.painter().circle_filled(dot_pos, 7.0, status_color);

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

                // ── Name / status / activity column (clickable) ─────────
                let name_col_width = ui.available_width().max(40.0);
                ui.vertical(|ui: &mut egui::Ui| {
                    ui.set_max_width(name_col_width);

                    let display_name = model
                        .self_profile
                        .as_ref()
                        .map(|p| p.display_name.clone())
                        .filter(|name| !should_prefer_fallback_name(name))
                        .unwrap_or_else(|| model.nick.clone());

                    let status_text = build_status_text(model);
                    let activity_text = model
                        .self_profile
                        .as_ref()
                        .and_then(|p| p.current_activity.as_ref())
                        .map(|a| format!("Playing {}", a.game_name));

                    let mut block = egui::text::LayoutJob::default();
                    block.append(
                        &display_name,
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(20.0),
                            color: egui::Color32::WHITE,
                            ..Default::default()
                        },
                    );
                    block.append(
                        "\n✨  ",
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(15.0),
                            color: egui::Color32::from_rgb(247, 203, 79),
                            ..Default::default()
                        },
                    );
                    block.append(
                        &status_text,
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(15.0),
                            color: egui::Color32::from_rgb(231, 236, 242),
                            ..Default::default()
                        },
                    );

                    if let Some(activity_text) = activity_text {
                        block.append(
                            "\n🎮  ",
                            0.0,
                            egui::TextFormat {
                                font_id: egui::FontId::proportional(15.0),
                                color: egui::Color32::from_rgb(163, 108, 255),
                                ..Default::default()
                            },
                        );
                        block.append(
                            &activity_text,
                            0.0,
                            egui::TextFormat {
                                font_id: egui::FontId::proportional(15.0),
                                color: egui::Color32::from_rgb(207, 214, 225),
                                ..Default::default()
                            },
                        );
                    }

                    let status_resp = ui.add(
                        egui::Label::new(block)
                            .sense(egui::Sense::click())
                            .selectable(false),
                    );
                    if status_resp.clicked() {
                        if let Some(ref p) = model.self_profile {
                            model.custom_status_text_draft = p.custom_status_text.clone();
                            model.custom_status_emoji_draft = p.custom_status_emoji.clone();
                        }
                        model.show_custom_status_popover = !model.show_custom_status_popover;
                    }
                    status_resp.on_hover_text("Edit display name and status");
                });
            });

            ui.add_space(12.0);

            let divider_rect = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover())
                .0;
            ui.painter().line_segment(
                [divider_rect.left_center(), divider_rect.right_center()],
                egui::Stroke::new(1.0, egui::Color32::from_white_alpha(20)),
            );

            ui.add_space(12.0);

            // ── Bottom section: controls ─────────────────────────────────
            let in_voice_channel = model.active_voice_channel_route != 0;

            ui.horizontal(|ui: &mut egui::Ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let btn_size = egui::vec2(54.0, 54.0);

                // Away
                let away_btn = circle_icon_button(
                    ui,
                    "🌙",
                    egui::Color32::from_rgb(242, 204, 81),
                    control_bg,
                    control_hover,
                    control_active,
                    btn_size,
                );
                if away_btn.clicked() {
                    model.show_away_message_dialog = true;
                    model.away_message_draft = model.away_message.clone();
                }
                away_btn.on_hover_text("Set Away Message");

                // Mute
                let mute_label = if model.self_muted { "Unmute" } else { "Mute" };
                let mute_color = if model.self_muted {
                    theme::COLOR_DANGER
                } else {
                    egui::Color32::from_rgb(234, 238, 244)
                };

                let mute_btn = ui.add_enabled_ui(in_voice_channel, |ui| {
                    circle_icon_button(
                        ui,
                        "🎤",
                        mute_color,
                        control_bg,
                        control_hover,
                        control_active,
                        btn_size,
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

                // Deafen
                let deafen_label = if model.self_deafened {
                    "Undeafen"
                } else {
                    "Deafen"
                };
                let deafen_text_color = if model.self_deafened {
                    theme::COLOR_DANGER
                } else {
                    egui::Color32::from_rgb(234, 238, 244)
                };
                let deafen_icon = if model.self_deafened { "🔈" } else { "🔊" };

                let deafen_btn = ui.add_enabled_ui(in_voice_channel, |ui| {
                    circle_icon_button(
                        ui,
                        deafen_icon,
                        deafen_text_color,
                        control_bg,
                        control_hover,
                        control_active,
                        btn_size,
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

                // Screen share
                let share_text_color = if model.sharing_active {
                    egui::Color32::from_rgb(140, 196, 255)
                } else {
                    egui::Color32::from_rgb(234, 238, 244)
                };

                let share_btn = ui.add_enabled_ui(in_voice_channel, |ui| {
                    circle_icon_button(
                        ui,
                        "🖥",
                        share_text_color,
                        control_bg,
                        control_hover,
                        control_active,
                        btn_size,
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

fn should_prefer_fallback_name(name: &str) -> bool {
    let trimmed = name.trim();
    trimmed.is_empty() || trimmed.starts_with("guest-") || trimmed.starts_with("user-")
}

fn initials_from_name(name: &str) -> String {
    let mut initials = String::new();
    for part in name.split_whitespace().filter(|p| !p.is_empty()).take(2) {
        if let Some(ch) = part.chars().next() {
            for upper in ch.to_uppercase() {
                initials.push(upper);
            }
        }
    }

    if initials.is_empty() {
        name.chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .for_each(|ch| initials.push(ch));
    }

    initials
}

fn circle_icon_button(
    ui: &mut egui::Ui,
    icon: &str,
    color: egui::Color32,
    fill: egui::Color32,
    hover: egui::Color32,
    active: egui::Color32,
    size: egui::Vec2,
) -> egui::Response {
    ui.scope(|ui| {
        let visuals = &mut ui.style_mut().visuals.widgets;
        visuals.inactive.bg_fill = fill;
        visuals.hovered.bg_fill = hover;
        visuals.active.bg_fill = active;
        visuals.inactive.weak_bg_fill = fill;
        visuals.hovered.weak_bg_fill = hover;
        visuals.active.weak_bg_fill = active;

        ui.add_sized(
            size,
            egui::Button::new(egui::RichText::new(icon).size(22.0).color(color).strong())
                .stroke(egui::Stroke::NONE)
                .corner_radius(size.x * 0.5),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
    })
    .inner
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
