use crate::ui::model::UiModel;
use crate::ui::theme;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, model: &UiModel) {
    let channel_name = if model.selected_channel_name.is_empty() {
        "Streaming"
    } else {
        &model.selected_channel_name
    };

    ui.horizontal(|ui| {
        ui.heading(egui::RichText::new(format!("📺 {channel_name}")).color(theme::text_color()));
    });
    ui.separator();

    let dbg = &model.stream_debug;
    ui.add_space(8.0);
    egui::Frame::group(ui.style())
        .fill(theme::bg_dark())
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_min_height((ui.available_height() - 8.0).max(260.0));
            ui.vertical(|ui| {
                let tags = if dbg.active_stream_tags.is_empty() {
                    "(none)".to_string()
                } else {
                    dbg.active_stream_tags
                        .iter()
                        .map(|t| t.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                ui.label(
                    egui::RichText::new("Live stream activity")
                        .strong()
                        .size(20.0),
                );
                ui.add_space(6.0);
                ui.label(format!("Active stream tags: {tags}"));
                ui.label(format!(
                    "Video datagrams/sec: {} | Completed frames/sec: {}",
                    dbg.video_datagrams_per_sec, dbg.completed_frames_per_sec
                ));
                ui.label(format!(
                    "Drops (no subscription): {} | Drops (channel full): {}",
                    dbg.dropped_no_subscription, dbg.dropped_channel_full
                ));
                ui.label(format!(
                    "Last frame: seq={} ts_ms={} size={} bytes",
                    dbg.last_frame_seq, dbg.last_frame_ts_ms, dbg.last_frame_size_bytes
                ));
                ui.add_space(8.0);

                let available = ui.available_width().max(200.0);
                let ratio = (dbg.completed_frames_per_sec.min(60) as f32) / 60.0;
                ui.add(
                    egui::ProgressBar::new(ratio)
                        .desired_width(available)
                        .text(format!("frame activity {:.0}%", ratio * 100.0)),
                );
            });
        });
}
