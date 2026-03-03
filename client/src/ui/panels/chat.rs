//! Chat panel: message display, input bar, typing indicators, Discord-like drag overlay.

use crate::ui::model::{AttachmentData, ChatMessage, PendingAttachment, UiIntent, UiModel};
use crate::ui::theme;
use chrono::{Days, Local, NaiveDate, TimeZone};
use crossbeam_channel::Sender;
use eframe::egui;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::debug;

const MESSAGE_GROUP_WINDOW_MS: i64 = 5 * 60 * 1000;
const MAX_PREVIEW_IMAGE_SIZE: f32 = 240.0;
const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
const ALLOWED_ATTACHMENT_MIME: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/avif",
    "video/mp4",
    "video/webm",
];

/// Duration the overlay stays fully visible after a drop.
const OVERLAY_HOLD_MS: u64 = 600;
/// Duration of the fade-out after the hold period.
const OVERLAY_FADE_MS: u64 = 400;

/// Height of a single attachment preview card in the composer strip.
const PREVIEW_CARD_HEIGHT: f32 = 86.0;

pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    let chat_rect = ui.max_rect();
    let shift_held = ui.ctx().input(|i| i.modifiers.shift);

    // Handle drag-and-drop (overlay state + file collection + shift-drop)
    handle_drag_and_drop(ui.ctx(), model, tx_intent, chat_rect, shift_held);

    // Channel header
    ui.horizontal(|ui| {
        let ch_name = if model.selected_channel_name.is_empty() {
            "Select a channel"
        } else {
            &model.selected_channel_name
        };
        ui.heading(egui::RichText::new(format!("# {ch_name}")).color(theme::text_color()));
    });
    ui.separator();

    // Reserve space for bottom area: typing + separator + preview strip + input
    let preview_height = if model.pending_attachments.is_empty() {
        0.0
    } else {
        PREVIEW_CARD_HEIGHT + 12.0
    };
    let available = ui.available_height() - 64.0 - preview_height;

    // Messages area
    egui::ScrollArea::vertical()
        .max_height(available.max(100.0))
        .stick_to_bottom(true)
        .show(ui, |ui| {
            if let Some(messages) = model.current_messages() {
                let mut prev_msg: Option<&ChatMessage> = None;
                let mut prev_day: Option<NaiveDate> = None;

                for msg in messages.iter() {
                    let msg_day = message_day(msg.timestamp);
                    if msg_day.is_some() && msg_day != prev_day {
                        show_date_separator(ui, msg_day.unwrap());
                    }

                    let grouped = prev_msg.is_some_and(|prev| should_group_messages(prev, msg));
                    show_message(ui, msg, grouped, tx_intent);

                    prev_msg = Some(msg);
                    prev_day = msg_day;
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("No messages yet. Start the conversation!")
                            .color(theme::text_muted())
                            .italics(),
                    );
                });
            }
        });

    // Typing indicator
    let typing = model.current_typing_users();
    if !typing.is_empty() {
        let text = match typing.len() {
            1 => format!("{} is typing...", typing[0]),
            2 => format!("{} and {} are typing...", typing[0], typing[1]),
            _ => format!(
                "{} and {} others are typing...",
                typing[0],
                typing.len() - 1
            ),
        };
        ui.label(
            egui::RichText::new(text)
                .small()
                .color(theme::text_muted())
                .italics(),
        );
    }

    let lower_input_spacer = (ui.available_height() - 34.0 - preview_height).max(2.0);
    ui.add_space(lower_input_spacer);
    ui.separator();

    // Discord-like attachment preview strip (above the input bar)
    show_attachment_preview_strip(ui, model);

    // Input bar
    ui.horizontal(|ui| {
        let hint = if !model.pending_attachments.is_empty() {
            "Add a comment..."
        } else {
            "Type a message..."
        };
        let response = ui.add(
            egui::TextEdit::singleline(&mut model.chat_input)
                .hint_text(hint)
                .desired_width(ui.available_width() - 70.0)
                .frame(true),
        );
        response.context_menu(|ui| {
            if ui.button("Paste").clicked() {
                // RequestPaste inserts into the currently focused widget.
                // Right-clicking does not always keep focus on the text edit,
                // so re-focus it before requesting the paste.
                response.request_focus();
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                ui.close();
            }
        });

        model.chat_input_focused = response.has_focus();

        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            send_chat_from_input(model, tx_intent);
            response.request_focus();
        }

        if ui.button("Send").clicked() {
            send_chat_from_input(model, tx_intent);
        }
    });

    // === Overlays (painted on top of everything) ===
    show_drag_overlay(ui, model, chat_rect);
    show_notifications(ui, model);
}

// ── Drag-and-drop handling ──────────────────────────────────────────────

fn handle_drag_and_drop(
    ctx: &egui::Context,
    model: &mut UiModel,
    tx_intent: &Sender<UiIntent>,
    chat_rect: egui::Rect,
    shift_held: bool,
) {
    let (hovered_files, dropped_files, pointer_pos) = ctx.input(|i| {
        (
            i.raw.hovered_files.clone(),
            i.raw.dropped_files.clone(),
            i.pointer.hover_pos().or_else(|| i.pointer.latest_pos()),
        )
    });

    // Check if pointer is inside the chat panel area
    let pointer_in_chat = pointer_pos.is_some_and(|pos| chat_rect.contains(pos));
    let was_drag_hovering = model.drag_hovering;

    // Update hover state: files hovering AND pointer over chat panel
    model.drag_hovering = !hovered_files.is_empty() && pointer_in_chat;

    // Process dropped files
    let drop_targeted_chat = pointer_in_chat || (!dropped_files.is_empty() && was_drag_hovering);

    if !dropped_files.is_empty() && drop_targeted_chat {
        // Set overlay hold timer (visible briefly after drop)
        model.drag_overlay_until =
            Some(Instant::now() + Duration::from_millis(OVERLAY_HOLD_MS + OVERLAY_FADE_MS));

        let mut added_any = false;
        for dropped in &dropped_files {
            let Some(ref path) = dropped.path else {
                continue;
            };
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("attachment")
                .to_string();
            let mime_type = detect_mime_type(path, &dropped.mime);
            let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

            let mut error = None;
            if !ALLOWED_ATTACHMENT_MIME.contains(&mime_type.as_str()) {
                error = Some("Unsupported file type".to_string());
            } else if size_bytes > MAX_ATTACHMENT_BYTES {
                error = Some("File exceeds 25MB limit".to_string());
            }

            model.pending_attachments.push(PendingAttachment {
                path: path.to_string_lossy().to_string(),
                filename,
                mime_type,
                size_bytes,
                error,
            });
            added_any = true;
        }

        // Shift-drop: immediately send (if no validation errors)
        if shift_held && added_any {
            send_chat_from_input(model, tx_intent);
        }

        // Request repaint for overlay animation
        ctx.request_repaint();
    }

    // Keep repainting while overlay is visible (for fade animation)
    if model.drag_hovering || model.drag_overlay_until.is_some_and(|t| Instant::now() < t) {
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

// ── Discord-like drag overlay ───────────────────────────────────────────

fn show_drag_overlay(ui: &mut egui::Ui, model: &mut UiModel, chat_rect: egui::Rect) {
    let now = Instant::now();

    // Compute overlay alpha
    let alpha = if model.drag_hovering {
        1.0_f32
    } else if let Some(until) = model.drag_overlay_until {
        if now >= until {
            model.drag_overlay_until = None;
            return;
        }
        let remaining = until.duration_since(now).as_millis() as f32;
        let fade_ms = OVERLAY_FADE_MS as f32;
        if remaining > fade_ms {
            1.0 // Still in the hold period
        } else {
            remaining / fade_ms // Fading out
        }
    } else {
        return; // No overlay to show
    };

    let painter = ui.painter();

    // Dim the entire chat panel with semi-transparent dark overlay
    let dim_color = egui::Color32::from_black_alpha((180.0 * alpha) as u8);
    painter.rect_filled(chat_rect, 0.0, dim_color);

    // Center card dimensions
    let card_width = 360.0_f32.min(chat_rect.width() - 40.0);
    let card_height = 200.0;
    let card_rect =
        egui::Rect::from_center_size(chat_rect.center(), egui::vec2(card_width, card_height));

    // Card background
    let card_bg = egui::Color32::from_rgba_premultiplied(
        (43.0 * alpha) as u8,
        (45.0 * alpha) as u8,
        (49.0 * alpha) as u8,
        (240.0 * alpha) as u8,
    );
    let card_rounding = egui::CornerRadius::same(12);
    painter.rect_filled(card_rect, card_rounding, card_bg);

    // Dashed border
    let dash_color = egui::Color32::from_rgba_premultiplied(
        (88.0 * alpha) as u8,
        (101.0 * alpha) as u8,
        (242.0 * alpha) as u8,
        (200.0 * alpha) as u8,
    );
    let inset = card_rect.shrink(6.0);
    draw_dashed_rect(painter, inset, 8.0, dash_color, 10.0, 6.0, 2.0);

    // Channel name
    let channel_name = if model.selected_channel_name.is_empty() {
        "channel".to_string()
    } else {
        model.selected_channel_name.clone()
    };

    // Upload icon (large centered arrow)
    let icon_y = card_rect.center().y - 40.0;
    painter.text(
        egui::pos2(card_rect.center().x, icon_y),
        egui::Align2::CENTER_CENTER,
        "\u{2B06}", // upward arrow
        egui::FontId::proportional(36.0),
        egui::Color32::from_rgba_premultiplied(
            (88.0 * alpha) as u8,
            (101.0 * alpha) as u8,
            (242.0 * alpha) as u8,
            (255.0 * alpha) as u8,
        ),
    );

    // Headline: "Upload to #channel-name"
    let headline_y = card_rect.center().y + 10.0;
    painter.text(
        egui::pos2(card_rect.center().x, headline_y),
        egui::Align2::CENTER_CENTER,
        format!("Upload to #{channel_name}"),
        egui::FontId::proportional(18.0),
        egui::Color32::from_rgba_premultiplied(
            (219.0 * alpha) as u8,
            (222.0 * alpha) as u8,
            (225.0 * alpha) as u8,
            (255.0 * alpha) as u8,
        ),
    );

    // Subtext
    let sub_y = headline_y + 24.0;
    painter.text(
        egui::pos2(card_rect.center().x, sub_y),
        egui::Align2::CENTER_CENTER,
        "You can add comments before uploading.",
        egui::FontId::proportional(13.0),
        egui::Color32::from_rgba_premultiplied(
            (148.0 * alpha) as u8,
            (155.0 * alpha) as u8,
            (164.0 * alpha) as u8,
            (255.0 * alpha) as u8,
        ),
    );

    // Hint
    let hint_y = sub_y + 20.0;
    painter.text(
        egui::pos2(card_rect.center().x, hint_y),
        egui::Align2::CENTER_CENTER,
        "Hold Shift to upload directly.",
        egui::FontId::proportional(11.0),
        egui::Color32::from_rgba_premultiplied(
            (96.0 * alpha) as u8,
            (100.0 * alpha) as u8,
            (108.0 * alpha) as u8,
            (220.0 * alpha) as u8,
        ),
    );
}

/// Draw a dashed rectangle outline (straight sides, no corner arcs).
fn draw_dashed_rect(
    painter: &egui::Painter,
    rect: egui::Rect,
    rounding: f32,
    color: egui::Color32,
    dash_len: f32,
    gap_len: f32,
    width: f32,
) {
    let stroke = egui::Stroke::new(width, color);
    let r = rounding;

    // Top side
    draw_dashed_line(
        painter,
        egui::pos2(rect.left() + r, rect.top()),
        egui::pos2(rect.right() - r, rect.top()),
        dash_len,
        gap_len,
        stroke,
    );
    // Bottom side
    draw_dashed_line(
        painter,
        egui::pos2(rect.left() + r, rect.bottom()),
        egui::pos2(rect.right() - r, rect.bottom()),
        dash_len,
        gap_len,
        stroke,
    );
    // Left side
    draw_dashed_line(
        painter,
        egui::pos2(rect.left(), rect.top() + r),
        egui::pos2(rect.left(), rect.bottom() - r),
        dash_len,
        gap_len,
        stroke,
    );
    // Right side
    draw_dashed_line(
        painter,
        egui::pos2(rect.right(), rect.top() + r),
        egui::pos2(rect.right(), rect.bottom() - r),
        dash_len,
        gap_len,
        stroke,
    );

    // Rounded corners using small arc segments
    let segments = 6;
    for corner in 0..4 {
        let (cx, cy, angle_start) = match corner {
            0 => (rect.left() + r, rect.top() + r, std::f32::consts::PI),
            1 => (
                rect.right() - r,
                rect.top() + r,
                std::f32::consts::FRAC_PI_2 * 3.0,
            ),
            2 => (rect.right() - r, rect.bottom() - r, 0.0),
            _ => (
                rect.left() + r,
                rect.bottom() - r,
                std::f32::consts::FRAC_PI_2,
            ),
        };
        let angle_step = std::f32::consts::FRAC_PI_2 / segments as f32;
        for s in (0..segments).step_by(2) {
            let a0 = angle_start + s as f32 * angle_step;
            let a1 = angle_start + (s + 1).min(segments) as f32 * angle_step;
            let p0 = egui::pos2(cx + r * a0.cos(), cy - r * a0.sin());
            let p1 = egui::pos2(cx + r * a1.cos(), cy - r * a1.sin());
            painter.line_segment([p0, p1], stroke);
        }
    }
}

fn draw_dashed_line(
    painter: &egui::Painter,
    start: egui::Pos2,
    end: egui::Pos2,
    dash_len: f32,
    gap_len: f32,
    stroke: egui::Stroke,
) {
    let delta = end - start;
    let total = delta.length();
    if total < 0.1 {
        return;
    }
    let dir = delta / total;
    let step = dash_len + gap_len;
    let mut t = 0.0;
    while t < total {
        let a = start + dir * t;
        let b = start + dir * (t + dash_len).min(total);
        painter.line_segment([a, b], stroke);
        t += step;
    }
}

// ── Attachment preview strip (Discord-like, above input) ────────────────

fn show_attachment_preview_strip(ui: &mut egui::Ui, model: &mut UiModel) {
    if model.pending_attachments.is_empty() {
        return;
    }

    let mut remove_idx: Option<usize> = None;

    egui::Frame::default()
        .fill(theme::bg_medium())
        .inner_margin(egui::Margin::symmetric(8, 6))
        .outer_margin(egui::Margin {
            left: 0,
            right: 0,
            top: 0,
            bottom: 4,
        })
        .corner_radius(egui::CornerRadius {
            nw: 8,
            ne: 8,
            sw: 0,
            se: 0,
        })
        .show(ui, |ui: &mut egui::Ui| {
            ui.horizontal(|ui: &mut egui::Ui| {
                for (idx, file) in model.pending_attachments.iter().enumerate() {
                    let is_image = file.mime_type.starts_with("image/");
                    let has_error = file.error.is_some();

                    // Card frame for each attachment
                    let card_fill = if has_error {
                        theme::COLOR_DANGER.linear_multiply(0.15)
                    } else {
                        theme::bg_light()
                    };
                    let card_stroke = if has_error {
                        egui::Stroke::new(1.0, theme::COLOR_DANGER.linear_multiply(0.5))
                    } else {
                        egui::Stroke::NONE
                    };

                    egui::Frame::default()
                        .fill(card_fill)
                        .stroke(card_stroke)
                        .inner_margin(egui::Margin::same(4))
                        .corner_radius(egui::CornerRadius::same(6))
                        .show(ui, |ui: &mut egui::Ui| {
                            ui.set_max_height(PREVIEW_CARD_HEIGHT - 12.0);

                            if is_image && !has_error {
                                // Image thumbnail preview
                                ui.vertical(|ui: &mut egui::Ui| {
                                    let uri = format!("file://{}", file.path);
                                    let image = egui::Image::from_uri(uri)
                                        .max_width(80.0)
                                        .max_height(52.0)
                                        .maintain_aspect_ratio(true)
                                        .corner_radius(egui::CornerRadius::same(4));
                                    ui.add(image);

                                    // Filename + remove button row
                                    ui.horizontal(|ui: &mut egui::Ui| {
                                        ui.label(
                                            egui::RichText::new(truncate_filename(
                                                &file.filename,
                                                14,
                                            ))
                                            .small()
                                            .color(theme::text_dim()),
                                        );
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new("\u{2715}")
                                                        .small()
                                                        .color(theme::text_muted()),
                                                )
                                                .small()
                                                .frame(false),
                                            )
                                            .clicked()
                                        {
                                            remove_idx = Some(idx);
                                        }
                                    });
                                });
                            } else {
                                // File/video card or errored attachment
                                ui.vertical(|ui: &mut egui::Ui| {
                                    let icon = if file.mime_type.starts_with("video/") {
                                        "\u{1F3AC}" // movie clapper
                                    } else {
                                        "\u{1F4CE}" // paperclip
                                    };
                                    ui.label(
                                        egui::RichText::new(icon)
                                            .size(22.0)
                                            .color(theme::text_dim()),
                                    );

                                    ui.label(
                                        egui::RichText::new(truncate_filename(&file.filename, 16))
                                            .small()
                                            .color(if has_error {
                                                theme::COLOR_DANGER
                                            } else {
                                                theme::text_color()
                                            }),
                                    );

                                    let detail = file
                                        .error
                                        .clone()
                                        .unwrap_or_else(|| format_size(file.size_bytes));
                                    ui.label(egui::RichText::new(detail).small().color(
                                        if has_error {
                                            theme::COLOR_DANGER
                                        } else {
                                            theme::text_muted()
                                        },
                                    ));

                                    ui.horizontal(|ui: &mut egui::Ui| {
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new("\u{2715} Remove")
                                                        .small()
                                                        .color(theme::text_muted()),
                                                )
                                                .small()
                                                .frame(false),
                                            )
                                            .clicked()
                                        {
                                            remove_idx = Some(idx);
                                        }
                                    });
                                });
                            }
                        });

                    // Small spacing between cards
                    ui.add_space(4.0);
                }
            });
        });

    if let Some(idx) = remove_idx {
        model.pending_attachments.remove(idx);
    }
}

fn truncate_filename(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        return name.to_string();
    }
    // Keep extension visible
    if let Some(dot) = name.rfind('.') {
        let ext = &name[dot..];
        let prefix_len = max_len.saturating_sub(ext.len() + 2);
        if prefix_len > 0 {
            return format!("{}..{ext}", &name[..prefix_len]);
        }
    }
    format!("{}..", &name[..max_len.saturating_sub(2)])
}

// ── Send logic ──────────────────────────────────────────────────────────

fn send_chat_from_input(model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    let text = model.chat_input.trim().to_string();
    if text.is_empty() && model.pending_attachments.is_empty() {
        return;
    }

    if model.pending_attachments.iter().any(|a| a.error.is_some()) {
        return;
    }

    let attachments = model
        .pending_attachments
        .iter()
        .map(|a| AttachmentData {
            asset_id: a.path.clone(),
            filename: a.filename.clone(),
            mime_type: a.mime_type.clone(),
            size_bytes: a.size_bytes,
            download_url: String::new(),
            thumbnail_url: None,
        })
        .collect::<Vec<_>>();

    let _ = tx_intent.send(UiIntent::SendChat { text, attachments });
    model.chat_input.clear();
    model.pending_attachments.clear();
    model.clear_current_draft();
}

// ── Message rendering (unchanged) ───────────────────────────────────────

fn detect_mime_type(path: &Path, raw_mime: &str) -> String {
    if !raw_mime.is_empty() {
        return raw_mime.to_string();
    }

    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "avif" => "image/avif",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn show_message(ui: &mut egui::Ui, msg: &ChatMessage, grouped: bool, tx_intent: &Sender<UiIntent>) {
    ui.horizontal(|ui| {
        if !grouped {
            // Full message with author + timestamp
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&msg.author_name)
                            .strong()
                            .color(theme::text_color()),
                    );
                    let ts = format_timestamp(msg.timestamp);
                    ui.label(egui::RichText::new(ts).small().color(theme::text_muted()));
                    if msg.edited {
                        ui.label(
                            egui::RichText::new("(edited)")
                                .small()
                                .color(theme::text_muted()),
                        );
                    }
                    if msg.pinned {
                        ui.label(egui::RichText::new("\u{1F4CC}").small());
                    }
                });
                show_message_content(ui, msg, tx_intent);
            });
        } else {
            // Grouped message: content only, indented.
            ui.add_space(8.0);
            show_message_content(ui, msg, tx_intent);
        }
    });
}

fn show_date_separator(ui: &mut egui::Ui, date: NaiveDate) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add(egui::Separator::default().spacing(6.0));
        ui.label(
            egui::RichText::new(format_day_label(date))
                .small()
                .color(theme::text_muted())
                .strong(),
        );
        ui.add(egui::Separator::default().spacing(6.0));
    });
    ui.add_space(4.0);
}

fn should_group_messages(previous: &ChatMessage, current: &ChatMessage) -> bool {
    let previous_author = canonical_author_key(previous);
    let current_author = canonical_author_key(current);
    if previous_author != current_author {
        debug!(
            previous_author,
            current_author,
            previous_message_id = %previous.message_id,
            current_message_id = %current.message_id,
            grouped = false,
            "chat grouping decision"
        );
        return false;
    }

    let previous_day = message_day(previous.timestamp);
    let current_day = message_day(current.timestamp);
    if previous_day.is_none() || current_day.is_none() || previous_day != current_day {
        debug!(
            previous_author,
            current_author,
            previous_message_id = %previous.message_id,
            current_message_id = %current.message_id,
            grouped = false,
            "chat grouping decision"
        );
        return false;
    }

    let elapsed = current.timestamp - previous.timestamp;
    let grouped = (0..=MESSAGE_GROUP_WINDOW_MS).contains(&elapsed);
    debug!(
        previous_author,
        current_author,
        previous_message_id = %previous.message_id,
        current_message_id = %current.message_id,
        grouped,
        "chat grouping decision"
    );
    grouped
}

fn canonical_author_key(message: &ChatMessage) -> String {
    let author_id = message.author_id.trim();
    if !author_id.is_empty() {
        return format!("id:{author_id}");
    }

    let author_name = message.author_name.trim();
    if !author_name.is_empty() {
        return format!("name:{author_name}:{}", message.message_id);
    }

    format!("message:{}", message.message_id)
}

fn message_day(unix_millis: i64) -> Option<NaiveDate> {
    Local
        .timestamp_millis_opt(unix_millis)
        .single()
        .map(|dt| dt.date_naive())
}

fn format_day_label(date: NaiveDate) -> String {
    let today = Local::now().date_naive();
    if date == today {
        return "Today".to_string();
    }

    if today
        .checked_sub_days(Days::new(1))
        .is_some_and(|yesterday| date == yesterday)
    {
        return "Yesterday".to_string();
    }

    date.format("%b %-d, %Y").to_string()
}

fn show_message_content(ui: &mut egui::Ui, msg: &ChatMessage, tx_intent: &Sender<UiIntent>) {
    render_linkified_text(ui, &msg.text);

    for att in &msg.attachments {
        if att.mime_type.starts_with("image/") {
            show_image_attachment(ui, att);
        } else {
            show_file_attachment(ui, att, "\u{1F39E}");
        }
    }

    // Reactions
    if !msg.reactions.is_empty() {
        ui.horizontal(|ui| {
            for reaction in &msg.reactions {
                let label = format!("{} {}", reaction.emoji, reaction.count);
                let btn = ui.add(
                    egui::Button::new(egui::RichText::new(&label).small())
                        .small()
                        .fill(if reaction.me {
                            theme::COLOR_ACCENT.linear_multiply(0.3)
                        } else {
                            theme::bg_light()
                        }),
                );
                if btn.clicked() {
                    if reaction.me {
                        let _ = tx_intent.send(UiIntent::RemoveReaction {
                            message_id: msg.message_id.clone(),
                            emoji: reaction.emoji.clone(),
                        });
                    } else {
                        let _ = tx_intent.send(UiIntent::AddReaction {
                            message_id: msg.message_id.clone(),
                            emoji: reaction.emoji.clone(),
                        });
                    }
                }
            }
        });
    }
}

fn show_image_attachment(ui: &mut egui::Ui, att: &AttachmentData) {
    let uri = if !att.download_url.is_empty() {
        att.download_url.clone()
    } else {
        format!("file://{}", att.asset_id)
    };
    ui.horizontal(|ui| {
        ui.label("\u{1F5BC}");
        let response = ui.add(
            egui::Label::new(
                egui::RichText::new(format!(
                    "{} ({})",
                    att.filename,
                    format_size(att.size_bytes)
                ))
                .color(theme::COLOR_LINK)
                .underline(),
            )
            .sense(egui::Sense::click()),
        );
        if response.clicked() {
            let _ = open::that(uri.clone());
        }
    });

    let image = egui::Image::from_uri(uri.clone())
        .max_width(MAX_PREVIEW_IMAGE_SIZE)
        .max_height(MAX_PREVIEW_IMAGE_SIZE)
        .maintain_aspect_ratio(true)
        .sense(egui::Sense::click());
    let response = ui.add(image);
    if response.clicked() {
        let _ = open::that(uri);
    }
    if response.hovered() {
        response.on_hover_text("Click to open full image");
    }
}

fn show_file_attachment(ui: &mut egui::Ui, att: &AttachmentData, icon: &str) {
    let uri = if !att.download_url.is_empty() {
        att.download_url.clone()
    } else {
        format!("file://{}", att.asset_id)
    };
    ui.horizontal(|ui| {
        ui.label(icon);
        let response = ui.add(
            egui::Label::new(
                egui::RichText::new(format!(
                    "{} ({}) [Open]",
                    att.filename,
                    format_size(att.size_bytes)
                ))
                .color(theme::COLOR_LINK)
                .underline(),
            )
            .sense(egui::Sense::click()),
        );
        if response.clicked() {
            let _ = open::that(uri);
        }
    });
}

fn render_linkified_text(ui: &mut egui::Ui, text: &str) {
    let segments = linkify_message(text);
    ui.horizontal_wrapped(|ui| {
        for segment in segments {
            match segment {
                MessageSegment::Text(value) => {
                    let response = ui.label(&value);
                    response.context_menu(|ui| {
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(value.clone());
                            ui.close();
                        }
                    });
                }
                MessageSegment::Url(url) => {
                    let response = ui.add(egui::Hyperlink::from_label_and_url(url.clone(), &url));
                    response.context_menu(|ui| {
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(url.clone());
                            ui.close();
                        }
                    });
                }
            }
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MessageSegment {
    Text(String),
    Url(String),
}

fn linkify_message(text: &str) -> Vec<MessageSegment> {
    let mut out = Vec::new();
    let mut cursor = 0;

    while let Some((start, end)) = next_url_span(text, cursor) {
        if start > cursor {
            out.push(MessageSegment::Text(text[cursor..start].to_string()));
        }
        let url = trim_url_suffix(&text[start..end]);
        out.push(MessageSegment::Url(url.to_string()));
        cursor = start + url.len();
    }

    if cursor < text.len() {
        out.push(MessageSegment::Text(text[cursor..].to_string()));
    }

    if out.is_empty() {
        out.push(MessageSegment::Text(text.to_string()));
    }

    out
}

fn next_url_span(text: &str, from: usize) -> Option<(usize, usize)> {
    let http = text[from..].find("http://").map(|i| from + i);
    let https = text[from..].find("https://").map(|i| from + i);
    let start = match (http, https) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => return None,
    };

    if start > 0 {
        let prev = text[..start].chars().next_back().unwrap_or(' ');
        if !prev.is_whitespace() && prev != '(' && prev != '[' {
            return next_url_span(text, start + 1);
        }
    }

    let mut end = text.len();
    for (idx, ch) in text[start..].char_indices() {
        if idx == 0 {
            continue;
        }
        if ch.is_whitespace() {
            end = start + idx;
            break;
        }
    }
    Some((start, end))
}

fn trim_url_suffix(url: &str) -> &str {
    url.trim_end_matches(&['.', ',', ';', ':', '!', '?', ')', ']'][..])
}

fn show_notifications(ui: &mut egui::Ui, model: &UiModel) {
    if model.notifications.is_empty() {
        return;
    }

    // Overlay notifications in the top-right area
    let rect = ui.max_rect();
    let mut y = rect.top() + 8.0;

    for notif in model.notifications.iter().rev().take(3) {
        let color = match notif.kind {
            crate::ui::model::NotificationKind::Poke => theme::COLOR_MENTION,
            crate::ui::model::NotificationKind::Mention => theme::COLOR_MENTION,
            crate::ui::model::NotificationKind::Error => theme::COLOR_DANGER,
            crate::ui::model::NotificationKind::Info => theme::COLOR_ACCENT,
        };

        let notif_rect =
            egui::Rect::from_min_size(egui::pos2(rect.right() - 300.0, y), egui::vec2(280.0, 30.0));
        ui.painter()
            .rect_filled(notif_rect, 6.0, color.linear_multiply(0.9));
        ui.painter().text(
            notif_rect.center(),
            egui::Align2::CENTER_CENTER,
            &notif.text,
            egui::FontId::proportional(13.0),
            egui::Color32::WHITE,
        );
        y += 36.0;
    }
}

fn format_timestamp(unix_millis: i64) -> String {
    Local
        .timestamp_millis_opt(unix_millis)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".to_string())
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        detect_mime_type, format_day_label, format_timestamp, linkify_message,
        should_group_messages, truncate_filename, ChatMessage, MessageSegment,
        MESSAGE_GROUP_WINDOW_MS,
    };
    use chrono::{Days, Local, TimeZone};

    fn msg(author_id: &str, timestamp: i64) -> ChatMessage {
        ChatMessage {
            message_id: format!("{author_id}-{timestamp}"),
            channel_id: "lounge-1".into(),
            author_id: author_id.into(),
            author_name: author_id.into(),
            text: "hello".into(),
            timestamp,
            attachments: vec![],
            reply_to: None,
            reactions: vec![],
            pinned: false,
            edited: false,
        }
    }

    #[test]
    fn detects_avif_mime_from_extension() {
        let mime = detect_mime_type(std::path::Path::new("sample.avif"), "");
        assert_eq!(mime, "image/avif");
    }

    #[test]
    fn formats_timestamp_using_local_time() {
        let unix_millis = 1_710_000_000_000_i64;
        let expected = Local
            .timestamp_millis_opt(unix_millis)
            .single()
            .unwrap()
            .format("%H:%M")
            .to_string();

        assert_eq!(format_timestamp(unix_millis), expected);
    }

    #[test]
    fn invalid_timestamp_uses_placeholder() {
        assert_eq!(format_timestamp(i64::MAX), "--:--");
    }

    #[test]
    fn groups_only_same_author_within_window() {
        let now = Local::now().timestamp_millis();
        assert!(should_group_messages(
            &msg("u1", now),
            &msg("u1", now + 60_000)
        ));
        assert!(!should_group_messages(
            &msg("u1", now),
            &msg("u2", now + 60_000)
        ));
        assert!(!should_group_messages(
            &msg("u1", now),
            &msg("u1", now + MESSAGE_GROUP_WINDOW_MS + 1)
        ));
    }

    #[test]
    fn author_change_within_window_starts_new_group() {
        let now = Local::now().timestamp_millis();
        assert!(!should_group_messages(
            &msg("dresk-id", now),
            &msg("overdose-id", now + 30_000)
        ));
    }

    #[test]
    fn same_author_id_different_names_is_grouped() {
        let now = Local::now().timestamp_millis();
        let mut first = msg("shared-auth-id", now);
        let mut second = msg("shared-auth-id", now + 5_000);
        first.author_name = "Overdose".into();
        second.author_name = "Dresk".into();

        assert!(should_group_messages(&first, &second));
    }

    #[test]
    fn same_text_different_author_is_not_grouped() {
        let now = Local::now().timestamp_millis();
        let mut first = msg("dresk-id", now);
        let mut second = msg("overdose-id", now + 5_000);
        first.text = "indeed".into();
        second.text = "indeed".into();

        assert!(!should_group_messages(&first, &second));
    }

    #[test]
    fn day_labels_today_and_yesterday() {
        let today = Local::now().date_naive();
        let yesterday = today.checked_sub_days(Days::new(1)).unwrap();

        assert_eq!(format_day_label(today), "Today");
        assert_eq!(format_day_label(yesterday), "Yesterday");
    }

    #[test]
    fn linkify_supports_multiple_urls_and_trims_punctuation() {
        let segs = linkify_message("hi https://a.test/path, and http://b.test/x). done");
        assert_eq!(
            segs,
            vec![
                MessageSegment::Text("hi ".into()),
                MessageSegment::Url("https://a.test/path".into()),
                MessageSegment::Text(", and ".into()),
                MessageSegment::Url("http://b.test/x".into()),
                MessageSegment::Text("). done".into()),
            ]
        );
    }

    #[test]
    fn truncate_preserves_extension() {
        assert_eq!(truncate_filename("screenshot.png", 14), "screenshot.png");
        assert_eq!(
            truncate_filename("very_long_screenshot_name.png", 14),
            "very_lon...png"
        );
        assert_eq!(truncate_filename("short.jpg", 20), "short.jpg");
    }
}
