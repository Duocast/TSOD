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

    let streamers: Vec<_> = model
        .current_members()
        .iter()
        .filter(|member| member.streaming)
        .collect();

    let (headline, subtitle, accent) = match streamers.len() {
        0 => (
            "No active stream",
            "Anyone in this channel can start streaming from the Share button.",
            theme::text_muted(),
        ),
        1 => (
            "Live stream",
            "Watching a single presenter stream for everyone in this channel.",
            theme::COLOR_ONLINE,
        ),
        _ => (
            "Multiple streamers detected",
            "Only one stream should be active per streaming channel.",
            theme::COLOR_MENTION,
        ),
    };

    ui.add_space(8.0);
    egui::Frame::group(ui.style())
        .fill(theme::bg_dark())
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_min_height((ui.available_height() - 8.0).max(260.0));
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("🖥").size(48.0));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(headline)
                            .size(20.0)
                            .strong()
                            .color(accent),
                    );

                    if let Some(streamer) = streamers.first() {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "Streaming now: {}",
                                streamer.display_name
                            ))
                            .strong()
                            .color(theme::text_color()),
                        );
                    }

                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(subtitle).color(theme::text_muted()));
                });
            });
        });
}
