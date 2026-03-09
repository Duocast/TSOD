use crate::ui::markdown;
use crate::ui::model::{OnlineStatus, UiIntent, UiModel, UserProfileData};
use crate::ui::theme;
use chrono::{Local, TimeZone};
use crossbeam_channel::Sender;
use eframe::egui;
use std::time::Duration;

const POPUP_SIZE: egui::Vec2 = egui::vec2(640.0, 650.0);
const BANNER_HEIGHT: f32 = 120.0;
const CARD_RADIUS: u8 = 18;
const CONTENT_X_PADDING: f32 = 26.0;

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
                .fill(egui::Color32::from_rgb(16, 20, 34))
                .corner_radius(CARD_RADIUS as f32)
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
    let banner_rect =
        egui::Rect::from_min_size(card_rect.min, egui::vec2(card_rect.width(), BANNER_HEIGHT));
    let banner_rounding = egui::CornerRadius {
        nw: CARD_RADIUS,
        ne: CARD_RADIUS,
        sw: 0,
        se: 0,
    };

    paint_vertical_gradient(
        ui,
        card_rect,
        egui::Color32::from_rgb(19, 25, 45),
        egui::Color32::from_rgb(17, 21, 36),
        egui::CornerRadius::same(CARD_RADIUS),
    );
    paint_horizontal_tint(
        ui,
        card_rect,
        egui::Color32::from_rgb(32, 53, 127),
        egui::Color32::from_rgb(78, 56, 149),
        0.08,
    );

    if let Some(url) = profile.banner_url.as_ref().filter(|u| !u.is_empty()) {
        ui.put(
            banner_rect,
            egui::Image::from_uri(url)
                .fit_to_exact_size(banner_rect.size())
                .corner_radius(banner_rounding),
        );
    } else {
        paint_vertical_gradient(
            ui,
            banner_rect,
            egui::Color32::from_rgb(22, 34, 72),
            egui::Color32::from_rgb(13, 19, 37),
            banner_rounding,
        );
        paint_horizontal_tint(
            ui,
            banner_rect,
            egui::Color32::from_rgb(65, 89, 186),
            egui::Color32::from_rgb(149, 97, 232),
            0.22,
        );
    }

    let header_bg = egui::Rect::from_min_max(
        egui::pos2(card_rect.left(), banner_rect.bottom() - 10.0),
        egui::pos2(card_rect.right(), banner_rect.bottom() + 84.0),
    );
    ui.painter().rect_filled(
        header_bg,
        egui::CornerRadius::ZERO,
        egui::Color32::from_rgba_unmultiplied(12, 17, 30, 176),
    );

    let avatar_center = egui::pos2(
        banner_rect.left() + CONTENT_X_PADDING + 70.0,
        banner_rect.bottom() + 34.0,
    );
    let avatar_size = 110.0;
    let avatar_rect =
        egui::Rect::from_center_size(avatar_center, egui::vec2(avatar_size, avatar_size));

    ui.painter().circle_filled(
        avatar_center,
        avatar_size * 0.5,
        egui::Color32::from_rgb(227, 102, 47),
    );

    if let Some(url) = profile.avatar_url.as_ref().filter(|u| !u.is_empty()) {
        ui.put(
            avatar_rect,
            egui::Image::from_uri(url)
                .fit_to_exact_size(avatar_rect.size())
                .corner_radius(egui::CornerRadius::same((avatar_size * 0.5) as u8)),
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
            if fallback.is_empty() { "?" } else { &fallback },
            egui::FontId::proportional(52.0),
            egui::Color32::WHITE,
        );
    }

    let status_center = egui::pos2(avatar_rect.right() - 12.0, avatar_rect.bottom() - 12.0);
    ui.painter()
        .circle_filled(status_center, 22.0, egui::Color32::from_rgb(12, 17, 30));
    ui.painter().circle_stroke(
        status_center,
        12.0,
        egui::Stroke::new(2.0, status_color(profile.status)),
    );

    ui.add_space(BANNER_HEIGHT + 18.0);
    ui.add_space(CONTENT_X_PADDING);

    ui.horizontal(|ui| {
        ui.add_space(CONTENT_X_PADDING + 124.0);
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(&profile.display_name)
                    .strong()
                    .size(62.0)
                    .color(egui::Color32::from_rgb(240, 242, 248)),
            );
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("●")
                        .size(26.0)
                        .color(status_color(profile.status)),
                );
                ui.label(
                    egui::RichText::new(status_text(profile.status))
                        .size(22.0)
                        .color(egui::Color32::from_rgb(227, 232, 243)),
                );
            });
        });
    });

    ui.add_space(14.0);
    draw_divider(ui);
    ui.add_space(12.0);

    if let Some(activity) = &profile.current_activity {
        let elapsed =
            ((Local::now().timestamp_millis() - activity.started_at).max(0) / 1000) as i64;
        let h = elapsed / 3600;
        let m = (elapsed % 3600) / 60;
        ui.horizontal(|ui| {
            ui.add_space(CONTENT_X_PADDING);
            ui.label(egui::RichText::new("🎮").size(30.0));
            ui.label(
                egui::RichText::new(format!("Playing {}  —  {}h  {}m", activity.game_name, h, m))
                    .size(22.0)
                    .color(egui::Color32::from_rgb(231, 234, 244)),
            );
        });
        ui.add_space(12.0);
        draw_divider(ui);
        ui.add_space(10.0);
        ui.ctx().request_repaint_after(Duration::from_secs(60));
    }

    if !profile.custom_status_text.trim().is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(CONTENT_X_PADDING);
            ui.label(egui::RichText::new(profile.custom_status_emoji.to_string()).size(30.0));
            ui.label(
                egui::RichText::new(&profile.custom_status_text)
                    .size(22.0)
                    .color(egui::Color32::from_rgb(231, 234, 244)),
            );
        });
        ui.add_space(12.0);
        draw_divider(ui);
        ui.add_space(10.0);
    }

    ui.horizontal(|ui| {
        ui.add_space(CONTENT_X_PADDING);
        ui.label(
            egui::RichText::new("ABOUT ME")
                .size(18.0)
                .color(egui::Color32::from_rgb(220, 225, 237)),
        );
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(CONTENT_X_PADDING);
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
    });

    ui.add_space(14.0);
    draw_divider(ui);
    ui.add_space(10.0);

    let mut roles = profile.roles.clone();
    roles.sort_by(|a, b| b.position.cmp(&a.position));
    if !roles.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(CONTENT_X_PADDING);
            ui.label(egui::RichText::new("ROLES").size(18.0));
            ui.add_space(10.0);
            for role in roles {
                draw_tag_chip(ui, "◈", &role.name, u32_to_color(role.color));
            }
        });
        ui.add_space(10.0);
    }

    if !profile.links.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(CONTENT_X_PADDING);
            ui.label(egui::RichText::new("LINKS").size(18.0));
            ui.add_space(10.0);
            for link in &profile.links {
                let emoji = platform_emoji(&link.platform);
                let label = if !link.display_text.is_empty() {
                    &link.display_text
                } else {
                    &link.platform
                };
                draw_tag_chip(ui, emoji, label, egui::Color32::from_rgb(67, 91, 175))
                    .on_hover_text(&link.url);
            }
        });
        ui.add_space(8.0);
    }

    if !profile.badges.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(CONTENT_X_PADDING);
            for badge in &profile.badges {
                draw_flat_badge(ui, &badge.label).on_hover_text(&badge.tooltip);
            }
        });
        ui.add_space(12.0);
    }

    draw_divider(ui);
    ui.add_space(12.0);

    ui.horizontal(|ui| {
        ui.add_space(CONTENT_X_PADDING);
        ui.label(
            egui::RichText::new(format!(
                "Member since {}",
                Local
                    .timestamp_millis_opt(profile.created_at)
                    .single()
                    .map(|dt| dt.format("%b %d, %Y").to_string())
                    .unwrap_or_else(|| "Unknown".into())
            ))
            .size(22.0),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(CONTENT_X_PADDING);
            ui.menu_button("···", |ui| {
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
                if ui.button("Ban").clicked() {
                    let _ = tx_intent.send(UiIntent::BanUser {
                        user_id: profile.user_id.clone(),
                        reason: String::new(),
                        duration: 0,
                    });
                    ui.close();
                }
            });
            if ui.button(egui::RichText::new("Poke").size(18.0)).clicked() {
                model.show_poke_dialog = true;
                model.poke_target_user_id = profile.user_id.clone();
                model.poke_target_display_name = profile.display_name.clone();
                model.poke_message_draft = "Poke".into();
            }
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("Message").size(18.0))
                        .fill(egui::Color32::from_rgb(43, 49, 73)),
                )
                .clicked()
            {
                let _ = tx_intent.send(UiIntent::CreateDmChannel {
                    participant_user_ids: vec![profile.user_id.clone()],
                });
            }
        });
    });
}

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
    let width = ui.available_width() - (CONTENT_X_PADDING * 2.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    let line_rect = rect.translate(egui::vec2(CONTENT_X_PADDING, 0.0));
    ui.painter().line_segment(
        [line_rect.left_center(), line_rect.right_center()],
        egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(164, 180, 225, 44),
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
        .fill(color.linear_multiply(0.24))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon).size(16.0));
                ui.label(
                    egui::RichText::new(label)
                        .size(18.0)
                        .color(egui::Color32::WHITE),
                );
            });
        })
        .response
}

fn draw_flat_badge(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.label(
        egui::RichText::new(format!("⭐ {label}"))
            .size(20.0)
            .color(egui::Color32::from_rgb(234, 238, 248)),
    )
}

fn paint_vertical_gradient(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    start: egui::Color32,
    end: egui::Color32,
    rounding: egui::CornerRadius,
) {
    let steps = 24;
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
    alpha_mult: f32,
) {
    let steps = 24;
    for i in 0..steps {
        let t0 = i as f32 / steps as f32;
        let t1 = (i + 1) as f32 / steps as f32;
        let x0 = egui::lerp(rect.left()..=rect.right(), t0);
        let x1 = egui::lerp(rect.left()..=rect.right(), t1);
        ui.painter().rect_filled(
            egui::Rect::from_min_max(egui::pos2(x0, rect.top()), egui::pos2(x1, rect.bottom())),
            egui::CornerRadius::ZERO,
            lerp_color(left, right, t0).linear_multiply(alpha_mult),
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

fn platform_emoji(platform: &str) -> &'static str {
    match platform.to_lowercase().as_str() {
        "steam" => "✱",
        "github" => "✿",
        "twitter" | "x" => "🐦",
        "twitch" => "📺",
        "youtube" => "▶️",
        "website" => "🌐",
        _ => "🔗",
    }
}
