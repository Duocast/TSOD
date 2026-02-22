//! User panel at the bottom-left: avatar, name, mute/deafen buttons.

use crate::ui::model::{UiIntent, UiModel};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, model: &UiModel, tx_intent: &Sender<UiIntent>) {
    // Avatar circle
    let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
    let center = rect.center();
    let status_color = if model.connected {
        theme::COLOR_ONLINE
    } else {
        theme::COLOR_OFFLINE
    };
    ui.painter()
        .circle_filled(center, 12.0, theme::COLOR_BG_LIGHT);
    let initial = model.nick.chars().next().unwrap_or('?').to_uppercase().to_string();
    ui.painter().text(
        center,
        egui::Align2::CENTER_CENTER,
        &initial,
        egui::FontId::proportional(12.0),
        theme::COLOR_TEXT,
    );
    // Status dot
    ui.painter()
        .circle_filled(center + egui::vec2(8.0, 8.0), 5.0, status_color);

    // Name
    ui.label(
        egui::RichText::new(&model.nick)
            .strong()
            .size(13.0),
    );

    // Mute button
    let mute_icon = if model.self_muted { "🔇" } else { "🎤" };
    let mute_btn = ui.add(
        egui::Button::new(mute_icon).frame(false),
    );
    if mute_btn.clicked() {
        let _ = tx_intent.send(UiIntent::ToggleSelfMute);
    }
    mute_btn.on_hover_text(if model.self_muted { "Unmute" } else { "Mute" });

    // Deafen button
    let deafen_icon = if model.self_deafened { "🔇" } else { "🔊" };
    let deafen_btn = ui.add(
        egui::Button::new(deafen_icon).frame(false),
    );
    if deafen_btn.clicked() {
        let _ = tx_intent.send(UiIntent::ToggleSelfDeafen);
    }
    deafen_btn.on_hover_text(if model.self_deafened { "Undeafen" } else { "Deafen" });
}
