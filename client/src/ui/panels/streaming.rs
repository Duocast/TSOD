use crate::ui::model::UiModel;
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

pub fn show(ui: &mut egui::Ui, model: &mut UiModel) {
    let channel_name = if model.selected_channel_name.is_empty() {
        "Streaming"
    } else {
        &model.selected_channel_name
    };

    ui.horizontal(|ui| {
        ui.heading(egui::RichText::new(format!("📺 {channel_name}")).color(theme::text_color()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let stats_text = if model.show_stream_stats {
                "Hide Stats"
            } else {
                "Stats"
            };
            let response = ui
                .add(egui::Button::new(stats_text))
                .on_hover_text("Toggle stream diagnostics overlay");
            if response.clicked() {
                model.show_stream_stats = !model.show_stream_stats;
            }
        });
    });
    ui.separator();

    let dbg = &model.stream_debug;

    egui::Frame::group(ui.style())
        .fill(theme::bg_dark())
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            let available = ui.available_size();
            let min_h = 260.0;
            let target_h = available.y.max(min_h);
            ui.set_min_height(target_h);

            let (rect, _response) = ui.allocate_exact_size(
                egui::vec2(available.x.max(220.0), target_h),
                egui::Sense::hover(),
            );

            let mut video_rect = rect.shrink2(egui::vec2(8.0, 8.0));
            video_rect.set_height((video_rect.height() - 36.0).max(180.0));

            let painter = ui.painter_at(rect);
            painter.rect_filled(video_rect, 6.0, theme::bg_medium());

            let mut rendered = false;
            let mut render_w = 0usize;
            let mut render_h = 0usize;
            let active_stream_tag = dbg
                .active_stream_tags
                .iter()
                .find(|tag| model.latest_stream_frames.contains_key(tag))
                .copied()
                .or_else(|| {
                    model
                        .latest_stream_frames
                        .iter()
                        .max_by_key(|(_, frame)| frame.frame_seq)
                        .map(|(tag, _)| *tag)
                });

            if let Some(stream_tag) = active_stream_tag {
                if let Some(frame) = model.latest_stream_frames.get(&stream_tag) {
                    let should_present = model
                        .last_presented_stream_frame_key
                        .get(&stream_tag)
                        .map(|last_presented| frame.frame_seq > *last_presented)
                        .unwrap_or(true);
                    if should_present {
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [frame.width, frame.height],
                            &frame.rgba,
                        );
                        model.latest_stream_frame_textures.insert(
                            stream_tag,
                            ui.ctx().load_texture(
                                format!("streaming.latest.{stream_tag}"),
                                image,
                                egui::TextureOptions::LINEAR,
                            ),
                        );
                        model
                            .last_presented_stream_frame_key
                            .insert(stream_tag, frame.frame_seq);
                        render_w = frame.width;
                        render_h = frame.height;
                    }

                    if render_w == 0 || render_h == 0 {
                        if let Some(texture) = model.latest_stream_frame_textures.get(&stream_tag) {
                            render_w = texture.size()[0];
                            render_h = texture.size()[1];
                        }
                    }

                    if render_w > 0 && render_h > 0 {
                        let aspect = render_w as f32 / render_h as f32;
                        let mut draw_size = video_rect.size();
                        if draw_size.x / draw_size.y > aspect {
                            draw_size.x = draw_size.y * aspect;
                        } else {
                            draw_size.y = draw_size.x / aspect;
                        }
                        let draw_rect =
                            egui::Rect::from_center_size(video_rect.center(), draw_size);
                        if let Some(texture) = model.latest_stream_frame_textures.get(&stream_tag) {
                            egui::Image::new((texture.id(), draw_size)).paint_at(ui, draw_rect);
                            rendered = true;
                        }
                    }
                }
            }

            if !dbg.active_stream_tags.is_empty() {
                ui.ctx().request_repaint();
            }

            if !rendered {
                painter.text(
                    video_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Waiting for stream…",
                    egui::FontId::proportional(18.0),
                    theme::text_muted(),
                );
            }

            let controls_rect = egui::Rect::from_min_max(
                egui::pos2(video_rect.left(), video_rect.bottom() + 8.0),
                egui::pos2(video_rect.right(), rect.bottom() - 4.0),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(controls_rect), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "Stream tags: {}",
                        if dbg.active_stream_tags.is_empty() {
                            "(none)".to_string()
                        } else {
                            dbg.active_stream_tags
                                .iter()
                                .map(u64::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    ));
                    ui.separator();
                    ui.label(format!("Frames/s: {}", dbg.completed_frames_per_sec));
                    ui.separator();
                    ui.label(format!(
                        "Tx: {} Kbps",
                        (dbg.video_tx_bytes_per_sec * 8) / 1000
                    ));
                });
            });

            if model.show_stream_stats {
                let stats_w = (video_rect.width() * 0.44).clamp(260.0, 430.0);
                let stats_rect = egui::Rect::from_min_size(
                    egui::pos2(video_rect.right() - stats_w - 8.0, video_rect.top() + 8.0),
                    egui::vec2(stats_w, (video_rect.height() * 0.7).max(170.0)),
                );
                painter.rect_filled(stats_rect, 6.0, egui::Color32::from_black_alpha(212));
                painter.rect_stroke(
                    stats_rect,
                    6.0,
                    egui::Stroke::new(1.0, theme::bg_light()),
                    egui::StrokeKind::Outside,
                );

                ui.scope_builder(
                    egui::UiBuilder::new().max_rect(stats_rect.shrink(10.0)),
                    |ui| {
                        ui.label(
                            egui::RichText::new("Stats")
                                .color(theme::text_color())
                                .strong(),
                        );
                        ui.separator();
                        ui.label(format!("Codecs: {} / {}", dbg.codec_video, dbg.codec_audio));
                        ui.label(format!(
                            "Selected codec/backend: {} / {}",
                            dbg.selected_codec, dbg.selected_backend
                        ));
                        ui.label(format!("Active layer: {}", dbg.active_layer));
                        ui.label(format!(
                            "Advertised profile: {}",
                            dbg.advertised_target_profile
                        ));
                        ui.label(format!(
                            "Rendered resolution: {}",
                            dbg.current_rendered_resolution
                        ));
                        ui.label(format!(
                            "Encode/decode FPS: {:.1} / {:.1}",
                            dbg.encoded_fps, dbg.decoded_fps
                        ));
                        ui.label(format!(
                            "Capture/render FPS: {:.1} / {:.1}",
                            dbg.capture_fps, dbg.rendered_fps
                        ));
                        ui.label(format!(
                            "Queue depth (cap/enc/pkt): {}/{}/{}",
                            dbg.queue_depth_capture,
                            dbg.queue_depth_encode,
                            dbg.queue_depth_packetize
                        ));
                        ui.label(format!(
                            "Tx/Rx bitrate: {} / {} bps",
                            dbg.tx_bitrate_bps, dbg.rx_bitrate_bps
                        ));
                        ui.label(format!(
                            "Encode/decode p95: {:.2} / {:.2} ms",
                            dbg.encode_p95_ms, dbg.decode_p95_ms
                        ));
                        ui.label(format!(
                            "Freeze/recovery: {} / {:.2} ms p95",
                            dbg.freeze_count, dbg.freeze_ms_p95
                        ));
                        ui.label(format!("Backend label: {}", dbg.backend_label));
                        ui.separator();
                        ui.label(format!(
                            "Video rx/tx dgrams: {} / {}",
                            dbg.video_datagrams_per_sec, dbg.video_tx_datagrams_per_sec
                        ));
                        ui.label(format!("Tx blocked/sec: {}", dbg.video_tx_blocked_per_sec));
                        ui.label(format!(
                            "Tx drops (video q/deadline): {}/{}",
                            dbg.video_tx_drop_queue_full, dbg.video_tx_drop_deadline
                        ));
                        ui.label(format!(
                            "Tx drops too-large (voice/video): {}/{}",
                            dbg.voice_tx_drop_too_large, dbg.video_tx_drop_too_large
                        ));
                        ui.label(format!(
                            "Drops (no sub/channel full): {}/{}",
                            dbg.dropped_no_subscription, dbg.dropped_channel_full
                        ));
                        ui.label(format!("Sender frame errors: {}", dbg.sender_frame_errors));
                        ui.label(format!(
                            "Last frame: seq={} ts_ms={} size={} B",
                            dbg.last_frame_seq, dbg.last_frame_ts_ms, dbg.last_frame_size_bytes
                        ));
                    },
                );
            }
        });
}
