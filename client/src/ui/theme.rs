//! Visual theme constants and application.

use eframe::egui;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeMode {
    Dark = 0,
    Light = 1,
    Oled = 2,
}

static ACTIVE_THEME_MODE: AtomicU8 = AtomicU8::new(ThemeMode::Dark as u8);

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

pub fn bg_dark() -> egui::Color32 {
    if is_light_mode() {
        egui::Color32::from_rgb(228, 233, 240)
    } else {
        COLOR_BG_DARK
    }
}

pub fn bg_medium() -> egui::Color32 {
    if is_light_mode() {
        egui::Color32::from_rgb(236, 240, 246)
    } else {
        COLOR_BG_MEDIUM
    }
}

pub fn bg_light() -> egui::Color32 {
    if is_light_mode() {
        egui::Color32::from_rgb(224, 229, 236)
    } else {
        COLOR_BG_LIGHT
    }
}

pub fn bg_input() -> egui::Color32 {
    if is_light_mode() {
        egui::Color32::from_rgb(214, 220, 229)
    } else {
        COLOR_BG_INPUT
    }
}

pub fn text_color() -> egui::Color32 {
    if is_light_mode() {
        egui::Color32::from_rgb(36, 41, 47)
    } else {
        COLOR_TEXT
    }
}

pub fn text_dim() -> egui::Color32 {
    if is_light_mode() {
        egui::Color32::from_rgb(94, 103, 115)
    } else {
        COLOR_TEXT_DIM
    }
}

pub fn text_muted() -> egui::Color32 {
    if is_light_mode() {
        egui::Color32::from_rgb(120, 130, 142)
    } else {
        COLOR_TEXT_MUTED
    }
}

pub fn is_light_mode() -> bool {
    ACTIVE_THEME_MODE.load(Ordering::Relaxed) == ThemeMode::Light as u8
}

pub fn muted_button_fill() -> egui::Color32 {
    if is_light_mode() {
        egui::Color32::from_rgb(224, 229, 236)
    } else {
        COLOR_BG_LIGHT
    }
}

pub fn apply_theme(ctx: &egui::Context, theme_name: &str) {
    let light_mode = theme_name.eq_ignore_ascii_case("light");
    let oled_mode = theme_name.eq_ignore_ascii_case("oled black");

    ACTIVE_THEME_MODE.store(
        if light_mode {
            ThemeMode::Light as u8
        } else if oled_mode {
            ThemeMode::Oled as u8
        } else {
            ThemeMode::Dark as u8
        },
        Ordering::Relaxed,
    );

    let mut style = egui::Style::default();
    style.visuals = if light_mode {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };

    // Visuals
    let v = &mut style.visuals;
    v.dark_mode = !light_mode;

    if light_mode {
        v.override_text_color = Some(egui::Color32::from_rgb(36, 41, 47));
        v.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(244, 246, 249);
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(230, 234, 240);
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(215, 222, 232);
        v.widgets.active.bg_fill = COLOR_ACCENT;
        v.widgets.open.bg_fill = egui::Color32::from_rgb(230, 234, 240);
        v.window_fill = egui::Color32::from_rgb(248, 250, 252);
        v.panel_fill = egui::Color32::from_rgb(239, 242, 247);
        v.extreme_bg_color = egui::Color32::from_rgb(255, 255, 255);
        v.faint_bg_color = egui::Color32::from_rgb(239, 242, 247);
    } else {
        v.override_text_color = Some(COLOR_TEXT);
        v.widgets.noninteractive.bg_fill = if oled_mode {
            egui::Color32::from_rgb(12, 12, 14)
        } else {
            COLOR_BG_MEDIUM
        };
        v.widgets.inactive.bg_fill = if oled_mode {
            egui::Color32::from_rgb(22, 22, 24)
        } else {
            COLOR_BG_LIGHT
        };
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(70, 73, 80);
        v.widgets.active.bg_fill = COLOR_ACCENT;
        v.widgets.open.bg_fill = if oled_mode {
            egui::Color32::from_rgb(22, 22, 24)
        } else {
            COLOR_BG_LIGHT
        };
        v.window_fill = if oled_mode {
            egui::Color32::from_rgb(8, 8, 9)
        } else {
            COLOR_BG_MEDIUM
        };
        v.panel_fill = if oled_mode {
            egui::Color32::from_rgb(0, 0, 0)
        } else {
            COLOR_BG_DARK
        };
        v.extreme_bg_color = if oled_mode {
            egui::Color32::from_rgb(14, 14, 16)
        } else {
            COLOR_BG_INPUT
        };
        v.faint_bg_color = if oled_mode {
            egui::Color32::from_rgb(8, 8, 9)
        } else {
            COLOR_BG_MEDIUM
        };
    }

    v.window_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: egui::Color32::from_black_alpha(80),
    };

    v.selection.bg_fill = COLOR_ACCENT.linear_multiply(0.3);
    v.selection.stroke = egui::Stroke::new(1.0, COLOR_ACCENT);

    // Spacing
    style.spacing.item_spacing = egui::vec2(8.0, 4.0);
    style.spacing.window_margin = egui::Margin::same(12);

    ctx.set_style(style);
}
