//! Settings panel (floating window).

use crate::ui::model::{UiIntent, UiModel};
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

    egui::CollapsingHeader::new("Voice")
        .default_open(false)
        .show(ui, |ui| {
            ui.label("Input Device:");
            ui.label(
                egui::RichText::new("(system default)")
                    .italics()
                    .color(egui::Color32::GRAY),
            );

            ui.label("Output Device:");
            ui.label(
                egui::RichText::new("(system default)")
                    .italics()
                    .color(egui::Color32::GRAY),
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
