//! Server / channel tree sidebar panel.

use crate::ui::model::{ChannelType, UiIntent, UiModel};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    ui.horizontal(|ui| {
        ui.heading("Channels");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("+")
                .on_hover_text("Create Channel")
                .clicked()
            {
                model.show_create_channel = true;
                model.create_channel_name.clear();
                model.create_channel_description.clear();
                model.create_channel_type = 0;
                model.create_channel_codec = 0;
                model.create_channel_quality = 64;
                model.create_channel_user_limit = 0;
                model.create_channel_tab = 0;
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
                egui::RichText::new(&cat_name)
                    .strong()
                    .color(theme::text_dim())
                    .size(11.0),
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
                    .color(theme::text_muted())
                    .italics(),
            );
        }
    });
}

const CHANNEL_TYPE_LABELS: &[&str] = &["Voice", "Text"];
const CODEC_LABELS: &[&str] = &["Opus Voice", "Opus Music"];

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
        .default_width(420.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            // Tab bar
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(model.create_channel_tab == 0, "Standard")
                    .clicked()
                {
                    model.create_channel_tab = 0;
                }
                ui.separator();
                if ui
                    .selectable_label(model.create_channel_tab == 1, "Audio")
                    .clicked()
                {
                    model.create_channel_tab = 1;
                }
            });
            ui.separator();
            ui.add_space(4.0);

            match model.create_channel_tab {
                0 => show_create_tab_standard(ui, model),
                1 => show_create_tab_audio(ui, model),
                _ => {}
            }

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(4.0);

            // Action buttons
            ui.horizontal(|ui| {
                let name = model.create_channel_name.trim().to_string();
                let can_create = !name.is_empty();

                if ui
                    .add_enabled(can_create, egui::Button::new("Create"))
                    .clicked()
                {
                    let ch_type = model.create_channel_type as u8;
                    let codec = model.create_channel_codec as u8;
                    let quality = model.create_channel_quality;
                    let user_limit = model.create_channel_user_limit;
                    let description = model.create_channel_description.trim().to_string();
                    let _ = tx_intent.send(UiIntent::CreateChannel {
                        name,
                        description,
                        channel_type: ch_type,
                        codec,
                        quality,
                        user_limit,
                    });
                    model.show_create_channel = false;
                }

                if ui.button("Cancel").clicked() {
                    model.show_create_channel = false;
                }
            });
        });
    if !open {
        model.show_create_channel = false;
    }
}

fn show_create_tab_standard(ui: &mut egui::Ui, model: &mut UiModel) {
    // Channel name
    ui.horizontal(|ui| {
        ui.label("Channel Name:");
    });
    ui.text_edit_singleline(&mut model.create_channel_name);
    ui.add_space(6.0);

    // Channel type
    ui.horizontal(|ui| {
        ui.label("Channel Type:");
        egui::ComboBox::from_id_salt("create_ch_type")
            .selected_text(
                *CHANNEL_TYPE_LABELS
                    .get(model.create_channel_type)
                    .unwrap_or(&"Voice"),
            )
            .width(140.0)
            .show_ui(ui, |ui| {
                for (i, label) in CHANNEL_TYPE_LABELS.iter().enumerate() {
                    ui.selectable_value(&mut model.create_channel_type, i, *label);
                }
            });
    });
    ui.add_space(6.0);

    // Topic / Description
    ui.label("Topic / Description:");
    ui.add(
        egui::TextEdit::multiline(&mut model.create_channel_description)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text("Channel topic..."),
    );
    ui.add_space(6.0);

    // User limit
    ui.horizontal(|ui| {
        ui.label("Max Clients:");
        let mut limit = model.create_channel_user_limit as i32;
        if ui
            .add(egui::DragValue::new(&mut limit).range(0..=999).speed(1))
            .changed()
        {
            model.create_channel_user_limit = limit.max(0) as u32;
        }
        if model.create_channel_user_limit == 0 {
            ui.label(
                egui::RichText::new("(unlimited)")
                    .small()
                    .color(theme::text_muted()),
            );
        }
    });
}

fn show_create_tab_audio(ui: &mut egui::Ui, model: &mut UiModel) {
    // Codec selection
    ui.horizontal(|ui| {
        ui.label("Codec:");
        egui::ComboBox::from_id_salt("create_ch_codec")
            .selected_text(
                *CODEC_LABELS
                    .get(model.create_channel_codec)
                    .unwrap_or(&"Opus Voice"),
            )
            .width(160.0)
            .show_ui(ui, |ui| {
                for (i, label) in CODEC_LABELS.iter().enumerate() {
                    ui.selectable_value(&mut model.create_channel_codec, i, *label);
                }
            });
    });
    ui.add_space(4.0);

    // Codec description
    let codec_desc = match model.create_channel_codec {
        0 => "Optimized for speech. Lower latency, smaller bandwidth.",
        1 => "Optimized for music and high-fidelity audio. Higher bandwidth.",
        _ => "",
    };
    ui.label(
        egui::RichText::new(codec_desc)
            .small()
            .italics()
            .color(theme::text_muted()),
    );
    ui.add_space(8.0);

    // Quality / Bitrate slider
    ui.horizontal(|ui| {
        ui.label("Quality:");
        let range = match model.create_channel_codec {
            0 => 8..=128,  // voice range
            1 => 32..=510, // music range
            _ => 8..=510,
        };
        let mut quality = model.create_channel_quality as i32;
        if ui
            .add(
                egui::Slider::new(&mut quality, range)
                    .suffix(" kbps")
                    .step_by(1.0),
            )
            .changed()
        {
            model.create_channel_quality = quality as u32;
        }
    });
    ui.add_space(4.0);

    // Quality presets
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Presets:")
                .small()
                .color(theme::text_dim()),
        );
        if model.create_channel_codec == 0 {
            // Voice presets
            if ui.small_button("Narrow (16)").clicked() {
                model.create_channel_quality = 16;
            }
            if ui.small_button("Normal (32)").clicked() {
                model.create_channel_quality = 32;
            }
            if ui.small_button("Wide (64)").clicked() {
                model.create_channel_quality = 64;
            }
            if ui.small_button("Ultra (128)").clicked() {
                model.create_channel_quality = 128;
            }
        } else {
            // Music presets
            if ui.small_button("Low (64)").clicked() {
                model.create_channel_quality = 64;
            }
            if ui.small_button("Medium (128)").clicked() {
                model.create_channel_quality = 128;
            }
            if ui.small_button("High (256)").clicked() {
                model.create_channel_quality = 256;
            }
            if ui.small_button("Lossless (510)").clicked() {
                model.create_channel_quality = 510;
            }
        }
    });
    ui.add_space(8.0);

    // Bandwidth estimate
    let bw_kbps = model.create_channel_quality;
    let overhead_kbps = 5; // QUIC/UDP overhead
    let total = bw_kbps + overhead_kbps;
    ui.label(
        egui::RichText::new(format!(
            "Estimated bandwidth: ~{total} kbps/user ({bw_kbps} audio + ~{overhead_kbps} overhead)"
        ))
        .small()
        .color(theme::text_dim()),
    );
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
        theme::text_color()
    } else {
        theme::text_dim()
    };

    let label = format!("{} {}", icon, ch.name);

    let response = ui.selectable_label(is_selected, egui::RichText::new(&label).color(text_color));

    if response.clicked() {
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
                    .color(theme::text_muted()),
            );
        });
    }
}
