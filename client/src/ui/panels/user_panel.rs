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
    egui::Frame::none()
        .fill(theme::COLOR_BG_DARK)
        .inner_margin(8.0)
        .outer_margin(egui::Margin::ZERO)
        .rounding(0.0)
        .show(ui, |ui: &mut egui::Ui| {
            ui.set_min_width(panel_width - 16.0);

            // Top row: avatar + name + status
            ui.horizontal(|ui: &mut egui::Ui| {
                // Clickable avatar button
                let initial = model.nick.chars().next().unwrap_or('?').to_uppercase().to_string();
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
                    theme::COLOR_BG_INPUT
                } else {
                    theme::COLOR_BG_LIGHT
                };
                ui.painter().circle_filled(rect.center(), 16.0, bg_color);

                // Initial letter
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    &initial,
                    egui::FontId::proportional(15.0),
                    theme::COLOR_TEXT,
                );

                // Status indicator dot
                let dot_pos = rect.center() + egui::vec2(10.0, 10.0);
                ui.painter().circle_filled(dot_pos, 6.0, theme::COLOR_BG_DARK);
                ui.painter().circle_filled(dot_pos, 4.0, status_color);

                if response.clicked() {
                    model.show_user_popup = !model.show_user_popup;
                }
                response.on_hover_text("User Profile");

                // Name and status text
                ui.vertical(|ui: &mut egui::Ui| {
                    ui.label(
                        egui::RichText::new(&model.nick)
                            .strong()
                            .size(13.0)
                            .color(theme::COLOR_TEXT),
                    );
                    let status_text = if model.connected { "Online" } else { "Offline" };
                    ui.label(
                        egui::RichText::new(status_text)
                            .size(11.0)
                            .color(theme::COLOR_TEXT_MUTED),
                    );
                });
            });

            ui.add_space(6.0);

            // Bottom row: action buttons
            ui.horizontal(|ui: &mut egui::Ui| {
                let btn_size = egui::vec2(36.0, 28.0);

                // Mute button
                let mute_label = if model.self_muted { "Unmute" } else { "Mute" };
                let mute_fill = if model.self_muted {
                    theme::COLOR_DANGER.linear_multiply(0.3)
                } else {
                    theme::COLOR_BG_LIGHT
                };
                let mute_text_color = if model.self_muted {
                    theme::COLOR_DANGER
                } else {
                    theme::COLOR_TEXT
                };
                let mute_icon = if model.self_muted { "MIC OFF" } else { "MIC" };

                let mute_btn = ui.add_sized(
                    btn_size,
                    egui::Button::new(
                        egui::RichText::new(mute_icon).size(10.0).color(mute_text_color).strong(),
                    )
                    .fill(mute_fill)
                    .rounding(4.0),
                );
                if mute_btn.clicked() {
                    model.self_muted = !model.self_muted;
                    let _ = tx_intent.send(UiIntent::ToggleSelfMute);
                }
                mute_btn.on_hover_text(mute_label);

                ui.add_space(2.0);

                // Deafen button
                let deafen_label = if model.self_deafened { "Undeafen" } else { "Deafen" };
                let deafen_fill = if model.self_deafened {
                    theme::COLOR_DANGER.linear_multiply(0.3)
                } else {
                    theme::COLOR_BG_LIGHT
                };
                let deafen_text_color = if model.self_deafened {
                    theme::COLOR_DANGER
                } else {
                    theme::COLOR_TEXT
                };
                let deafen_icon = if model.self_deafened { "DEAF" } else { "SOUND" };

                let deafen_btn = ui.add_sized(
                    btn_size,
                    egui::Button::new(
                        egui::RichText::new(deafen_icon).size(10.0).color(deafen_text_color).strong(),
                    )
                    .fill(deafen_fill)
                    .rounding(4.0),
                );
                if deafen_btn.clicked() {
                    model.self_deafened = !model.self_deafened;
                    let _ = tx_intent.send(UiIntent::ToggleSelfDeafen);
                }
                deafen_btn.on_hover_text(deafen_label);

                // Spacer pushes settings to right
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui: &mut egui::Ui| {
                    let settings_btn = ui.add_sized(
                        btn_size,
                        egui::Button::new(
                            egui::RichText::new("SET").size(10.0).color(theme::COLOR_TEXT_DIM).strong(),
                        )
                        .fill(theme::COLOR_BG_LIGHT)
                        .rounding(4.0),
                    );
                    if settings_btn.clicked() {
                        model.show_settings = !model.show_settings;
                    }
                    settings_btn.on_hover_text("Settings");
                });
            });

            // VAD level bar (when voice is active)
            if let Some(vad) = model.vad_level {
                ui.add_space(4.0);
                let bar_width = ui.available_width();
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(bar_width, 4.0),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(rect, 2.0, theme::COLOR_BG_MEDIUM);
                let filled_width = bar_width * vad;
                let filled = egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(filled_width, 4.0),
                );
                let color = if vad > 0.5 {
                    theme::COLOR_ONLINE
                } else {
                    theme::COLOR_IDLE
                };
                ui.painter().rect_filled(filled, 2.0, color);
            }
        });
}
