//! Chat panel: message display, input bar, typing indicators.

use crate::ui::model::{AttachmentData, ChatMessage, PendingAttachment, UiIntent, UiModel};
use crate::ui::theme;
use chrono::{Days, Local, NaiveDate, TimeZone};
use crossbeam_channel::Sender;
use eframe::egui;
use std::path::Path;
use tracing::debug;

const MESSAGE_GROUP_WINDOW_MS: i64 = 5 * 60 * 1000;
const MAX_PREVIEW_IMAGE_SIZE: f32 = 240.0;
const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
const ALLOWED_ATTACHMENT_MIME: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "video/mp4",
    "video/webm",
];

pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    collect_dropped_files(ui.ctx(), model);

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

    // Messages area (takes remaining space minus input)
    let available = ui.available_height() - 64.0; // reserve space for typing + compact input
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

    let lower_input_spacer = (ui.available_height() - 34.0).max(2.0);
    ui.add_space(lower_input_spacer);
    ui.separator();

    show_pending_attachments(ui, model);

    // Input bar
    ui.horizontal(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(&mut model.chat_input)
                .hint_text("Type a message...")
                .desired_width(ui.available_width() - 70.0)
                .frame(true),
        );

        model.chat_input_focused = response.has_focus();

        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            send_chat_from_input(model, tx_intent);
            response.request_focus();
        }

        if ui.button("Send").clicked() {
            send_chat_from_input(model, tx_intent);
        }
    });

    // Notifications overlay
    show_notifications(ui, model);
}

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
}

fn show_pending_attachments(ui: &mut egui::Ui, model: &mut UiModel) {
    if model.pending_attachments.is_empty() {
        return;
    }

    ui.group(|ui| {
        ui.label(egui::RichText::new("Pending attachments").small().strong());
        let mut remove_idx = None;
        for (idx, file) in model.pending_attachments.iter().enumerate() {
            ui.horizontal(|ui| {
                let status = file
                    .error
                    .clone()
                    .unwrap_or_else(|| format_size(file.size_bytes));
                let text = if file.error.is_some() {
                    egui::RichText::new(format!("{} ({status})", file.filename))
                        .color(theme::COLOR_DANGER)
                } else {
                    egui::RichText::new(format!("{} ({status})", file.filename))
                };
                ui.label(text);
                if ui.small_button("Remove").clicked() {
                    remove_idx = Some(idx);
                }
            });
        }
        if let Some(idx) = remove_idx {
            model.pending_attachments.remove(idx);
        }
    });
    ui.add_space(4.0);
}

fn collect_dropped_files(ctx: &egui::Context, model: &mut UiModel) {
    let dropped_files = ctx.input(|i| i.raw.dropped_files.clone());
    for dropped in dropped_files {
        let Some(path) = dropped.path else {
            continue;
        };
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();
        let mime_type = detect_mime_type(&path, &dropped.mime);
        let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

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
    }
}

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
                        ui.label(egui::RichText::new("📌").small());
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
            show_file_attachment(ui, att, "🎞");
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
        ui.label("🖼");
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
                    ui.label(value);
                }
                MessageSegment::Url(url) => {
                    ui.add(egui::Hyperlink::from_label_and_url(url.clone(), url));
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
        format_day_label, format_timestamp, linkify_message, should_group_messages, ChatMessage,
        MessageSegment, MESSAGE_GROUP_WINDOW_MS,
    };
    use chrono::{DateTime, Days, Local};

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
    fn formats_timestamp_using_local_time() {
        let unix_millis = 1_710_000_000_000_i64;
        let expected = DateTime::<Local>::from_timestamp_millis(unix_millis)
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
}
