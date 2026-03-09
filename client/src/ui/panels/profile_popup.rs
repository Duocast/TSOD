use crate::ui::model::{OnlineStatus, UiIntent, UiModel, UserProfileData};
use crate::ui::theme;
use chrono::{Local, TimeZone};
use crossbeam_channel::Sender;
use eframe::egui;
use std::time::Duration;

const POPUP_SIZE: egui::Vec2 = egui::vec2(340.0, 420.0);
const BANNER_HEIGHT: f32 = 100.0;

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
                .fill(theme::bg_medium())
                .corner_radius(10.0),
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
        i.pointer.primary_clicked()
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
    let banner_rect = egui::Rect::from_min_size(
        ui.min_rect().min,
        egui::vec2(ui.available_width(), BANNER_HEIGHT),
    );
    let clip_rounding = egui::CornerRadius {
        nw: 10,
        ne: 10,
        sw: 0,
        se: 0,
    };
    ui.painter()
        .rect_filled(banner_rect, clip_rounding, theme::bg_dark());

    if let Some(url) = profile.banner_url.as_ref().filter(|u| !u.is_empty()) {
        ui.put(
            banner_rect,
            egui::Image::from_uri(url)
                .fit_to_exact_size(banner_rect.size())
                .corner_radius(clip_rounding),
        );
    } else {
        let steps = 20;
        for i in 0..steps {
            let t0 = i as f32 / steps as f32;
            let t1 = (i + 1) as f32 / steps as f32;
            let y0 = egui::lerp(banner_rect.top()..=banner_rect.bottom(), t0);
            let y1 = egui::lerp(banner_rect.top()..=banner_rect.bottom(), t1);
            let color = lerp_color(accent, theme::bg_dark(), t0);
            ui.painter().rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(banner_rect.left(), y0),
                    egui::pos2(banner_rect.right(), y1),
                ),
                if i == 0 {
                    clip_rounding
                } else {
                    egui::CornerRadius::ZERO
                },
                color,
            );
        }
    }

    let avatar_center = egui::pos2(banner_rect.left() + 44.0, banner_rect.bottom() + 20.0);
    ui.painter().circle_filled(avatar_center, 35.0, accent);
    ui.painter()
        .circle_filled(avatar_center, 32.0, theme::bg_dark());

    let avatar_rect = egui::Rect::from_center_size(avatar_center, egui::vec2(64.0, 64.0));
    if let Some(url) = profile.avatar_url.as_ref().filter(|u| !u.is_empty()) {
        ui.put(
            avatar_rect,
            egui::Image::from_uri(url)
                .fit_to_exact_size(avatar_rect.size())
                .corner_radius(egui::CornerRadius::same(32)),
        );
    } else {
        let initial = profile
            .display_name
            .chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .to_string();
        ui.painter().text(
            avatar_center,
            egui::Align2::CENTER_CENTER,
            initial,
            egui::FontId::proportional(26.0),
            theme::text_color(),
        );
    }

    ui.add_space(BANNER_HEIGHT - 6.0);
    ui.horizontal(|ui| {
        ui.add_space(84.0);
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(&profile.display_name)
                    .strong()
                    .size(20.0),
            );
            ui.label(egui::RichText::new(status_text(profile.status)).color(theme::text_muted()));
        });
    });
    ui.separator();

    if let Some(activity) = &profile.current_activity {
        let elapsed =
            ((Local::now().timestamp_millis() - activity.started_at).max(0) / 1000) as i64;
        let h = elapsed / 3600;
        let m = (elapsed % 3600) / 60;
        ui.horizontal(|ui| {
            ui.label("🎮");
            ui.label(
                egui::RichText::new(format!("Playing {} — {}h {}m", activity.game_name, h, m))
                    .strong(),
            );
        });
        ui.separator();
        ui.ctx().request_repaint_after(Duration::from_secs(60));
    }

    if !profile.custom_status_text.trim().is_empty() {
        ui.label(format!(
            "{} {}",
            profile.custom_status_emoji, profile.custom_status_text
        ));
        ui.separator();
    }

    ui.label(egui::RichText::new("ABOUT ME").small().strong());
    let mut about = profile.description.clone();
    if about.chars().count() > 190 {
        about = format!("{}...", about.chars().take(190).collect::<String>());
    }
    if about.is_empty() {
        about = "No profile description set.".into();
    }
    ui.label(about);
    ui.separator();

    let mut roles = profile.roles.clone();
    roles.sort_by(|a, b| b.position.cmp(&a.position));
    if !roles.is_empty() {
        ui.label(egui::RichText::new("ROLES").small().strong());
        ui.horizontal_wrapped(|ui| {
            for role in roles {
                let role_color = u32_to_color(role.color);
                egui::Frame::new()
                    .fill(role_color.linear_multiply(0.2))
                    .corner_radius(6.0)
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(role.name).color(role_color).small());
                    });
            }
        });
        ui.separator();
    }

    if !profile.links.is_empty() {
        ui.label(egui::RichText::new("LINKS").small().strong());
        ui.horizontal_wrapped(|ui| {
            for (i, link) in profile.links.iter().enumerate() {
                if i > 0 {
                    ui.label("·");
                }
                let emoji = platform_emoji(&link.platform);
                let label = if !link.display_text.is_empty() {
                    &link.display_text
                } else {
                    &link.platform
                };
                ui.label(format!("{emoji} {label}"))
                    .on_hover_text(&link.url);
            }
        });
        ui.separator();
    }

    if !profile.badges.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for badge in &profile.badges {
                if badge.icon_url.trim().is_empty() {
                    ui.label(&badge.label).on_hover_text(&badge.tooltip);
                } else {
                    ui.add(
                        egui::Image::from_uri(&badge.icon_url)
                            .fit_to_exact_size(egui::vec2(16.0, 16.0))
                            .sense(egui::Sense::hover()),
                    )
                    .on_hover_text(&badge.tooltip);
                }
            }
        });
        ui.separator();
    }

    ui.label(format!(
        "Member since {}",
        Local
            .timestamp_millis_opt(profile.created_at)
            .single()
            .map(|dt| dt.format("%b %d, %Y").to_string())
            .unwrap_or_else(|| "Unknown".into())
    ));

    ui.horizontal(|ui| {
        if ui.add(egui::Button::new("Message").fill(accent)).clicked() {
            let _ = tx_intent.send(UiIntent::CreateDmChannel {
                participant_user_ids: vec![profile.user_id.clone()],
            });
        }
        if ui.button("Poke").clicked() {
            model.show_poke_dialog = true;
            model.poke_target_user_id = profile.user_id.clone();
            model.poke_target_display_name = profile.display_name.clone();
            model.poke_message_draft = "Poke".into();
        }
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
    });
}

fn status_text(status: OnlineStatus) -> &'static str {
    match status {
        OnlineStatus::Online => "● Online",
        OnlineStatus::Idle => "🌙 Idle",
        OnlineStatus::DoNotDisturb => "⛔ Do Not Disturb",
        OnlineStatus::Invisible => "Invisible",
        OnlineStatus::Offline => "Offline",
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
        "steam" => "\u{1F3AE}",
        "github" => "\u{1F419}",
        "twitter" | "x" => "\u{1F426}",
        "twitch" => "\u{1F4FA}",
        "youtube" => "\u{25B6}\u{FE0F}",
        "website" => "\u{1F310}",
        _ => "\u{1F517}",
    }
}
