//! Server / channel tree sidebar panel.

use crate::ui::model::{ChannelType, UiIntent, UiModel};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    ui.heading("Channels");
    ui.separator();

    egui::ScrollArea::vertical().show(ui, |ui| {
        // Group channels by parent (categories)
        let categories: Vec<_> = model
            .channels
            .iter()
            .filter(|c| c.channel_type == ChannelType::Category)
            .collect();

        let ungrouped: Vec<_> = model
            .channels
            .iter()
            .filter(|c| c.channel_type != ChannelType::Category && c.parent_id.is_none())
            .collect();

        // Show ungrouped channels first
        for ch in &ungrouped {
            show_channel(ui, ch, model, tx_intent);
        }

        // Show categories with their children
        for cat in &categories {
            let id = egui::Id::new(&cat.id);
            egui::CollapsingHeader::new(
                egui::RichText::new(&cat.name).strong().color(theme::COLOR_TEXT_DIM).size(11.0),
            )
            .id_salt(id)
            .default_open(true)
            .show(ui, |ui| {
                let children: Vec<_> = model
                    .channels
                    .iter()
                    .filter(|c| c.parent_id.as_deref() == Some(&cat.id))
                    .collect();

                for ch in &children {
                    show_channel(ui, ch, model, tx_intent);
                }
            });
        }

        // If no channels exist, show placeholder
        if model.channels.is_empty() {
            ui.label(
                egui::RichText::new("No channels yet")
                    .color(theme::COLOR_TEXT_MUTED)
                    .italics(),
            );
        }
    });
}

fn show_channel(
    ui: &mut egui::Ui,
    ch: &crate::ui::model::ChannelEntry,
    model: &UiModel,
    tx_intent: &Sender<UiIntent>,
) {
    let is_selected = model.selected_channel.as_deref() == Some(&ch.id);
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
