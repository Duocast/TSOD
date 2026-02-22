//! Visual theme constants and application.

use eframe::egui;

// Brand colors (dark theme, inspired by modern voice chat apps)
pub const COLOR_BG_DARK: egui::Color32 = egui::Color32::from_rgb(30, 31, 34);
pub const COLOR_BG_MEDIUM: egui::Color32 = egui::Color32::from_rgb(43, 45, 49);
pub const COLOR_BG_LIGHT: egui::Color32 = egui::Color32::from_rgb(54, 57, 63);
pub const COLOR_BG_INPUT: egui::Color32 = egui::Color32::from_rgb(64, 68, 75);
pub const COLOR_TEXT: egui::Color32 = egui::Color32::from_rgb(219, 222, 225);
pub const COLOR_TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(148, 155, 164);
pub const COLOR_TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(96, 100, 108);
pub const COLOR_ACCENT: egui::Color32 = egui::Color32::from_rgb(88, 101, 242);
pub const COLOR_ONLINE: egui::Color32 = egui::Color32::from_rgb(35, 165, 90);
pub const COLOR_IDLE: egui::Color32 = egui::Color32::from_rgb(240, 178, 50);
pub const COLOR_DND: egui::Color32 = egui::Color32::from_rgb(237, 66, 69);
pub const COLOR_OFFLINE: egui::Color32 = egui::Color32::from_rgb(128, 132, 142);
pub const COLOR_VOICE_ACTIVE: egui::Color32 = egui::Color32::from_rgb(35, 165, 90);
pub const COLOR_MENTION: egui::Color32 = egui::Color32::from_rgb(250, 168, 26);
pub const COLOR_LINK: egui::Color32 = egui::Color32::from_rgb(0, 168, 252);
pub const COLOR_DANGER: egui::Color32 = egui::Color32::from_rgb(237, 66, 69);

pub fn apply_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    // Visuals
    let v = &mut style.visuals;
    v.dark_mode = true;

    v.override_text_color = Some(COLOR_TEXT);

    v.widgets.noninteractive.bg_fill = COLOR_BG_MEDIUM;
    v.widgets.inactive.bg_fill = COLOR_BG_LIGHT;
    v.widgets.hovered.bg_fill = egui::Color32::from_rgb(70, 73, 80);
    v.widgets.active.bg_fill = COLOR_ACCENT;
    v.widgets.open.bg_fill = COLOR_BG_LIGHT;

    v.window_fill = COLOR_BG_MEDIUM;
    v.panel_fill = COLOR_BG_DARK;
    v.extreme_bg_color = COLOR_BG_INPUT;
    v.faint_bg_color = COLOR_BG_MEDIUM;

    v.window_rounding = egui::Rounding::same(8.0);
    v.window_shadow = egui::epaint::Shadow {
        offset: egui::vec2(0.0, 4.0),
        blur: 12.0,
        spread: 0.0,
        color: egui::Color32::from_black_alpha(80),
    };

    v.selection.bg_fill = COLOR_ACCENT.linear_multiply(0.3);
    v.selection.stroke = egui::Stroke::new(1.0, COLOR_ACCENT);

    // Spacing
    style.spacing.item_spacing = egui::vec2(8.0, 4.0);
    style.spacing.window_margin = egui::Margin::same(12.0);

    ctx.set_style(style);
}
