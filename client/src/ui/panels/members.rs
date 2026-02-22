//! Member list panel (right sidebar).

use crate::ui::model::{UiIntent, UiModel};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, model: &UiModel, tx_intent: &Sender<UiIntent>) {
    ui.heading("Members");
    ui.separator();

    let members = model.current_members();
    if members.is_empty() {
        ui.label(
            egui::RichText::new("No members")
                .color(theme::COLOR_TEXT_MUTED)
                .italics(),
        );
        return;
    }

    // Separate online (connected) members - for now all are "online"
    ui.label(
        egui::RichText::new(format!("ONLINE — {}", members.len()))
            .small()
            .strong()
            .color(theme::COLOR_TEXT_MUTED),
    );

    egui::ScrollArea::vertical().show(ui, |ui| {
        for member in members {
            let is_speaking = model
                .speaking_users
                .get(&member.user_id)
                .copied()
                .unwrap_or(false)
                || member.speaking;

            let response = ui
                .horizontal(|ui| {
                    // Speaking indicator (green ring)
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(32.0, 32.0), egui::Sense::hover());
                    let center = rect.center();
                    let radius = 14.0;

                    // Avatar circle (placeholder)
                    ui.painter().circle_filled(
                        center,
                        radius,
                        theme::COLOR_BG_LIGHT,
                    );

                    // First letter of name as avatar
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
                        theme::COLOR_TEXT,
                    );

                    // Speaking ring
                    if is_speaking {
                        ui.painter().circle_stroke(
                            center,
                            radius + 2.0,
                            egui::Stroke::new(2.0, theme::COLOR_VOICE_ACTIVE),
                        );
                    }

                    // Name and status icons
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(&member.display_name)
                                .color(theme::COLOR_TEXT),
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
                            ui.label(
                                egui::RichText::new(status_parts.join(", "))
                                    .small()
                                    .color(theme::COLOR_TEXT_MUTED),
                            );
                        }
                    });
                })
                .response;

            // Context menu for moderation
            response.context_menu(|ui| {
                if ui.button("Poke").clicked() {
                    let _ = tx_intent.send(UiIntent::PokeUser {
                        user_id: member.user_id.clone(),
                        message: String::new(),
                    });
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
                let deafen_label = if member.deafened { "Undeafen" } else { "Deafen" };
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
                if ui
                    .button(egui::RichText::new("Ban").color(theme::COLOR_DANGER))
                    .clicked()
                {
                    let _ = tx_intent.send(UiIntent::BanUser {
                        user_id: member.user_id.clone(),
                        reason: String::new(),
                        duration: 0,
                    });
                    ui.close_menu();
                }
            });
        }
    });
}
