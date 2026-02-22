//! Connection telemetry panel.

use crate::ui::model::UiModel;
use crate::ui::theme;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, model: &UiModel) {
    let t = &model.telemetry;

    egui::Grid::new("telemetry_grid")
        .num_columns(2)
        .spacing([20.0, 4.0])
        .show(ui, |ui| {
            ui.label("RTT:");
            ui.label(format!("{} ms", t.rtt_ms));
            ui.end_row();

            ui.label("Packet Loss:");
            let loss_color = if t.loss_rate > 0.05 {
                theme::COLOR_DANGER
            } else if t.loss_rate > 0.01 {
                theme::COLOR_IDLE
            } else {
                theme::COLOR_ONLINE
            };
            ui.colored_label(loss_color, format!("{:.1}%", t.loss_rate * 100.0));
            ui.end_row();

            ui.label("Jitter:");
            ui.label(format!("{} ms", t.jitter_ms));
            ui.end_row();

            ui.label("Goodput:");
            ui.label(format_bitrate(t.goodput_bps));
            ui.end_row();

            ui.label("Playout Delay:");
            ui.label(format!("{} ms", t.playout_delay_ms));
            ui.end_row();

            ui.label("AGC Gain:");
            ui.label(format!("{:.1} dB", t.agc_gain_db));
            ui.end_row();

            ui.label("VAD Probability:");
            let vad_color = if t.vad_probability > 0.5 {
                theme::COLOR_ONLINE
            } else {
                theme::COLOR_TEXT_MUTED
            };
            ui.colored_label(vad_color, format!("{:.0}%", t.vad_probability * 100.0));
            ui.end_row();
        });

    ui.separator();

    // Visual RTT / loss graph (simple bar)
    ui.label(
        egui::RichText::new("Network Quality")
            .strong()
            .size(13.0),
    );

    let quality = compute_quality_score(t.rtt_ms, t.loss_rate, t.jitter_ms);
    let (quality_text, quality_color) = match quality {
        80..=100 => ("Excellent", theme::COLOR_ONLINE),
        60..=79 => ("Good", theme::COLOR_ONLINE),
        40..=59 => ("Fair", theme::COLOR_IDLE),
        20..=39 => ("Poor", theme::COLOR_DND),
        _ => ("Bad", theme::COLOR_DANGER),
    };

    ui.horizontal(|ui| {
        let bar_width = 200.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_width, 16.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, 4.0, egui::Color32::from_gray(40));
        let filled = egui::Rect::from_min_size(
            rect.min,
            egui::vec2(bar_width * quality as f32 / 100.0, 16.0),
        );
        ui.painter().rect_filled(filled, 4.0, quality_color);
        ui.label(
            egui::RichText::new(format!("{quality_text} ({quality}%)"))
                .color(quality_color),
        );
    });
}

fn compute_quality_score(rtt_ms: u32, loss_rate: f32, jitter_ms: u32) -> u32 {
    let mut score = 100i32;

    // RTT penalty
    if rtt_ms > 300 {
        score -= 40;
    } else if rtt_ms > 150 {
        score -= 20;
    } else if rtt_ms > 50 {
        score -= 5;
    }

    // Loss penalty
    let loss_pct = loss_rate * 100.0;
    if loss_pct > 5.0 {
        score -= 40;
    } else if loss_pct > 2.0 {
        score -= 20;
    } else if loss_pct > 0.5 {
        score -= 5;
    }

    // Jitter penalty
    if jitter_ms > 50 {
        score -= 20;
    } else if jitter_ms > 20 {
        score -= 10;
    }

    score.clamp(0, 100) as u32
}

fn format_bitrate(bps: u32) -> String {
    if bps >= 1_000_000 {
        format!("{:.1} Mbps", bps as f64 / 1_000_000.0)
    } else if bps >= 1_000 {
        format!("{:.0} kbps", bps as f64 / 1_000.0)
    } else {
        format!("{bps} bps")
    }
}
