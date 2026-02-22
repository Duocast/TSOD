//! Chat panel: message display, input bar, typing indicators.

use crate::ui::model::{ChatMessage, UiIntent, UiModel};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, model: &mut UiModel, tx_intent: &Sender<UiIntent>) {
    // Channel header
    ui.horizontal(|ui| {
        let ch_name = if model.selected_channel_name.is_empty() {
            "Select a channel"
        } else {
            &model.selected_channel_name
        };
        ui.heading(
            egui::RichText::new(format!("# {ch_name}")).color(theme::COLOR_TEXT),
        );
    });
    ui.separator();

    // Messages area (takes remaining space minus input)
    let available = ui.available_height() - 80.0; // reserve space for input + typing
    egui::ScrollArea::vertical()
        .max_height(available.max(100.0))
        .stick_to_bottom(true)
        .show(ui, |ui| {
            if let Some(messages) = model.current_messages() {
                let mut prev_author: Option<&str> = None;
                for msg in messages.iter() {
                    let compact = prev_author == Some(&msg.author_id);
                    show_message(ui, msg, compact, model, tx_intent);
                    prev_author = Some(&msg.author_id);
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("No messages yet. Start the conversation!")
                            .color(theme::COLOR_TEXT_MUTED)
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
            _ => format!("{} and {} others are typing...", typing[0], typing.len() - 1),
        };
        ui.label(
            egui::RichText::new(text)
                .small()
                .color(theme::COLOR_TEXT_MUTED)
                .italics(),
        );
    }

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

fn show_message(
    ui: &mut egui::Ui,
    msg: &ChatMessage,
    compact: bool,
    _model: &UiModel,
    tx_intent: &Sender<UiIntent>,
) {
    ui.horizontal(|ui| {
        if !compact {
            // Full message with author + timestamp
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&msg.author_name)
                            .strong()
                            .color(theme::COLOR_TEXT),
                    );
                    let ts = format_timestamp(msg.timestamp);
                    ui.label(
                        egui::RichText::new(ts)
                            .small()
                            .color(theme::COLOR_TEXT_MUTED),
                    );
                    if msg.edited {
                        ui.label(
                            egui::RichText::new("(edited)")
                                .small()
                                .color(theme::COLOR_TEXT_MUTED),
                        );
                    }
                    if msg.pinned {
                        ui.label(
                            egui::RichText::new("📌")
                                .small(),
                        );
                    }
                });
                show_message_content(ui, msg, tx_intent);
            });
        } else {
            // Compact: just the content, indented
            ui.add_space(8.0);
            show_message_content(ui, msg, tx_intent);
        }
    });
}

fn show_message_content(
    ui: &mut egui::Ui,
    msg: &ChatMessage,
    tx_intent: &Sender<UiIntent>,
) {
    // Message text (markdown-like rendering - basic for now)
    ui.label(&msg.text);

    // Attachments
    for att in &msg.attachments {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("📎 {} ({})", att.filename, format_size(att.size_bytes)))
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
                    egui::Button::new(
                        egui::RichText::new(&label).small(),
                    )
                    .small()
                    .fill(if reaction.me {
                        theme::COLOR_ACCENT.linear_multiply(0.3)
                    } else {
                        theme::COLOR_BG_LIGHT
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

        let notif_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - 300.0, y),
            egui::vec2(280.0, 30.0),
        );
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
    // Simple HH:MM format
    let secs = unix_millis / 1000;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    format!("{hours:02}:{minutes:02}")
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
