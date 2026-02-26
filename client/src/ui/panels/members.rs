//! Member list panel (right sidebar).

use crate::ui::model::{UiIntent, UiModel};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

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
            let row_height = ui.spacing().interact_size.y.max(36.0);
            let row_width = ui.available_width().max(1.0);
            let (row_rect, response) =
                ui.allocate_exact_size(egui::vec2(row_width, row_height), egui::Sense::click());

            if response.hovered() {
                ui.painter().rect_filled(
                    row_rect,
                    egui::Rounding::same(4.0),
                    ui.visuals().widgets.hovered.bg_fill.linear_multiply(0.35),
                );
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
                .rect_filled(meter_bg, egui::Rounding::same(2.0), theme::bg_light());
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
                    egui::Rounding::same(2.0),
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
                theme::text_color(),
            );

            let mut status_parts = Vec::new();
            if member.muted || member.self_muted {
                status_parts.push("muted");
            }
            if member.deafened || member.self_deafened {
                status_parts.push("deafened");
            }
            if member.streaming {
                status_parts.push("streaming");
            }
            if !status_parts.is_empty() {
                ui.painter().text(
                    egui::pos2(text_x, row_rect.bottom() - 8.0),
                    egui::Align2::LEFT_BOTTOM,
                    status_parts.join(", "),
                    egui::TextStyle::Small.resolve(ui.style()),
                    theme::text_muted(),
                );
            }

            response.context_menu(|ui| {
                if ui.button("Poke").clicked() {
                    model.show_poke_dialog = true;
                    model.poke_target_user_id = member.user_id.clone();
                    model.poke_target_display_name = member.display_name.clone();
                    model.poke_message_draft = "Poke".into();
                    ui.close_menu();
                }
                ui.separator();
                let mute_label = if member.muted { "Unmute" } else { "Mute" };
                if ui.button(mute_label).clicked() {
                    let _ = tx_intent.send(UiIntent::MuteUser {
                        user_id: member.user_id.clone(),
                        muted: !member.muted,
                    });
                    ui.close_menu();
                }
                let deafen_label = if member.deafened {
                    "Undeafen"
                } else {
                    "Deafen"
                };
                if ui.button(deafen_label).clicked() {
                    let _ = tx_intent.send(UiIntent::DeafenUser {
                        user_id: member.user_id.clone(),
                        deafened: !member.deafened,
                    });
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Kick").clicked() {
                    let _ = tx_intent.send(UiIntent::KickUser {
                        user_id: member.user_id.clone(),
                        reason: String::new(),
                    });
                    ui.close_menu();
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
}
