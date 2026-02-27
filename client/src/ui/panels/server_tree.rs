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
                model.create_channel_parent_id = None;
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
        #[cfg(debug_assertions)]
        {
            let panel_rect = ui.max_rect();
            ui.input(|input| {
                if input.pointer.button_clicked(egui::PointerButton::Secondary) {
                    if let Some(pos) = input.pointer.interact_pos() {
                        if panel_rect.contains(pos) {
                            tracing::debug!(
                                target: "ui::channels_panel",
                                ?pos,
                                "secondary click detected in channels panel"
                            );
                        }
                    }
                }
            });
        }

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

        for ch in &ungrouped {
            show_channel(ui, ch, model, tx_intent, &channels);
        }

        // Show categories with their children
        for cat in &categories {
            show_channel(ui, cat, model, tx_intent, &channels);
        }

        // If no channels exist, show placeholder
        if channels.is_empty() {
            ui.label(
                egui::RichText::new("No channels yet")
                    .color(theme::text_muted())
                    .italics(),
            );
        }

        // Right-click empty area in channels panel.
        let filler_h = ui.available_height().max(1.0);
        let (filler_rect, filler_resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), filler_h),
            egui::Sense::click(),
        );
        ui.painter()
            .rect_filled(filler_rect, 0.0, egui::Color32::TRANSPARENT);
        filler_resp.context_menu(|ui| {
            if ui.button("Server Settings…").clicked() {
                model.show_permissions_center = true;
                model.permissions_tab = crate::ui::model::PermissionsTab::Roles;
                let _ = tx_intent.send(UiIntent::PermsOpen);
                ui.close();
            }
            if ui.button("Permissions…").clicked() {
                model.show_permissions_center = true;
                let _ = tx_intent.send(UiIntent::PermsOpen);
                ui.close();
            }
            ui.separator();
            if ui.button("Create channel").clicked() {
                model.show_create_channel = true;
                model.create_channel_parent_id = None;
                model.create_channel_name.clear();
                model.create_channel_description.clear();
                ui.close();
            }
        });
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
    egui::Window::new(if model.create_channel_parent_id.is_some() {
        "Create Sub-channel"
    } else {
        "Create Channel"
    })
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
                    parent_channel_id: model.create_channel_parent_id.clone(),
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

pub fn show_channel_dialogs(
    ctx: &egui::Context,
    model: &mut UiModel,
    tx_intent: &Sender<UiIntent>,
) {
    if model.show_rename_channel {
        let mut open = true;
        egui::Window::new("Edit Channel")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("New channel name");
                ui.text_edit_singleline(&mut model.rename_channel_name);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        let new_name = model.rename_channel_name.trim().to_string();
                        if !new_name.is_empty() && new_name.len() <= 64 {
                            if let Some(channel_id) = model.rename_channel_target_id.clone() {
                                let _ = tx_intent.send(UiIntent::RenameChannel {
                                    channel_id,
                                    new_name,
                                });
                            }
                            model.show_rename_channel = false;
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        model.show_rename_channel = false;
                    }
                });
            });
        if !open {
            model.show_rename_channel = false;
        }
    }

    if model.show_delete_channel_confirm {
        let mut open = true;
        egui::Window::new("Delete Channel")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("Delete channel? This will also remove all sub-channels.");
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        if let Some(channel_id) = model.delete_channel_target_id.clone() {
                            let _ = tx_intent.send(UiIntent::DeleteChannel { channel_id });
                        }
                        model.show_delete_channel_confirm = false;
                    }
                    if ui.button("Cancel").clicked() {
                        model.show_delete_channel_confirm = false;
                    }
                });
            });
        if !open {
            model.show_delete_channel_confirm = false;
        }
    }
}

fn show_channel(
    ui: &mut egui::Ui,
    ch: &crate::ui::model::ChannelEntry,
    model: &mut UiModel,
    tx_intent: &Sender<UiIntent>,
    all_channels: &[crate::ui::model::ChannelEntry],
) {
    let is_selected = model.selected_channel.as_deref() == Some(ch.id.as_str());
    let children: Vec<_> = all_channels
        .iter()
        .filter(|candidate| candidate.parent_id.as_deref() == Some(ch.id.as_str()))
        .collect();
    let has_children = !children.is_empty();
    let collapsed = *model.channel_collapsed.get(&ch.id).unwrap_or(&false);

    let row_height = ui.spacing().interact_size.y.max(20.0);
    let row_width = ui.available_width().max(1.0);
    let (row_rect, row_response) =
        ui.allocate_exact_size(egui::vec2(row_width, row_height), egui::Sense::click());

    let rounding = egui::CornerRadius::same(4);
    let visuals = ui.visuals();
    if is_selected {
        ui.painter().rect_filled(
            row_rect,
            rounding,
            visuals.selection.bg_fill.linear_multiply(0.45),
        );
    } else if row_response.hovered() {
        ui.painter().rect_filled(
            row_rect,
            rounding,
            visuals.widgets.hovered.bg_fill.linear_multiply(0.35),
        );
    }

    let indent = 20.0;
    let triangle_rect = egui::Rect::from_min_size(
        row_rect.min + egui::vec2(2.0, 0.0),
        egui::vec2(indent - 2.0, row_rect.height()),
    );

    if has_children {
        let icon = if collapsed { "▶" } else { "▼" };
        ui.painter().text(
            triangle_rect.center(),
            egui::Align2::CENTER_CENTER,
            icon,
            egui::FontId::proportional(12.0),
            theme::text_muted(),
        );
    }

    let text_color = if is_selected {
        visuals.strong_text_color()
    } else {
        theme::text_color()
    };
    ui.painter().text(
        row_rect.left_center() + egui::vec2(indent + 2.0, 0.0),
        egui::Align2::LEFT_CENTER,
        &ch.name,
        egui::TextStyle::Button.resolve(ui.style()),
        text_color,
    );

    if row_response.clicked_by(egui::PointerButton::Primary) {
        let clicked_triangle = has_children
            && row_response
                .interact_pointer_pos()
                .is_some_and(|pos| triangle_rect.contains(pos));

        if clicked_triangle {
            model.channel_collapsed.insert(ch.id.clone(), !collapsed);
        } else {
            let _ = tx_intent.send(UiIntent::JoinChannel {
                channel_id: ch.id.clone(),
            });
        }
    }

    #[cfg(debug_assertions)]
    if row_response.secondary_clicked() {
        tracing::debug!(
            target: "ui::channels_panel",
            channel_id = %ch.id,
            channel_name = %ch.name,
            "secondary click captured on channel row"
        );
    }

    row_response.context_menu(|ui| {
        if ui.button("Switch to channel").clicked() {
            let _ = tx_intent.send(UiIntent::JoinChannel {
                channel_id: ch.id.clone(),
            });
            ui.close();
        }
        if ui.button("Edit Channel…").clicked() {
            model.rename_channel_target_id = Some(ch.id.clone());
            model.rename_channel_name = ch.name.clone();
            model.show_rename_channel = true;
            ui.close();
        }
        if ui.button("Permissions…").clicked() {
            model.show_permissions_center = true;
            model.permissions_tab = crate::ui::model::PermissionsTab::Channels;
            model.permissions_channel_scope_name = ch.name.clone();
            model.permissions_selected_channel_id = Some(ch.id.clone());
            let _ = tx_intent.send(UiIntent::PermsOpen);
            ui.close();
        }
        if ui.button("Delete channel").clicked() {
            model.delete_channel_target_id = Some(ch.id.clone());
            model.show_delete_channel_confirm = true;
            ui.close();
        }
        if ui.button("Create sub-channel").clicked() {
            model.show_create_channel = true;
            model.create_channel_parent_id = Some(ch.id.clone());
            model.create_channel_name.clear();
            model.create_channel_description.clear();
            ui.close();
        }
    });

    if has_children && !collapsed {
        ui.indent(ui.id().with(format!("indent-{}", ch.id)), |ui| {
            for child in children {
                show_channel(ui, child, model, tx_intent, all_channels);
            }
        });
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
