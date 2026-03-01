use crate::ui::model::UiModel;
use crate::ui::theme;
use eframe::egui;

fn parse_raw_rgba(payload: &[u8]) -> Option<(usize, usize, &[u8])> {
    if payload.len() < 8 {
        return None;
    }
    let width = u16::from_le_bytes([payload[0], payload[1]]) as usize;
    let height = u16::from_le_bytes([payload[2], payload[3]]) as usize;
    let stride = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
    if width == 0 || height == 0 || stride < width * 4 {
        return None;
    }
    let needed = stride.checked_mul(height)?;
    if payload.len() < 8 + needed {
        return None;
    }
    Some((width, height, &payload[8..8 + needed]))
}

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
                if let Some(frame) = model.latest_stream_frame.as_ref() {
                    if let Some((width, height, rgba)) = parse_raw_rgba(&frame.payload) {
                        let image = egui::ColorImage::from_rgba_unmultiplied([width, height], rgba);
                        let texture = ui.ctx().load_texture(
                            "streaming.latest",
                            image,
                            egui::TextureOptions::LINEAR,
                        );
                        let available = ui.available_width().min(width as f32);
                        let size =
                            egui::vec2(available, available * (height as f32 / width as f32));
                        ui.add(egui::Image::new((texture.id(), size)));
                    } else {
                        ui.label("waiting for frames...");
                    }
                } else {
                    ui.label("waiting for frames...");
                }
            });
        });
}
