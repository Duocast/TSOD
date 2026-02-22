//! Server / channel tree sidebar panel.

use crate::ui::model::{ChannelType, UiIntent, UiModel};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    ui.horizontal(|ui| {
        ui.heading("Channels");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("+").on_hover_text("Create Channel").clicked() {
                model.show_create_channel = true;
                model.create_channel_name.clear();
            }
        });
    });
    ui.separator();

    egui::ScrollArea::vertical().show(ui, |ui| {
        // Clone channel data to avoid borrow conflicts with &mut model in show_channel
        let channels: Vec<_> = model.channels.clone();

        let categories: Vec<_> = channels
            .iter()
            .filter(|c| c.channel_type == ChannelType::Category)
            .cloned()
            .collect();

        let ungrouped: Vec<_> = channels
            .iter()
            .filter(|c| c.channel_type != ChannelType::Category && c.parent_id.is_none())
            .cloned()
            .collect();

        // Show ungrouped channels first
        for ch in &ungrouped {
            show_channel(ui, ch, model, tx_intent);
        }

        // Show categories with their children
        for cat in &categories {
            let id = egui::Id::new(&cat.id);
            let cat_name = cat.name.clone();
            let cat_id = cat.id.clone();
            egui::CollapsingHeader::new(
                egui::RichText::new(&cat_name).strong().color(theme::COLOR_TEXT_DIM).size(11.0),
            )
            .id_salt(id)
            .default_open(true)
            .show(ui, |ui| {
                let children: Vec<_> = channels
                    .iter()
                    .filter(|c| c.parent_id.as_deref() == Some(cat_id.as_str()))
                    .cloned()
                    .collect();

                for ch in &children {
                    show_channel(ui, ch, model, tx_intent);
                }
            });
        }

        // If no channels exist, show placeholder
        if channels.is_empty() {
            ui.label(
                egui::RichText::new("No channels yet")
                    .color(theme::COLOR_TEXT_MUTED)
                    .italics(),
            );
        }
    });
}

pub fn show_create_channel_dialog(
    ctx: &egui::Context,
    model: &mut UiModel,
    tx_intent: &Sender<UiIntent>,
) {
    if !model.show_create_channel {
        return;
    }

    let mut open = true;
    egui::Window::new("Create Channel")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label("Channel Name:");
            let response = ui.text_edit_singleline(&mut model.create_channel_name);

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let name = model.create_channel_name.trim().to_string();
                let can_create = !name.is_empty();

                if ui.add_enabled(can_create, egui::Button::new("Create Voice Channel")).clicked() {
                    let _ = tx_intent.send(UiIntent::CreateChannel {
                        name,
                        channel_type: 1, // voice
                    });
                    model.show_create_channel = false;
                    model.create_channel_name.clear();
                }

                if ui.button("Cancel").clicked() {
                    model.show_create_channel = false;
                    model.create_channel_name.clear();
                }
            });

            // Submit on Enter
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let name = model.create_channel_name.trim().to_string();
                if !name.is_empty() {
                    let _ = tx_intent.send(UiIntent::CreateChannel {
                        name,
                        channel_type: 1,
                    });
                    model.show_create_channel = false;
                    model.create_channel_name.clear();
                }
            }
        });
    if !open {
        model.show_create_channel = false;
    }
}

fn show_channel(
    ui: &mut egui::Ui,
    ch: &crate::ui::model::ChannelEntry,
    model: &mut UiModel,
    tx_intent: &Sender<UiIntent>,
) {
    let is_selected = model.selected_channel.as_deref() == Some(ch.id.as_str());
    let icon = match ch.channel_type {
        ChannelType::Text => "#",
        ChannelType::Voice => "🔊",
        ChannelType::Category => ">",
    };

    let text_color = if is_selected {
        theme::COLOR_TEXT
    } else {
        theme::COLOR_TEXT_DIM
    };

    let label = format!("{} {}", icon, ch.name);

    let response = ui.selectable_label(
        is_selected,
        egui::RichText::new(&label).color(text_color),
    );

    if response.clicked() {
        // Set selected channel in model immediately for UI responsiveness
        model.selected_channel = Some(ch.id.clone());
        model.selected_channel_name = ch.name.clone();
        let _ = tx_intent.send(UiIntent::JoinChannel {
            channel_id: ch.id.clone(),
        });
    }

    // Show member count for voice channels
    if ch.channel_type == ChannelType::Voice && ch.member_count > 0 {
        ui.indent(ch.id.as_str(), |ui| {
            let count_text = if ch.user_limit > 0 {
                format!("{}/{}", ch.member_count, ch.user_limit)
            } else {
                format!("{}", ch.member_count)
            };
            ui.label(
                egui::RichText::new(count_text)
                    .small()
                    .color(theme::COLOR_TEXT_MUTED),
            );
        });
    }
}
