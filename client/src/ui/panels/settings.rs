//! Settings panel (floating window).

use crate::ui::model::{UiIntent, UiModel};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    egui::CollapsingHeader::new("Audio Processing")
        .default_open(true)
        .show(ui, |ui| {
            // Noise suppression
            let mut ns = model.noise_suppression_enabled;
            if ui.checkbox(&mut ns, "Noise Suppression (RNNoise)").changed() {
                model.noise_suppression_enabled = ns;
                let _ = tx_intent.send(UiIntent::SetNoiseSuppression(ns));
            }

            // AGC
            let mut agc = model.agc_enabled;
            if ui.checkbox(&mut agc, "Automatic Gain Control").changed() {
                model.agc_enabled = agc;
                let _ = tx_intent.send(UiIntent::SetAgcEnabled(agc));
            }

            // VAD threshold
            ui.horizontal(|ui| {
                ui.label("VAD Threshold:");
                let mut vad = model.vad_threshold;
                if ui
                    .add(egui::Slider::new(&mut vad, 0.0..=1.0).step_by(0.05))
                    .changed()
                {
                    model.vad_threshold = vad;
                    let _ = tx_intent.send(UiIntent::SetVadThreshold(vad));
                }
            });

            // PTT toggle
            ui.horizontal(|ui| {
                ui.label("Push to Talk:");
                let mut ptt = model.ptt_enabled;
                if ui.checkbox(&mut ptt, "").changed() {
                    model.ptt_enabled = ptt;
                    let _ = tx_intent.send(UiIntent::TogglePtt);
                }
            });
        });

    egui::CollapsingHeader::new("Volume / Gain")
        .default_open(true)
        .show(ui, |ui| {
            // Input gain slider
            ui.horizontal(|ui| {
                ui.label("Input Volume:");
                let pct = (model.input_gain * 100.0).round() as i32;
                ui.label(
                    egui::RichText::new(format!("{pct}%"))
                        .small()
                        .color(theme::COLOR_TEXT_DIM),
                );
            });
            let mut input_gain = model.input_gain;
            if ui.add(
                egui::Slider::new(&mut input_gain, 0.0..=2.0)
                    .step_by(0.01)
                    .show_value(false),
            ).changed() {
                model.input_gain = input_gain;
                let _ = tx_intent.send(UiIntent::SetInputGain(input_gain));
            }

            ui.add_space(4.0);

            // Output gain slider
            ui.horizontal(|ui| {
                ui.label("Output Volume:");
                let pct = (model.output_gain * 100.0).round() as i32;
                ui.label(
                    egui::RichText::new(format!("{pct}%"))
                        .small()
                        .color(theme::COLOR_TEXT_DIM),
                );
            });
            let mut output_gain = model.output_gain;
            if ui.add(
                egui::Slider::new(&mut output_gain, 0.0..=2.0)
                    .step_by(0.01)
                    .show_value(false),
            ).changed() {
                model.output_gain = output_gain;
                let _ = tx_intent.send(UiIntent::SetOutputGain(output_gain));
            }

            ui.add_space(6.0);

            // Reset button
            if ui.small_button("Reset to 100%").clicked() {
                model.input_gain = 1.0;
                model.output_gain = 1.0;
                let _ = tx_intent.send(UiIntent::SetInputGain(1.0));
                let _ = tx_intent.send(UiIntent::SetOutputGain(1.0));
            }
        });

    egui::CollapsingHeader::new("Mic Test")
        .default_open(false)
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new("Test your microphone with the full DSP pipeline active.")
                    .small()
                    .color(theme::COLOR_TEXT_DIM),
            );
            ui.add_space(4.0);

            let btn_text = if model.loopback_active {
                "Stop Loopback"
            } else {
                "Start Loopback"
            };
            let btn_color = if model.loopback_active {
                theme::COLOR_DANGER
            } else {
                theme::COLOR_ACCENT
            };

            if ui.add(
                egui::Button::new(
                    egui::RichText::new(btn_text).color(egui::Color32::WHITE).strong(),
                )
                .fill(btn_color)
                .min_size(egui::vec2(140.0, 28.0))
                .rounding(4.0),
            ).clicked() {
                model.loopback_active = !model.loopback_active;
                let _ = tx_intent.send(UiIntent::ToggleLoopback);
            }

            // VU meter when loopback is active
            if model.loopback_active {
                ui.add_space(4.0);
                if let Some(vad) = model.vad_level {
                    let bar_width = ui.available_width().min(250.0);
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(bar_width, 10.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(rect, 3.0, theme::COLOR_BG_DARK);
                    let filled = egui::Rect::from_min_size(
                        rect.min,
                        egui::vec2(bar_width * vad, 10.0),
                    );
                    let color = if vad > 0.7 {
                        theme::COLOR_DANGER
                    } else if vad > 0.3 {
                        theme::COLOR_ONLINE
                    } else {
                        theme::COLOR_IDLE
                    };
                    ui.painter().rect_filled(filled, 3.0, color);
                }
                ui.label(
                    egui::RichText::new("Loopback active — you should hear yourself")
                        .small()
                        .color(theme::COLOR_ONLINE),
                );
            }
        });

    egui::CollapsingHeader::new("Voice Devices")
        .default_open(false)
        .show(ui, |ui| {
            // Input device dropdown
            ui.label("Input Device:");
            let input_selected = model.selected_input_device.clone();
            egui::ComboBox::from_id_salt("input_device")
                .selected_text(&input_selected)
                .width(250.0)
                .show_ui(ui, |ui| {
                    if ui.selectable_value(
                        &mut model.selected_input_device,
                        "(system default)".to_string(),
                        "(system default)",
                    ).clicked() {
                        let _ = tx_intent.send(UiIntent::SetInputDevice("(system default)".into()));
                    }
                    for dev_name in &model.input_devices {
                        if ui.selectable_value(
                            &mut model.selected_input_device,
                            dev_name.clone(),
                            dev_name,
                        ).clicked() {
                            let _ = tx_intent.send(UiIntent::SetInputDevice(dev_name.clone()));
                        }
                    }
                });

            ui.add_space(4.0);

            // Output device dropdown
            ui.label("Output Device:");
            let output_selected = model.selected_output_device.clone();
            egui::ComboBox::from_id_salt("output_device")
                .selected_text(&output_selected)
                .width(250.0)
                .show_ui(ui, |ui| {
                    if ui.selectable_value(
                        &mut model.selected_output_device,
                        "(system default)".to_string(),
                        "(system default)",
                    ).clicked() {
                        let _ = tx_intent.send(UiIntent::SetOutputDevice("(system default)".into()));
                    }
                    for dev_name in &model.output_devices {
                        if ui.selectable_value(
                            &mut model.selected_output_device,
                            dev_name.clone(),
                            dev_name,
                        ).clicked() {
                            let _ = tx_intent.send(UiIntent::SetOutputDevice(dev_name.clone()));
                        }
                    }
                });

            ui.add_space(4.0);

            let device_count = model.input_devices.len() + model.output_devices.len();
            ui.label(
                egui::RichText::new(format!(
                    "{} device(s) detected (auto-refreshing)",
                    device_count,
                ))
                .small()
                .color(theme::COLOR_TEXT_MUTED),
            );
        });

    egui::CollapsingHeader::new("Connection")
        .default_open(false)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Status:");
                if model.connected {
                    ui.colored_label(egui::Color32::GREEN, "Connected");
                } else {
                    ui.colored_label(egui::Color32::RED, "Disconnected");
                }
            });
        });

    ui.separator();

    // Log viewer (collapsible)
    egui::CollapsingHeader::new("Debug Log")
        .default_open(false)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(200.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in model.log.iter() {
                        ui.label(
                            egui::RichText::new(line)
                                .small()
                                .monospace()
                                .color(egui::Color32::GRAY),
                        );
                    }
                });
        });
}
