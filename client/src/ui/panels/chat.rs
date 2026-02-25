//! Chat panel: message display, input bar, typing indicators.

use crate::ui::model::{ChatMessage, UiIntent, UiModel};
use crate::ui::theme;
use chrono::{DateTime, Days, Local, NaiveDate, TimeZone};
use crossbeam_channel::Sender;
use eframe::egui;
use tracing::debug;

const MESSAGE_GROUP_WINDOW_MS: i64 = 5 * 60 * 1000;

pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
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
            let text = model.chat_input.trim().to_string();
            if !text.is_empty() {
                let _ = tx_intent.send(UiIntent::SendChat { text });
                model.chat_input.clear();
            }
            response.request_focus();
        }

        if ui.button("Send").clicked() {
            let text = model.chat_input.trim().to_string();
            if !text.is_empty() {
                let _ = tx_intent.send(UiIntent::SendChat { text });
                model.chat_input.clear();
            }
        }
    });

    // Notifications overlay
    show_notifications(ui, model);
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
    // Message text (markdown-like rendering - basic for now)
    ui.label(&msg.text);

    // Attachments
    for att in &msg.attachments {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "📎 {} ({})",
                    att.filename,
                    format_size(att.size_bytes)
                ))
                .color(theme::COLOR_LINK),
            );
        });
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
        format_day_label, format_timestamp, should_group_messages, ChatMessage,
        MESSAGE_GROUP_WINDOW_MS,
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
}
