use crate::net::video_decode;
use crate::proto::voiceplatform::v1 as pb;
use crate::ui::model::{StreamView, UiModel};
use crate::ui::theme;
use eframe::egui;

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn codec_label(codec: pb::VideoCodec) -> &'static str {
    match codec {
        pb::VideoCodec::Av1 => "AV1",
        pb::VideoCodec::Vp9 => "VP9",
        pb::VideoCodec::Vp8 => "VP8",
        _ => "Unknown",
    }
}

fn decode_texture(ui: &egui::Ui, stream: &mut StreamView) {
    let Some(frame) = stream.latest_frame.as_ref() else {
        return;
    };
    let frame_key = Some((frame.stream_tag, frame.frame_seq));
    if stream.texture_key == frame_key {
        return;
    }
    if let Ok(codec) = pb::VideoCodec::try_from(frame.codec) {
        if let Ok(decoded) = video_decode::decode_video_frame(codec, &frame.payload) {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [decoded.width, decoded.height],
                &decoded.rgba,
            );
            stream.texture = Some(ui.ctx().load_texture(
                format!("streaming.{}", stream.stream_tag),
                image,
                egui::TextureOptions::LINEAR,
            ));
            stream.texture_key = frame_key;
        }
    }
}

fn paint_stream_tile(
    ui: &mut egui::Ui,
    stream: &mut StreamView,
    show_stats: bool,
    viewer_count: usize,
    focused: bool,
) -> egui::Response {
    decode_texture(ui, stream);
    let desired = if focused {
        egui::vec2(ui.available_width(), ui.available_height().max(240.0))
    } else {
        egui::vec2((ui.available_width() / 2.0 - 6.0).max(220.0), 210.0)
    };
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, theme::bg_medium());

    let video_rect = rect.shrink2(egui::vec2(8.0, 8.0));
    let mut video_rect = egui::Rect::from_min_max(
        video_rect.min,
        egui::pos2(video_rect.max.x, video_rect.max.y - 48.0),
    );
    let mut rendered = false;
    let mut frame_size = (0, 0);

    if let Some(texture) = &stream.texture {
        frame_size = (texture.size()[0], texture.size()[1]);
        let aspect = frame_size.0 as f32 / frame_size.1 as f32;
        let mut draw_size = video_rect.size();
        if draw_size.x / draw_size.y > aspect {
            draw_size.x = draw_size.y * aspect;
        } else {
            draw_size.y = draw_size.x / aspect;
        }
        let draw_rect = egui::Rect::from_center_size(video_rect.center(), draw_size);
        egui::Image::new((texture.id(), draw_size)).paint_at(ui, draw_rect);
        rendered = true;
    }

    if !rendered {
        painter.text(
            video_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Waiting for stream…",
            egui::FontId::proportional(14.0),
            theme::text_muted(),
        );
    }

    let info = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 8.0, rect.bottom() - 36.0),
        egui::pos2(rect.right() - 8.0, rect.bottom() - 8.0),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(info), |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Tag {}", stream.stream_tag));
            ui.separator();
            ui.label(format!("{}", codec_label(stream.codec)));
            ui.separator();
            if frame_size.0 > 0 {
                ui.label(format!("{}x{}", frame_size.0, frame_size.1));
            } else {
                ui.label("res n/a");
            }
            ui.separator();
            ui.label(format!("Viewers: {}", viewer_count));
        });
        if show_stats {
            ui.small(format!("Last frame at: {} ms", stream.last_frame_at_ms));
        }
    });

    response
}

pub fn show(ui: &mut egui::Ui, model: &mut UiModel) {
    let channel_name = if model.selected_channel_name.is_empty() {
        "Streaming"
    } else {
        &model.selected_channel_name
    };
    ui.horizontal(|ui| {
        ui.heading(egui::RichText::new(format!("📺 {channel_name}")).color(theme::text_color()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(if model.show_stream_stats {
                    "Hide Stats"
                } else {
                    "Stats"
                })
                .clicked()
            {
                model.show_stream_stats = !model.show_stream_stats;
            }
        });
    });
    ui.separator();

    if model.streams.is_empty() {
        ui.add_space(16.0);
        ui.label(egui::RichText::new("No active streams").color(theme::text_muted()));
        return;
    }

    if model
        .focused_stream_tag
        .is_some_and(|tag| !model.streams.contains_key(&tag))
    {
        model.focused_stream_tag = model.streams.keys().next().copied();
    }

    let viewer_count = model
        .selected_channel
        .as_ref()
        .and_then(|channel_id| model.members.get(channel_id))
        .map(|members| members.iter().filter(|m| m.streaming).count())
        .unwrap_or(0);

    if let Some(tag) = model.focused_stream_tag {
        ui.horizontal(|ui| {
            if ui.button("Back to grid").clicked() {
                model.focused_stream_tag = None;
            }
            ui.label(format!("Focused stream: {tag}"));
        });
        if let Some(stream) = model.streams.get_mut(&tag) {
            let response =
                paint_stream_tile(ui, stream, model.show_stream_stats, viewer_count, true);
            if response.double_clicked() {
                model.focused_stream_tag = None;
            }
        }
        return;
    }

    let tags: Vec<u64> = model.streams.keys().copied().collect();
    egui::Grid::new("stream-grid")
        .num_columns(2)
        .spacing(egui::vec2(12.0, 12.0))
        .show(ui, |ui| {
            for (idx, tag) in tags.iter().enumerate() {
                if let Some(stream) = model.streams.get_mut(tag) {
                    let response =
                        paint_stream_tile(ui, stream, model.show_stream_stats, viewer_count, false);
                    if response.clicked() || response.double_clicked() {
                        model.focused_stream_tag = Some(*tag);
                    }
                }
                if idx % 2 == 1 {
                    ui.end_row();
                }
            }
        });

    let dbg = &model.stream_debug;
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("Frames/s: {}", dbg.completed_frames_per_sec));
        ui.separator();
        ui.label(format!(
            "Tx: {} Kbps",
            (dbg.video_tx_bytes_per_sec * 8) / 1000
        ));
        ui.separator();
        ui.label(format!(
            "Network: {}",
            human_bytes(dbg.network_activity_bytes)
        ));
    });
}
