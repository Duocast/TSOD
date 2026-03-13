//! Member list panel (right sidebar).

use crate::ui::model::{UiIntent, UiModel};
use crate::ui::panels::telemetry;
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

fn member_name_color(model: &UiModel, member: &crate::ui::model::MemberEntry) -> egui::Color32 {
    let user_id = member.user_id.as_str();
    if let Some(color) = member.accent_color.filter(|color| *color != 0) {
        return egui::Color32::from_rgb(
            ((color >> 16) & 0xFF) as u8,
            ((color >> 8) & 0xFF) as u8,
            (color & 0xFF) as u8,
        );
    }

    if !user_id.trim().is_empty() {
        if user_id == model.user_id {
            if let Some(color) = model
                .self_profile
                .as_ref()
                .map(|profile| profile.accent_color)
                .filter(|color| *color != 0)
            {
                return egui::Color32::from_rgb(
                    ((color >> 16) & 0xFF) as u8,
                    ((color >> 8) & 0xFF) as u8,
                    (color & 0xFF) as u8,
                );
            }
        }

        if let Some(color) = model
            .get_cached_profile_stale(user_id)
            .map(|profile| profile.accent_color)
            .filter(|color| *color != 0)
        {
            return egui::Color32::from_rgb(
                ((color >> 16) & 0xFF) as u8,
                ((color >> 8) & 0xFF) as u8,
                (color & 0xFF) as u8,
            );
        }
    }

    theme::text_color()
}

pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    ui.heading("Members");

    ui.separator();

    let members = model.current_members().to_vec();
    if members.is_empty() {
        ui.label(
            egui::RichText::new("No members")
                .color(theme::text_muted())
                .italics(),
        );
        return;
    }

    ui.label(
        egui::RichText::new(format!("ONLINE — {}", members.len()))
            .small()
            .strong()
            .color(theme::text_muted()),
    );

    egui::ScrollArea::vertical().show(ui, |ui| {
        for member in members {
            let is_speaking = model
                .speaking_users
                .get(&member.user_id)
                .copied()
                .unwrap_or(false)
                || member.speaking;
            let voice_level = model
                .voice_levels
                .get(&member.user_id)
                .copied()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let cached_profile = model.get_cached_profile_stale(&member.user_id);
            let activity_text = cached_profile
                .and_then(|profile| profile.current_activity.as_ref())
                .map(|activity| format!("Playing {}", activity.game_name));
            let has_member_status = member.muted
                || member.self_muted
                || member.deafened
                || member.self_deafened
                || member.streaming
                || !member.away_message.trim().is_empty();
            let row_height = if has_member_status && activity_text.is_some() {
                ui.spacing().interact_size.y.max(56.0)
            } else if has_member_status {
                ui.spacing().interact_size.y.max(40.0)
            } else if activity_text.is_some() {
                ui.spacing().interact_size.y.max(48.0)
            } else {
                ui.spacing().interact_size.y.max(36.0)
            };
            let row_width = ui.available_width().max(1.0);
            let (row_rect, response) =
                ui.allocate_exact_size(egui::vec2(row_width, row_height), egui::Sense::click());

            if response.hovered() {
                ui.painter().rect_filled(
                    row_rect,
                    egui::CornerRadius::same(4),
                    ui.visuals().widgets.hovered.bg_fill.linear_multiply(0.35),
                );
            }

            // Left-click opens profile popup; middle-click opens connection info.
            if response.clicked() {
                let click_pos = response.rect.right_top() + egui::vec2(8.0, 0.0);
                model.open_profile_popup(member.user_id.clone(), click_pos, tx_intent);
            }

            if response.middle_clicked() {
                model.open_member_connection_info_window(
                    member.user_id.clone(),
                    member.display_name.clone(),
                );
            }

            if response.clicked() {
                let click_pos = response
                    .interact_pointer_pos()
                    .unwrap_or_else(|| response.rect.right_top());
                model.open_profile_popup(member.user_id.clone(), click_pos, tx_intent);
            }

            let avatar_size = 32.0;
            let avatar_rect = egui::Rect::from_min_size(
                row_rect.min + egui::vec2(4.0, (row_height - avatar_size) * 0.5),
                egui::vec2(avatar_size, avatar_size),
            );
            let center = avatar_rect.center();
            let radius = 14.0;

            ui.painter()
                .circle_filled(center, radius, theme::bg_light());

            let avatar_url = if member.user_id == model.user_id {
                model
                    .avatar_url
                    .as_deref()
                    .filter(|url| !url.is_empty())
                    .map(str::to_owned)
            } else {
                None
            }
            .or_else(|| {
                cached_profile
                    .and_then(|profile| profile.avatar_url.as_deref())
                    .filter(|url| !url.is_empty())
                    .map(str::to_owned)
            })
            .or_else(|| {
                if cached_profile.is_some() {
                    None
                } else {
                    member
                        .avatar_url
                        .as_deref()
                        .filter(|url| !url.is_empty())
                        .map(str::to_owned)
                }
            });

            if let Some(avatar_url) = avatar_url {
                ui.put(
                    avatar_rect,
                    crate::ui::image_from_source(&avatar_url)
                        .fit_to_exact_size(avatar_rect.size())
                        .corner_radius(egui::CornerRadius::same(16)),
                );
            } else {
                let initial = member
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
                    egui::FontId::proportional(14.0),
                    theme::text_color(),
                );
            }

            if is_speaking {
                ui.painter().circle_stroke(
                    center,
                    radius + 2.0,
                    egui::Stroke::new(2.0, theme::COLOR_VOICE_ACTIVE),
                );
            }

            let meter_width = 72.0;
            let meter_height = 5.0;
            let meter_bg = egui::Rect::from_min_size(
                egui::pos2(
                    row_rect.right() - meter_width - 8.0,
                    row_rect.center().y - meter_height * 0.5,
                ),
                egui::vec2(meter_width, meter_height),
            );
            ui.painter()
                .rect_filled(meter_bg, egui::CornerRadius::same(2), theme::bg_light());
            if voice_level > 0.0 {
                let meter_fg = egui::Rect::from_min_max(
                    meter_bg.min,
                    egui::pos2(
                        meter_bg.min.x + meter_bg.width() * voice_level,
                        meter_bg.max.y,
                    ),
                );
                ui.painter().rect_filled(
                    meter_fg,
                    egui::CornerRadius::same(2),
                    theme::COLOR_VOICE_ACTIVE,
                );
            }

            let text_x = avatar_rect.right() + 8.0;
            let top_y = row_rect.top() + 8.0;
            ui.painter().text(
                egui::pos2(text_x, top_y),
                egui::Align2::LEFT_TOP,
                &member.display_name,
                egui::TextStyle::Body.resolve(ui.style()),
                member_name_color(model, &member),
            );
            if let Some(activity_text) = &activity_text {
                ui.painter().text(
                    egui::pos2(text_x, top_y + 18.0),
                    egui::Align2::LEFT_TOP,
                    activity_text,
                    egui::TextStyle::Small.resolve(ui.style()),
                    theme::text_muted(),
                );
            }

            let mut status_parts: Vec<String> = Vec::new();
            if member.muted {
                status_parts.push("🔇 server-muted".into());
            }
            if member.self_muted {
                status_parts.push("🎙️ self-muted".into());
            }
            if member.deafened {
                status_parts.push("🚫🔊 server-deafened".into());
            }
            if member.self_deafened {
                status_parts.push("🔈 self-deafened".into());
            }
            if member.streaming {
                status_parts.push("📺 streaming".into());
            }
            if !member.away_message.trim().is_empty() {
                status_parts.push(format!("🌙 Away: {}", member.away_message.trim()));
            }
            if !status_parts.is_empty() {
                ui.painter().text(
                    egui::pos2(text_x, row_rect.bottom() - 4.0),
                    egui::Align2::LEFT_BOTTOM,
                    status_parts.join(", "),
                    egui::TextStyle::Small.resolve(ui.style()),
                    theme::text_muted(),
                );
            }

            response.context_menu(|ui| {
                if ui.button("View Profile").clicked() {
                    let click_pos = ui.min_rect().right_top() + egui::vec2(8.0, 0.0);
                    model.open_profile_popup(member.user_id.clone(), click_pos, tx_intent);
                    ui.close();
                }
                ui.separator();
                let current_gain = model.user_output_gain(&member.user_id);
                let mut draft_gain = current_gain;
                let mut local_muted = model.user_locally_muted(&member.user_id);
                ui.label("Local audio controls");
                if ui.checkbox(&mut local_muted, "Mute for me").changed() {
                    model
                        .settings
                        .per_user_audio
                        .entry(member.user_id.clone())
                        .or_default()
                        .muted = local_muted;
                    model.settings_draft = model.settings.clone();
                    model.settings_dirty = false;
                    let _ = tx_intent.send(UiIntent::SetUserLocalMute {
                        user_id: member.user_id.clone(),
                        muted: local_muted,
                    });
                    let _ =
                        tx_intent.send(UiIntent::SaveSettings(Box::new(model.settings.clone())));
                }
                if ui
                    .add(
                        egui::Slider::new(&mut draft_gain, 0.0..=2.0)
                            .text("Volume")
                            .show_value(true),
                    )
                    .changed()
                {
                    model
                        .settings
                        .per_user_audio
                        .entry(member.user_id.clone())
                        .or_default()
                        .gain = draft_gain;
                    model.settings_draft = model.settings.clone();
                    model.settings_dirty = false;
                    let _ = tx_intent.send(UiIntent::SetUserOutputGain {
                        user_id: member.user_id.clone(),
                        gain: draft_gain,
                    });
                    let _ =
                        tx_intent.send(UiIntent::SaveSettings(Box::new(model.settings.clone())));
                }
                ui.separator();
                if ui.button("Poke").clicked() {
                    model.show_poke_dialog = true;
                    model.poke_target_user_id = member.user_id.clone();
                    model.poke_target_display_name = member.display_name.clone();
                    model.poke_message_draft = "Poke".into();
                    ui.close();
                }
                ui.separator();
                if ui.button("Roles…").clicked() {
                    model.show_permissions_center = true;
                    model.permissions_tab = crate::ui::model::PermissionsTab::Members;
                    let _ = tx_intent.send(UiIntent::PermsOpen);
                    ui.close();
                }

                let tooltip = "Missing permission: Move Members / Mute Members / Deafen Members";
                let mute_label = if member.muted { "Unmute" } else { "Mute" };
                ui.add_enabled(false, egui::Button::new(mute_label))
                    .on_disabled_hover_text(tooltip);
                let deafen_label = if member.deafened {
                    "Undeafen"
                } else {
                    "Deafen"
                };
                ui.add_enabled(false, egui::Button::new(deafen_label))
                    .on_disabled_hover_text(tooltip);
                ui.add_enabled(false, egui::Button::new("Move…"))
                    .on_disabled_hover_text("Missing permission: Move Members");
                ui.separator();
                if ui.button("Get Connection Info").clicked() {
                    model.open_member_connection_info_window(
                        member.user_id.clone(),
                        member.display_name.clone(),
                    );
                    ui.close();
                }
                ui.separator();
                if ui.button("Kick").clicked() {
                    let _ = tx_intent.send(UiIntent::KickUser {
                        user_id: member.user_id.clone(),
                        reason: String::new(),
                    });
                    ui.close();
                }
                ui.add_enabled(
                    false,
                    egui::Button::new(egui::RichText::new("Ban").color(theme::COLOR_DANGER)),
                );
            });
        }
    });

    if model.show_poke_dialog {
        egui::Window::new("Poke user")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.label(format!("Send a poke to {}", model.poke_target_display_name));
                ui.text_edit_singleline(&mut model.poke_message_draft);
                ui.horizontal(|ui| {
                    if ui.button("Send").clicked() {
                        let _ = tx_intent.send(UiIntent::PokeUser {
                            user_id: model.poke_target_user_id.clone(),
                            message: model.poke_message_draft.clone(),
                        });
                        model.show_poke_dialog = false;
                    }
                    if ui.button("Cancel").clicked() {
                        model.show_poke_dialog = false;
                    }
                });
            });
    }

    let now = std::time::Instant::now();
    let mut close_window_indices = Vec::new();
    for (index, connection_info) in model.member_connection_info_windows.iter().enumerate() {
        let mut open = true;
        egui::Window::new(format!(
            "Connection Info — {}",
            connection_info.display_name
        ))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(460.0)
        .show(ui.ctx(), |ui| {
            let connected_for = model
                .connection_established_at
                .map(|t| now.saturating_duration_since(t))
                .unwrap_or_default();
            let idle_for = model
                .member_last_active_at
                .get(&connection_info.user_id)
                .map(|t| now.saturating_duration_since(*t))
                .or_else(|| {
                    model
                        .member_first_seen_at
                        .get(&connection_info.user_id)
                        .map(|t| now.saturating_duration_since(*t))
                })
                .unwrap_or_default();

            egui::Grid::new(format!("member_connection_info_grid_{index}"))
                .num_columns(2)
                .spacing([20.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Client name:");
                    ui.label(connection_info.display_name.as_str());
                    ui.end_row();

                    ui.label("Connection time:");
                    ui.label(format_duration(connected_for));
                    ui.end_row();

                    ui.label("Idle time:");
                    ui.label(format_duration(idle_for));
                    ui.end_row();

                    ui.label("Ping:");
                    ui.label(format!("{} ms", connection_info.telemetry.rtt_ms));
                    ui.end_row();

                    ui.label("Client address:");
                    ui.label(connection_info.connection_host.as_str());
                    ui.end_row();

                    ui.label("Packet Loss:");
                    let loss_color = if connection_info.telemetry.loss_rate > 0.05 {
                        theme::COLOR_DANGER
                    } else if connection_info.telemetry.loss_rate > 0.01 {
                        theme::COLOR_IDLE
                    } else {
                        theme::COLOR_ONLINE
                    };
                    ui.colored_label(
                        loss_color,
                        format!("{:.1}%", connection_info.telemetry.loss_rate * 100.0),
                    );
                    ui.end_row();

                    ui.separator();
                    ui.separator();
                    ui.end_row();

                    ui.label("Jitter:");
                    ui.label(format!("{} ms", connection_info.telemetry.jitter_ms));
                    ui.end_row();

                    ui.label("RX Bitrate:");
                    ui.label(format!(
                        "{} ({}/s)",
                        telemetry::format_bitrate(connection_info.telemetry.rx_bitrate_bps),
                        connection_info.telemetry.rx_pps
                    ));
                    ui.end_row();

                    ui.label("TX Bitrate:");
                    ui.label(format!(
                        "{} ({}/s)",
                        telemetry::format_bitrate(connection_info.telemetry.tx_bitrate_bps),
                        connection_info.telemetry.tx_pps
                    ));
                    ui.end_row();

                    ui.label("Jitter Buffer:");
                    ui.label(format!(
                        "{} pkts",
                        connection_info.telemetry.jitter_buffer_depth
                    ));
                    ui.end_row();

                    ui.label("Late/Lost:");
                    ui.label(format!(
                        "{}/{}",
                        connection_info.telemetry.late_packets,
                        connection_info.telemetry.lost_packets
                    ));
                    ui.end_row();

                    ui.label("Concealment:");
                    ui.label(format!(
                        "{} frames",
                        connection_info.telemetry.concealment_frames
                    ));
                    ui.end_row();

                    ui.label("Peak Stream Level:");
                    ui.label(format!(
                        "{:.0}%",
                        connection_info.telemetry.peak_stream_level * 100.0
                    ));
                    ui.end_row();

                    ui.label("Server Send-Queue Drops:");
                    ui.label(connection_info.telemetry.send_queue_drop_count.to_string());
                    ui.end_row();

                    ui.label("Playout Delay:");
                    ui.label(format!("{} ms", connection_info.telemetry.playout_delay_ms));
                    ui.end_row();

                    ui.label("AGC Gain:");
                    ui.label(format!("{:.1} dB", connection_info.telemetry.agc_gain_db));
                    ui.end_row();

                    ui.label("VAD Probability:");
                    let vad_color = if connection_info.telemetry.vad_probability > 0.5 {
                        theme::COLOR_ONLINE
                    } else {
                        theme::text_muted()
                    };
                    ui.colored_label(
                        vad_color,
                        format!("{:.0}%", connection_info.telemetry.vad_probability * 100.0),
                    );
                    ui.end_row();
                });

            ui.separator();
            ui.label(egui::RichText::new("Network Quality").strong().size(13.0));
            let quality = telemetry::compute_quality_score(
                connection_info.telemetry.rtt_ms,
                connection_info.telemetry.loss_rate,
                connection_info.telemetry.jitter_ms,
            );
            let (quality_text, quality_color) = match quality {
                80..=100 => ("Excellent", theme::COLOR_ONLINE),
                60..=79 => ("Good", theme::COLOR_ONLINE),
                40..=59 => ("Fair", theme::COLOR_IDLE),
                20..=39 => ("Poor", theme::COLOR_DND),
                _ => ("Bad", theme::COLOR_DANGER),
            };
            ui.horizontal(|ui| {
                let bar_width = 200.0;
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(bar_width, 16.0), egui::Sense::hover());
                ui.painter()
                    .rect_filled(rect, 4.0, egui::Color32::from_gray(40));
                let filled = egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(bar_width * quality as f32 / 100.0, 16.0),
                );
                ui.painter().rect_filled(filled, 4.0, quality_color);
                ui.label(
                    egui::RichText::new(format!("{quality_text} ({quality}%)"))
                        .color(quality_color),
                );
            });
        });
        if !open {
            close_window_indices.push(index);
        }
    }

    for index in close_window_indices.into_iter().rev() {
        model.member_connection_info_windows.remove(index);
    }
}

fn format_duration(dur: std::time::Duration) -> String {
    let total_secs = dur.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}
