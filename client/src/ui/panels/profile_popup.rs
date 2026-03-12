use crate::ui::markdown;
use crate::ui::model::{OnlineStatus, UiIntent, UiModel, UserProfileData};
use crate::ui::theme;
use chrono::{Local, TimeZone};
use crossbeam_channel::Sender;
use eframe::egui;
use std::time::Duration;

const POPUP_WIDTH: f32 = 420.0;
const POPUP_HEIGHT: f32 = 580.0;
const POPUP_SIZE: egui::Vec2 = egui::vec2(POPUP_WIDTH, POPUP_HEIGHT);
const BANNER_HEIGHT: f32 = 100.0;
const AVATAR_SIZE: f32 = 84.0;
const AVATAR_HALF: f32 = AVATAR_SIZE / 2.0;
const CARD_ROUNDING: f32 = 14.0;
const CONTENT_PAD: f32 = 20.0;
const BADGE_CROWN: &[u8] = include_bytes!("../../../assets/Badges/24_crown.png");
const BADGE_SHIELD: &[u8] = include_bytes!("../../../assets/Badges/14_shield.png");
const BADGE_CODE: &[u8] = include_bytes!("../../../assets/Badges/07_code.png");
const BADGE_TROPHY: &[u8] = include_bytes!("../../../assets/Badges/23_trophy.png");
const BADGE_LEVEL_UP: &[u8] = include_bytes!("../../../assets/Badges/40_level_up.png");

/// Badge definitions: (badge_id, asset_path_for_server)
const BADGE_DEFS: &[(&str, &str)] = &[
    ("staff", "client/assets/Badges/24_crown.png"),
    ("admin", "client/assets/Badges/24_crown.png"),
    ("mod", "client/assets/Badges/14_shield.png"),
    ("developer", "client/assets/Badges/07_code.png"),
    ("founder", "client/assets/Badges/23_trophy.png"),
    ("early-adopter", "client/assets/Badges/40_level_up.png"),
];

pub fn show(ctx: &egui::Context, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    let Some(user_id) = model.profile_popup_user_id.clone() else {
        return;
    };

    let mut open = true;
    let anchor = model
        .profile_popup_anchor
        .unwrap_or_else(|| ctx.available_rect().center());
    let pos = egui::pos2(anchor.x + 8.0, anchor.y + 8.0);

    let mut popup_rect = egui::Rect::NOTHING;
    egui::Window::new("profile_popup")
        .id(egui::Id::new("profile_popup_window"))
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .movable(false)
        .constrain(true)
        .fixed_pos(pos)
        .fixed_size(POPUP_SIZE)
        .frame(
            egui::Frame::window(&ctx.style())
                .fill(egui::Color32::from_rgb(24, 25, 34))
                .corner_radius(CARD_ROUNDING)
                .inner_margin(egui::Margin::ZERO),
        )
        .open(&mut open)
        .show(ctx, |ui| {
            popup_rect = ui.max_rect();
            ui.set_min_size(POPUP_SIZE);
            if let Some(profile) = model
                .profile_popup_data
                .as_ref()
                .filter(|p| p.user_id == user_id)
                .cloned()
            {
                render_profile(ui, model, tx_intent, &profile);
            } else if model.profile_popup_loading {
                ui.centered_and_justified(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("Loading profile...").color(theme::text_muted()));
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("Profile unavailable").color(theme::text_muted()));
                });
            }
        });

    let esc_pressed = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    let clicked_outside = ctx.input(|i| {
        i.pointer.primary_pressed()
            && i.pointer
                .interact_pos()
                .is_some_and(|pos| popup_rect != egui::Rect::NOTHING && !popup_rect.contains(pos))
    });

    if !open || esc_pressed || clicked_outside {
        model.profile_popup_user_id = None;
        model.profile_popup_data = None;
        model.profile_popup_loading = false;
        model.profile_popup_anchor = None;
    }
}

fn render_profile(
    ui: &mut egui::Ui,
    model: &mut UiModel,
    tx_intent: &Sender<UiIntent>,
    profile: &UserProfileData,
) {
    let accent = u32_to_color(profile.accent_color);
    let card_rect = ui.max_rect();

    // -- Background --
    paint_vertical_gradient(
        ui,
        card_rect,
        egui::Color32::from_rgb(24, 25, 34),
        egui::Color32::from_rgb(20, 21, 30),
        egui::CornerRadius::same(CARD_ROUNDING as u8),
    );

    // -- Banner --
    let banner_rect =
        egui::Rect::from_min_size(card_rect.min, egui::vec2(card_rect.width(), BANNER_HEIGHT));
    let clip_rounding = egui::CornerRadius {
        nw: CARD_ROUNDING as u8,
        ne: CARD_ROUNDING as u8,
        sw: 0,
        se: 0,
    };

    if let Some(url) = profile.banner_url.as_ref().filter(|u| !u.is_empty()) {
        ui.put(
            banner_rect,
            crate::ui::image_from_source(url)
                .fit_to_exact_size(banner_rect.size())
                .corner_radius(clip_rounding),
        );
    } else {
        paint_vertical_gradient(
            ui,
            banner_rect,
            lerp_color(accent, egui::Color32::from_rgb(84, 124, 243), 0.4),
            egui::Color32::from_rgb(30, 33, 55),
            clip_rounding,
        );
        paint_horizontal_tint(
            ui,
            banner_rect,
            accent,
            egui::Color32::from_rgb(128, 79, 212),
        );
    }

    // -- Avatar (overlapping banner) --
    let avatar_center = egui::pos2(
        card_rect.left() + CONTENT_PAD + AVATAR_HALF,
        banner_rect.bottom(),
    );

    // Outer ring (card background color to cut out from banner)
    ui.painter().circle_filled(
        avatar_center,
        AVATAR_HALF + 5.0,
        egui::Color32::from_rgb(24, 25, 34),
    );
    // Avatar fill
    ui.painter().circle_filled(
        avatar_center,
        AVATAR_HALF,
        egui::Color32::from_rgb(227, 102, 47),
    );

    let avatar_rect =
        egui::Rect::from_center_size(avatar_center, egui::vec2(AVATAR_SIZE, AVATAR_SIZE));
    if let Some(url) = profile.avatar_url.as_ref().filter(|u| !u.is_empty()) {
        ui.put(
            avatar_rect,
            crate::ui::image_from_source(url)
                .fit_to_exact_size(avatar_rect.size())
                .corner_radius(egui::CornerRadius::same(AVATAR_HALF as u8)),
        );
    } else {
        let fallback = profile
            .display_name
            .chars()
            .take(2)
            .collect::<String>()
            .to_uppercase();
        ui.painter().text(
            avatar_center,
            egui::Align2::CENTER_CENTER,
            fallback,
            egui::FontId::proportional(34.0),
            egui::Color32::WHITE,
        );
    }

    // -- Status indicator --
    let status_center = egui::pos2(avatar_rect.right() - 6.0, avatar_rect.bottom() - 6.0);
    ui.painter()
        .circle_filled(status_center, 12.0, egui::Color32::from_rgb(24, 25, 34));
    ui.painter()
        .circle_filled(status_center, 8.0, status_color(profile.status));

    // -- Name + status (to right of avatar) --
    let name_left = avatar_rect.right() + 14.0;
    let name_top = banner_rect.bottom() - 20.0;

    ui.painter().text(
        egui::pos2(name_left, name_top),
        egui::Align2::LEFT_TOP,
        &profile.display_name,
        egui::FontId::proportional(26.0),
        egui::Color32::from_rgb(239, 240, 246),
    );

    let status_y = name_top + 32.0;
    let dot_text = "●";
    let dot_galley = ui.painter().layout_no_wrap(
        dot_text.to_string(),
        egui::FontId::proportional(12.0),
        status_color(profile.status),
    );
    ui.painter().galley(
        egui::pos2(name_left, status_y),
        dot_galley,
        egui::Color32::WHITE,
    );
    ui.painter().text(
        egui::pos2(name_left + 16.0, status_y),
        egui::Align2::LEFT_TOP,
        status_text(profile.status),
        egui::FontId::proportional(14.0),
        egui::Color32::from_rgb(190, 196, 210),
    );

    // -- Content area below avatar --
    let content_top = avatar_center.y + AVATAR_HALF + 12.0;
    let content_rect = egui::Rect::from_min_max(
        egui::pos2(card_rect.left() + CONTENT_PAD, content_top),
        egui::pos2(card_rect.right() - CONTENT_PAD, card_rect.bottom()),
    );

    let mut child_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(content_rect)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(&mut child_ui, |ui| {
            render_content_area(ui, model, tx_intent, profile);
        });
}

fn render_content_area(
    ui: &mut egui::Ui,
    model: &mut UiModel,
    tx_intent: &Sender<UiIntent>,
    profile: &UserProfileData,
) {
    draw_divider(ui);
    ui.add_space(8.0);

    // -- Game activity --
    if let Some(activity) = &profile.current_activity {
        let elapsed =
            ((Local::now().timestamp_millis() - activity.started_at).max(0) / 1000) as i64;
        let h = elapsed / 3600;
        let m = (elapsed % 3600) / 60;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("🎮").size(16.0));
            ui.label(
                egui::RichText::new(format!(
                    "Playing {} \u{2014} {}h {:02}m",
                    activity.game_name, h, m
                ))
                .strong()
                .size(14.0)
                .color(egui::Color32::from_rgb(220, 222, 230)),
            );
        });
        ui.add_space(6.0);
        draw_divider(ui);
        ui.add_space(6.0);
        ui.ctx().request_repaint_after(Duration::from_secs(60));
    }

    // -- Custom status --
    if !profile.custom_status_text.trim().is_empty() {
        ui.horizontal(|ui| {
            let emoji = if profile.custom_status_emoji.trim().is_empty() {
                "\u{2728}"
            } else {
                &profile.custom_status_emoji
            };
            ui.label(egui::RichText::new(emoji).size(16.0));
            ui.label(
                egui::RichText::new(&profile.custom_status_text)
                    .size(14.0)
                    .color(egui::Color32::from_rgb(210, 214, 224)),
            );
        });
        ui.add_space(6.0);
        draw_divider(ui);
        ui.add_space(6.0);
    }

    // -- About me --
    ui.label(
        egui::RichText::new("ABOUT ME")
            .size(13.0)
            .strong()
            .color(egui::Color32::from_rgb(180, 186, 200)),
    );
    ui.add_space(2.0);
    let about = if profile.description.is_empty() {
        "No profile description set."
    } else {
        &profile.description
    };
    if about.chars().count() > 190 {
        let truncated: String = about.chars().take(190).chain("...".chars()).collect();
        markdown::render_about_me(ui, &truncated);
    } else {
        markdown::render_about_me(ui, about);
    }
    ui.add_space(8.0);
    draw_divider(ui);
    ui.add_space(6.0);

    // -- Roles --
    let mut roles = profile.roles.clone();
    roles.sort_by(|a, b| b.position.cmp(&a.position));
    if !roles.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("ROLES")
                    .size(13.0)
                    .strong()
                    .color(egui::Color32::from_rgb(180, 186, 200)),
            );
            ui.add_space(6.0);
            for role in roles {
                let role_color = u32_to_color(role.color);
                draw_tag_chip(ui, &role_color_icon(role_color), &role.name, role_color);
            }
        });
        ui.add_space(6.0);
    }

    // -- Links --
    if !profile.links.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("LINKS")
                    .size(13.0)
                    .strong()
                    .color(egui::Color32::from_rgb(180, 186, 200)),
            );
            ui.add_space(6.0);
            for link in &profile.links {
                let emoji = platform_emoji(&link.platform);
                let label = if !link.display_text.is_empty() {
                    &link.display_text
                } else {
                    &link.platform
                };

                let link_label = format!("{emoji} {label}");
                ui.add(
                    egui::Hyperlink::from_label_and_url(link_label, &link.url)
                        .open_in_new_tab(true),
                )
                .on_hover_text(&link.url);
            }
        });
        ui.add_space(6.0);
    }

    // -- Badges (inline, no header) --
    if !profile.badges.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for badge in &profile.badges {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if let Some((uri, bytes)) = badge_icon_from_icon_url(&badge.icon_url)
                        .or_else(|| badge_icon_fallback(&badge.id))
                    {
                        ui.add(
                            egui::Image::from_bytes(uri, bytes)
                                .fit_to_exact_size(egui::vec2(16.0, 16.0))
                                .corner_radius(2.0),
                        );
                    } else if !badge.icon_url.trim().is_empty() {
                        ui.add(
                            crate::ui::image_from_source(&badge.icon_url)
                                .fit_to_exact_size(egui::vec2(16.0, 16.0))
                                .corner_radius(2.0),
                        );
                    } else {
                        ui.label(egui::RichText::new("\u{2B50}").size(16.0));
                    }
                    ui.label(
                        egui::RichText::new(&badge.label)
                            .size(14.0)
                            .color(egui::Color32::from_rgb(220, 222, 230)),
                    );
                })
                .response
                .on_hover_text(&badge.tooltip);
                ui.add_space(10.0);
            }
        });
        ui.add_space(6.0);
    }

    draw_divider(ui);
    ui.add_space(6.0);

    // -- Footer: member since + action buttons --
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!(
                "Member since {}",
                Local
                    .timestamp_millis_opt(profile.created_at)
                    .single()
                    .map(|dt| dt.format("%b %d, %Y").to_string())
                    .unwrap_or_else(|| "Unknown".into())
            ))
            .size(13.0)
            .color(egui::Color32::from_rgb(150, 156, 170)),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Context menu ("...")
            ui.menu_button(egui::RichText::new("\u{2026}").size(14.0), |ui| {
                if ui.button("Copy User ID").clicked() {
                    ui.ctx().copy_text(profile.user_id.clone());
                    ui.close();
                }
                if ui.button("Roles").clicked() {
                    model.show_permissions_center = true;
                    model.permissions_tab = crate::ui::model::PermissionsTab::Members;
                    let _ = tx_intent.send(UiIntent::PermsOpen);
                    ui.close();
                }
                if ui.button("Connection Info").clicked() {
                    model.open_member_connection_info_window(
                        profile.user_id.clone(),
                        profile.display_name.clone(),
                    );
                    ui.close();
                }
                ui.separator();
                let mut local_muted = model.user_locally_muted(&profile.user_id);
                if ui.checkbox(&mut local_muted, "Mute for me").changed() {
                    model
                        .settings
                        .per_user_audio
                        .entry(profile.user_id.clone())
                        .or_default()
                        .muted = local_muted;
                }
                let mut gain = model.user_output_gain(&profile.user_id);
                if ui
                    .add(egui::Slider::new(&mut gain, 0.0..=2.0).text("Volume"))
                    .changed()
                {
                    model
                        .settings
                        .per_user_audio
                        .entry(profile.user_id.clone())
                        .or_default()
                        .gain = gain;
                    let _ = tx_intent.send(UiIntent::SetUserOutputGain {
                        user_id: profile.user_id.clone(),
                        gain,
                    });
                }
                ui.separator();
                if ui.button("Kick").clicked() {
                    let _ = tx_intent.send(UiIntent::KickUser {
                        user_id: profile.user_id.clone(),
                        reason: String::new(),
                    });
                    ui.close();
                }
                ui.add_enabled(
                    false,
                    egui::Button::new(egui::RichText::new("Ban").color(theme::COLOR_DANGER)),
                )
                .on_disabled_hover_text("Ban is currently disabled from this menu");
                ui.separator();
                ui.menu_button("Grant badge", |ui| {
                    for (badge_id, path) in BADGE_DEFS {
                        if ui.button(*badge_id).clicked() {
                            let label = title_case_badge_label(badge_id);
                            let _ = tx_intent.send(UiIntent::GrantBadgeToUser {
                                user_id: profile.user_id.clone(),
                                badge_id: (*badge_id).to_string(),
                                label: label.clone(),
                                icon_path: (*path).to_string(),
                                tooltip: format!("{label} badge"),
                            });
                            ui.close();
                        }
                    }
                });
                ui.menu_button("Remove badge", |ui| {
                    if profile.badges.is_empty() {
                        ui.label("No badges to remove");
                        return;
                    }
                    for badge in &profile.badges {
                        if ui.button(&badge.id).clicked() {
                            let _ = tx_intent.send(UiIntent::RevokeBadgeFromUser {
                                user_id: profile.user_id.clone(),
                                badge_id: badge.id.clone(),
                            });
                            ui.close();
                        }
                    }
                });
            });

            // Poke button
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("Poke").size(13.0))
                        .fill(egui::Color32::from_rgb(55, 58, 75))
                        .corner_radius(6.0),
                )
                .clicked()
            {
                model.show_poke_dialog = true;
                model.poke_target_user_id = profile.user_id.clone();
                model.poke_target_display_name = profile.display_name.clone();
                model.poke_message_draft = "Poke".into();
            }

            // Message button
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("Message").size(13.0))
                        .fill(egui::Color32::from_rgb(55, 58, 75))
                        .corner_radius(6.0),
                )
                .clicked()
            {
                model.profile_popup_user_id = None;
                model.profile_popup_data = None;
                model.profile_popup_loading = false;
                model.profile_popup_anchor = None;
                let _ = tx_intent.send(UiIntent::CreateDmChannel {
                    participant_user_ids: vec![profile.user_id.clone()],
                });
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn status_text(status: OnlineStatus) -> &'static str {
    match status {
        OnlineStatus::Online => "Online",
        OnlineStatus::Idle => "Idle",
        OnlineStatus::DoNotDisturb => "Do Not Disturb",
        OnlineStatus::Invisible => "Invisible",
        OnlineStatus::Offline => "Offline",
    }
}

fn status_color(status: OnlineStatus) -> egui::Color32 {
    match status {
        OnlineStatus::Online => egui::Color32::from_rgb(88, 214, 135),
        OnlineStatus::Idle => theme::COLOR_IDLE,
        OnlineStatus::DoNotDisturb => theme::COLOR_DND,
        OnlineStatus::Invisible | OnlineStatus::Offline => theme::COLOR_OFFLINE,
    }
}

fn draw_divider(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(155, 171, 220, 32),
        ),
    );
}

fn draw_tag_chip(
    ui: &mut egui::Ui,
    icon: &str,
    label: &str,
    color: egui::Color32,
) -> egui::Response {
    egui::Frame::new()
        .fill(color.linear_multiply(0.20))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(egui::RichText::new(icon).size(14.0));
                ui.label(
                    egui::RichText::new(label)
                        .size(13.0)
                        .color(egui::Color32::WHITE),
                );
            });
        })
        .response
}

/// Returns a small colored square emoji stand-in for role color indicators.
fn role_color_icon(_color: egui::Color32) -> String {
    "\u{25A0}".to_string()
}

fn paint_vertical_gradient(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    start: egui::Color32,
    end: egui::Color32,
    rounding: egui::CornerRadius,
) {
    let steps = 28;
    for i in 0..steps {
        let t0 = i as f32 / steps as f32;
        let t1 = (i + 1) as f32 / steps as f32;
        let y0 = egui::lerp(rect.top()..=rect.bottom(), t0);
        let y1 = egui::lerp(rect.top()..=rect.bottom(), t1);
        ui.painter().rect_filled(
            egui::Rect::from_min_max(egui::pos2(rect.left(), y0), egui::pos2(rect.right(), y1)),
            if i == 0 {
                rounding
            } else {
                egui::CornerRadius::ZERO
            },
            lerp_color(start, end, t0),
        );
    }
}

fn paint_horizontal_tint(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    left: egui::Color32,
    right: egui::Color32,
) {
    let steps = 24;
    for i in 0..steps {
        let t0 = i as f32 / steps as f32;
        let t1 = (i + 1) as f32 / steps as f32;
        let x0 = egui::lerp(rect.left()..=rect.right(), t0);
        let x1 = egui::lerp(rect.left()..=rect.right(), t1);
        ui.painter().rect_filled(
            egui::Rect::from_min_max(egui::pos2(x0, rect.top()), egui::pos2(x1, rect.bottom())),
            egui::CornerRadius::same(0),
            lerp_color(left, right, t0).linear_multiply(0.12),
        );
    }
}

fn u32_to_color(value: u32) -> egui::Color32 {
    let r = ((value >> 16) & 0xFF) as u8;
    let g = ((value >> 8) & 0xFF) as u8;
    let b = (value & 0xFF) as u8;
    if r == 0 && g == 0 && b == 0 {
        theme::COLOR_ACCENT
    } else {
        egui::Color32::from_rgb(r, g, b)
    }
}

fn lerp_color(start: egui::Color32, end: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let r = start.r() as f32 + (end.r() as f32 - start.r() as f32) * t;
    let g = start.g() as f32 + (end.g() as f32 - start.g() as f32) * t;
    let b = start.b() as f32 + (end.b() as f32 - start.b() as f32) * t;
    egui::Color32::from_rgb(r as u8, g as u8, b as u8)
}

fn badge_icon_fallback(badge_id: &str) -> Option<(&'static str, &'static [u8])> {
    match badge_id {
        "staff" | "admin" => Some(("bytes://badge/crown", BADGE_CROWN)),
        "mod" => Some(("bytes://badge/shield", BADGE_SHIELD)),
        "developer" => Some(("bytes://badge/code", BADGE_CODE)),
        "founder" => Some(("bytes://badge/trophy", BADGE_TROPHY)),
        "early-adopter" => Some(("bytes://badge/level_up", BADGE_LEVEL_UP)),
        _ => None,
    }
}

fn badge_icon_from_icon_url(icon_url: &str) -> Option<(&'static str, &'static [u8])> {
    match icon_url.trim() {
        "client/assets/Badges/24_crown.png" => Some(("bytes://badge/crown", BADGE_CROWN)),
        "client/assets/Badges/14_shield.png" => Some(("bytes://badge/shield", BADGE_SHIELD)),
        "client/assets/Badges/07_code.png" => Some(("bytes://badge/code", BADGE_CODE)),
        "client/assets/Badges/23_trophy.png" => Some(("bytes://badge/trophy", BADGE_TROPHY)),
        "client/assets/Badges/40_level_up.png" => Some(("bytes://badge/level_up", BADGE_LEVEL_UP)),
        _ => None,
    }
}

fn title_case_badge_label(badge_id: &str) -> String {
    badge_id
        .split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn platform_emoji(platform: &str) -> &'static str {
    match platform.to_lowercase().as_str() {
        "steam" => "\u{2731}",
        "github" => "\u{273F}",
        "twitter" | "x" => "\u{1F426}",
        "twitch" => "\u{1F4FA}",
        "youtube" => "\u{25B6}\u{FE0F}",
        "website" => "\u{1F310}",
        _ => "\u{1F517}",
    }
}
