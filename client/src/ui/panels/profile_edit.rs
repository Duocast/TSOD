//! Self-profile editing modal with tabs: Profile, Links, Avatar, Banner.
//!
//! Opened by clicking the user's own avatar in the user panel, or via
//! Settings > Identity > Edit Profile button.

use crate::ui::model::{ProfileEditTab, ProfileLinkData, UiIntent, UiModel};
use crate::ui::theme;
use crossbeam_channel::Sender;
use eframe::egui;

const MODAL_WIDTH: f32 = 520.0;
const MODAL_HEIGHT: f32 = 560.0;
const DISPLAY_NAME_MAX: usize = 32;
const DESCRIPTION_MAX: usize = 190;
const LINKS_MAX: usize = 5;
const SUPPORTED_PLATFORMS: &[&str] = &[
    "Steam",
    "GitHub",
    "Twitter/X",
    "Twitch",
    "YouTube",
    "Website",
];

/// Show the profile edit window. Call each frame when `model.show_edit_profile` is true.
pub fn show(ctx: &egui::Context, model: &mut UiModel, tx: &Sender<UiIntent>) {
    if !model.show_edit_profile {
        return;
    }

    let mut open = true;
    let mut cancel_clicked = false;
    egui::Window::new("Edit Profile")
        .id(egui::Id::new("profile_edit_modal"))
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .constrain(true)
        .default_size([MODAL_WIDTH, MODAL_HEIGHT])
        .min_size([MODAL_WIDTH, MODAL_HEIGHT])
        .show(ctx, |ui| {
            ui.set_min_size(egui::vec2(MODAL_WIDTH - 24.0, MODAL_HEIGHT - 24.0));

            // ── Tab bar ───────────────────────────────────────────────────
            ui.horizontal(|ui| {
                tab_btn(ui, model, ProfileEditTab::Profile, "Profile");
                tab_btn(ui, model, ProfileEditTab::Links, "Links");
                tab_btn(ui, model, ProfileEditTab::Avatar, "Avatar");
                tab_btn(ui, model, ProfileEditTab::Banner, "Banner");
            });

            ui.separator();
            ui.add_space(6.0);

            // ── Tab content ───────────────────────────────────────────────
            egui::ScrollArea::vertical()
                .max_height(MODAL_HEIGHT - 140.0)
                .show(ui, |ui| {
                    ui.set_min_width(MODAL_WIDTH - 24.0);
                    match model.edit_profile_tab {
                        ProfileEditTab::Profile => tab_profile(ui, model),
                        ProfileEditTab::Links => tab_links(ui, model, tx),
                        ProfileEditTab::Avatar => tab_avatar(ui, model, tx),
                        ProfileEditTab::Banner => tab_banner(ui, model, tx),
                    }
                });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);

            // ── Save / Cancel buttons ─────────────────────────────────────
            ui.horizontal(|ui| {
                let save_label = if model.profile_save_in_flight {
                    "Saving…"
                } else {
                    "Save Changes"
                };
                let save_btn = ui.add_enabled(
                    !model.profile_save_in_flight,
                    egui::Button::new(
                        egui::RichText::new(save_label)
                            .color(egui::Color32::WHITE)
                            .strong(),
                    )
                    .fill(theme::COLOR_ACCENT),
                );
                if save_btn.clicked() {
                    commit_save(model, tx);
                }

                if ui.button("Cancel").clicked() {
                    cancel_clicked = true;
                }
            });
        });

    if !open || cancel_clicked {
        cancel_edit(model);
        model.show_edit_profile = false;
    }
}

// ── Tab helpers ───────────────────────────────────────────────────────────────

fn tab_btn(ui: &mut egui::Ui, model: &mut UiModel, tab: ProfileEditTab, label: &str) {
    let selected = model.edit_profile_tab == tab;
    let text = egui::RichText::new(label).strong();
    let text = if selected {
        text.color(theme::text_color())
    } else {
        text.color(theme::text_muted())
    };
    let fill = if selected {
        theme::bg_light()
    } else {
        theme::bg_medium()
    };
    if ui
        .add(egui::Button::new(text).fill(fill).corner_radius(6.0))
        .clicked()
    {
        model.edit_profile_tab = tab;
    }
}

// ── Profile tab ───────────────────────────────────────────────────────────────

fn tab_profile(ui: &mut egui::Ui, model: &mut UiModel) {
    // Display Name
    ui.label(egui::RichText::new("Display Name").strong());
    ui.add_space(2.0);

    let dn = &mut model.edit_profile_draft.display_name;
    ui.add(
        egui::TextEdit::singleline(dn)
            .desired_width(f32::INFINITY)
            .hint_text("Display name")
            .char_limit(DISPLAY_NAME_MAX),
    );
    // Strip newlines and control chars from single-line field.
    *dn = sanitize_single_line(dn);

    if dn.len() >= DISPLAY_NAME_MAX {
        ui.label(
            egui::RichText::new(format!("{}/{} characters", dn.len(), DISPLAY_NAME_MAX))
                .color(theme::COLOR_DANGER)
                .size(11.0),
        );
    } else {
        ui.label(
            egui::RichText::new(format!("{}/{} characters", dn.len(), DISPLAY_NAME_MAX))
                .color(theme::text_muted())
                .size(11.0),
        );
    }

    ui.add_space(10.0);

    // About Me
    ui.label(egui::RichText::new("About Me").strong());
    ui.add_space(2.0);

    let desc = &mut model.edit_profile_draft.description;
    let remaining = DESCRIPTION_MAX.saturating_sub(desc.len());
    ui.add(
        egui::TextEdit::multiline(desc)
            .desired_width(f32::INFINITY)
            .desired_rows(4)
            .hint_text("Tell others a bit about yourself…")
            .char_limit(DESCRIPTION_MAX),
    );
    // Strip control chars except standard line breaks.
    *desc = sanitize_multiline(desc);

    let counter_color = if remaining == 0 {
        theme::COLOR_DANGER
    } else if remaining < 20 {
        theme::COLOR_IDLE
    } else {
        theme::text_muted()
    };
    ui.label(
        egui::RichText::new(format!("{remaining} characters remaining"))
            .color(counter_color)
            .size(11.0),
    );

    ui.add_space(10.0);

    // Username Color
    ui.label(egui::RichText::new("Username Color").strong());
    ui.add_space(4.0);

    // 18-color preset grid (3 rows × 6 columns).
    const PRESET_COLORS: [(u32, &str); 18] = [
        (0x5865F2, "Blurple"),
        (0xEB459E, "Fuchsia"),
        (0xED4245, "Red"),
        (0xFEE75C, "Yellow"),
        (0x57F287, "Green"),
        (0x3BA55C, "Dark Green"),
        (0x5BC0EB, "Sky Blue"),
        (0x9B59B6, "Purple"),
        (0xE67E22, "Orange"),
        (0x1ABC9C, "Teal"),
        (0xE91E63, "Pink"),
        (0x607D8B, "Slate"),
        (0x2C2F33, "Dark"),
        (0x99AAB5, "Light"),
        (0xFFFFFF, "White"),
        (0x000000, "Black"),
        (0x34495E, "Charcoal"),
        (0x71368A, "Violet"),
    ];
    let swatch_size = 28.0;
    let swatch_spacing = 4.0;
    let cols = 6;

    for row in 0..3 {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = swatch_spacing;
            for col in 0..cols {
                let idx = row * cols + col;
                let (hex, label) = PRESET_COLORS[idx];
                let color = egui::Color32::from_rgb(
                    ((hex >> 16) & 0xFF) as u8,
                    ((hex >> 8) & 0xFF) as u8,
                    (hex & 0xFF) as u8,
                );
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(swatch_size, swatch_size),
                    egui::Sense::click(),
                );
                ui.painter().rect_filled(rect, 4.0, color);
                // Border for very dark/light swatches so they're visible.
                if hex == 0x000000 || hex == 0x2C2F33 {
                    ui.painter().rect_stroke(
                        rect,
                        4.0,
                        egui::Stroke::new(1.0, theme::text_muted()),
                        egui::StrokeKind::Outside,
                    );
                }
                // Selection ring.
                if model.edit_profile_draft.accent_color == hex {
                    ui.painter().rect_stroke(
                        rect.expand(2.0),
                        6.0,
                        egui::Stroke::new(2.0, egui::Color32::WHITE),
                        egui::StrokeKind::Outside,
                    );
                }
                if resp.clicked() {
                    model.edit_profile_draft.accent_color = hex;
                    model.edit_profile_draft.accent_hex_input = format!("#{:06X}", hex);
                }
                resp.on_hover_text(label);
            }
        });
    }

    ui.add_space(6.0);

    // Hex input + live preview swatch.
    ui.horizontal(|ui| {
        let hex = &mut model.edit_profile_draft.accent_hex_input;
        if hex.is_empty() {
            *hex = format!("#{:06X}", model.edit_profile_draft.accent_color);
        }
        let resp = ui.add(
            egui::TextEdit::singleline(hex)
                .desired_width(80.0)
                .char_limit(7)
                .hint_text("#RRGGBB"),
        );
        if resp.changed() {
            if let Some(parsed) = parse_hex_color(hex) {
                model.edit_profile_draft.accent_color = parsed;
            }
        }

        ui.add_space(4.0);

        // Preview circle of the current accent color.
        let current = model.edit_profile_draft.accent_color;
        let preview_color = egui::Color32::from_rgb(
            ((current >> 16) & 0xFF) as u8,
            ((current >> 8) & 0xFF) as u8,
            (current & 0xFF) as u8,
        );
        let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 9.0, preview_color);
        ui.painter().circle_stroke(
            rect.center(),
            9.0,
            egui::Stroke::new(1.0, theme::text_muted()),
        );

        ui.label(
            egui::RichText::new("Name preview")
                .color(theme::text_muted())
                .size(11.0),
        );
    });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Chat preview:")
                .color(theme::text_muted())
                .size(11.0),
        );
        let preview_name = model.edit_profile_draft.display_name.trim();
        let preview_name = if preview_name.is_empty() {
            "Username"
        } else {
            preview_name
        };
        ui.label(
            egui::RichText::new(preview_name)
                .strong()
                .color(accent_color32(model.edit_profile_draft.accent_color)),
        );
    });
}

// ── Links tab ─────────────────────────────────────────────────────────────────

fn tab_links(ui: &mut egui::Ui, model: &mut UiModel, tx: &Sender<UiIntent>) {
    let links_count = model.edit_profile_draft.links.len();

    if links_count == 0 {
        ui.label(
            egui::RichText::new("No links added yet.")
                .color(theme::text_muted())
                .italics(),
        );
    } else {
        // Show existing links with remove buttons.
        let mut to_remove: Option<usize> = None;
        for (i, link) in model.edit_profile_draft.links.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(&link.platform)
                        .strong()
                        .color(theme::COLOR_ACCENT),
                );
                ui.label(
                    egui::RichText::new(&link.url)
                        .color(theme::text_muted())
                        .size(12.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button(egui::RichText::new("✕").color(theme::COLOR_DANGER))
                        .on_hover_text("Remove")
                        .clicked()
                    {
                        to_remove = Some(i);
                    }
                });
            });
        }
        if let Some(idx) = to_remove {
            model.edit_profile_draft.links.remove(idx);
        }
    }

    ui.add_space(10.0);

    // Add new link form (only if under the limit).
    if links_count < LINKS_MAX {
        ui.label(egui::RichText::new("Add Link").strong());
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("Platform:");
            egui::ComboBox::from_id_salt("add_link_platform")
                .selected_text(if model.edit_profile_draft.add_link_platform.is_empty() {
                    "Select…"
                } else {
                    &model.edit_profile_draft.add_link_platform
                })
                .width(120.0)
                .show_ui(ui, |ui| {
                    for &p in SUPPORTED_PLATFORMS {
                        ui.selectable_value(
                            &mut model.edit_profile_draft.add_link_platform,
                            p.to_string(),
                            p,
                        );
                    }
                });
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("URL:   ");
            ui.add(
                egui::TextEdit::singleline(&mut model.edit_profile_draft.add_link_url)
                    .desired_width(300.0)
                    .hint_text("https://…"),
            );
        });

        ui.add_space(6.0);
        let can_add = !model.edit_profile_draft.add_link_platform.is_empty()
            && !model.edit_profile_draft.add_link_url.is_empty();
        if ui
            .add_enabled(can_add, egui::Button::new("Add Link"))
            .clicked()
        {
            let platform = model.edit_profile_draft.add_link_platform.clone();
            let url = model.edit_profile_draft.add_link_url.trim().to_string();

            match validate_link_url(&platform, &url) {
                Ok(normalized) => {
                    model.edit_profile_draft.links.push(ProfileLinkData {
                        platform,
                        url: normalized,
                        display_text: String::new(),
                        verified: false,
                    });
                    model.edit_profile_draft.add_link_platform.clear();
                    model.edit_profile_draft.add_link_url.clear();
                }
                Err(reason) => {
                    let _ = tx.send(UiIntent::Help); // just to have a no-op; we show inline error
                    ui.colored_label(theme::COLOR_DANGER, reason);
                }
            }
        }
    } else {
        ui.label(
            egui::RichText::new(format!("Maximum {LINKS_MAX} links reached."))
                .color(theme::text_muted()),
        );
    }
}

// ── Avatar tab ────────────────────────────────────────────────────────────────

fn tab_avatar(ui: &mut egui::Ui, model: &mut UiModel, tx: &Sender<UiIntent>) {
    // Preview (128×128)
    let preview_bytes = model.edit_profile_draft.avatar_preview_bytes.clone();
    let preview_url = model
        .edit_profile_draft
        .avatar_preview_url
        .clone()
        .or_else(|| {
            model
                .self_profile
                .as_ref()
                .and_then(|p| p.avatar_url.clone())
        })
        .or_else(|| model.avatar_url.clone());

    let preview_size = egui::vec2(128.0, 128.0);
    if let Some(bytes) = &preview_bytes {
        ui.add(
            egui::Image::from_bytes("bytes://avatar_preview", bytes.clone())
                .fit_to_exact_size(preview_size)
                .corner_radius(64.0),
        );
    } else if let Some(url) = &preview_url {
        ui.add(
            crate::ui::image_from_source(url)
                .fit_to_exact_size(preview_size)
                .corner_radius(64.0),
        );
    } else {
        // Initial-letter fallback
        let (rect, _) = ui.allocate_exact_size(preview_size, egui::Sense::hover());
        ui.painter().circle_filled(
            rect.center(),
            64.0,
            accent_color32(model.edit_profile_draft.accent_color),
        );
        let initial = model
            .edit_profile_draft
            .display_name
            .chars()
            .next()
            .or_else(|| model.nick.chars().next())
            .unwrap_or('?')
            .to_uppercase()
            .next()
            .unwrap_or('?');
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            initial.to_string(),
            egui::FontId::proportional(48.0),
            theme::text_color(),
        );
    }

    ui.add_space(8.0);
    ui.label(
        egui::RichText::new("Target: 256×256 px — Accepted: PNG, JPEG, WebP — max 3 MB")
            .color(theme::text_muted())
            .size(11.0),
    );
    ui.add_space(6.0);

    ui.horizontal(|ui| {
        let upload_label = if model.avatar_upload_in_flight {
            "Uploading…"
        } else {
            "Upload Image"
        };
        if ui
            .add_enabled(
                !model.avatar_upload_in_flight,
                egui::Button::new(upload_label),
            )
            .clicked()
        {
            if let Some(path) = pick_image_file("avatar") {
                model.avatar_upload_in_flight = true;
                let _ = tx.send(UiIntent::UploadProfileAvatar { path });
            }
        }

        if model.edit_profile_draft.pending_avatar_asset_id.is_some()
            || model
                .self_profile
                .as_ref()
                .is_some_and(|p| p.avatar_url.is_some())
        {
            if ui
                .button(egui::RichText::new("Remove").color(theme::COLOR_DANGER))
                .clicked()
            {
                model.edit_profile_draft.pending_avatar_asset_id = Some(String::new()); // empty = clear
                model.edit_profile_draft.avatar_preview_url = None;
                model.edit_profile_draft.avatar_preview_bytes = None;
                let _ = tx.send(UiIntent::RemoveAvatar);
            }
        }
    });

    if model.avatar_upload_in_flight {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(egui::RichText::new("Uploading avatar…").color(theme::text_muted()));
        });
    }

    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(
            "Avatar changes take effect immediately after upload is verified by the server.",
        )
        .color(theme::text_muted())
        .size(11.0)
        .italics(),
    );
}

// ── Banner tab ────────────────────────────────────────────────────────────────

fn tab_banner(ui: &mut egui::Ui, model: &mut UiModel, tx: &Sender<UiIntent>) {
    // Preview scaled to available width
    let preview_bytes = model.edit_profile_draft.banner_preview_bytes.clone();
    let preview_url = model
        .edit_profile_draft
        .banner_preview_url
        .clone()
        .or_else(|| {
            model
                .self_profile
                .as_ref()
                .and_then(|p| p.banner_url.clone())
        });

    let avail_w = ui.available_width().min(480.0);
    let banner_h = avail_w * (240.0 / 680.0);

    if let Some(bytes) = &preview_bytes {
        ui.add(
            egui::Image::from_bytes("bytes://banner_preview", bytes.clone())
                .fit_to_exact_size(egui::vec2(avail_w, banner_h))
                .corner_radius(8.0),
        );
    } else if let Some(url) = &preview_url {
        ui.add(
            crate::ui::image_from_source(url)
                .fit_to_exact_size(egui::vec2(avail_w, banner_h))
                .corner_radius(8.0),
        );
    } else {
        // Gradient fallback using accent color
        let (rect, _) = ui.allocate_exact_size(egui::vec2(avail_w, banner_h), egui::Sense::hover());
        let accent = accent_color32(model.edit_profile_draft.accent_color);
        let dark = theme::bg_dark();
        ui.painter().rect_filled(rect, 8.0, dark);
        // Draw a simple tinted overlay
        ui.painter().rect_filled(
            rect,
            8.0,
            egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 60),
        );
    }

    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Recommended: 680×240 px — Accepted: PNG, JPEG, WebP — max 10 MB")
            .color(theme::text_muted())
            .size(11.0),
    );
    ui.add_space(6.0);

    ui.horizontal(|ui| {
        let upload_label = if model.banner_upload_in_flight {
            "Uploading…"
        } else {
            "Upload Image"
        };
        if ui
            .add_enabled(
                !model.banner_upload_in_flight,
                egui::Button::new(upload_label),
            )
            .clicked()
        {
            if let Some(path) = pick_image_file("banner") {
                model.banner_upload_in_flight = true;
                let _ = tx.send(UiIntent::UploadProfileBanner { path });
            }
        }

        if model.edit_profile_draft.pending_banner_asset_id.is_some()
            || model
                .self_profile
                .as_ref()
                .is_some_and(|p| p.banner_url.is_some())
        {
            if ui
                .button(egui::RichText::new("Remove").color(theme::COLOR_DANGER))
                .clicked()
            {
                model.edit_profile_draft.pending_banner_asset_id = Some(String::new());
                model.edit_profile_draft.banner_preview_url = None;
                model.edit_profile_draft.banner_preview_bytes = None;
                let _ = tx.send(UiIntent::RemoveBanner);
            }
        }
    });

    if model.banner_upload_in_flight {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(egui::RichText::new("Uploading banner…").color(theme::text_muted()));
        });
    }
}

// ── Custom status popover ─────────────────────────────────────────────────────

/// Small popover anchored to the status area in the user panel.
pub fn show_custom_status_popover(
    ctx: &egui::Context,
    model: &mut UiModel,
    tx: &Sender<UiIntent>,
    anchor: egui::Pos2,
) {
    if !model.show_custom_status_popover {
        return;
    }

    let popover_id = egui::Id::new("custom_status_popover");
    egui::Area::new(popover_id)
        .fixed_pos(anchor)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(&ctx.style())
                .fill(theme::bg_medium())
                .corner_radius(8.0)
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.set_min_width(260.0);
                    ui.label(egui::RichText::new("Custom Status").strong());
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.label("Emoji:");
                        ui.add(
                            egui::TextEdit::singleline(&mut model.custom_status_emoji_draft)
                                .desired_width(40.0)
                                .hint_text("😊"),
                        );
                    });
                    ui.add_space(4.0);

                    ui.label("Status:");
                    ui.add(
                        egui::TextEdit::singleline(&mut model.custom_status_text_draft)
                            .desired_width(f32::INFINITY)
                            .hint_text("What's on your mind?"),
                    );
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Set Status")
                                        .color(egui::Color32::WHITE)
                                        .strong(),
                                )
                                .fill(theme::COLOR_ACCENT),
                            )
                            .clicked()
                        {
                            let text = model.custom_status_text_draft.trim().to_string();
                            let emoji = model.custom_status_emoji_draft.trim().to_string();
                            let _ = tx.send(UiIntent::SetCustomStatus {
                                status_text: Some(text),
                                status_emoji: Some(emoji),
                            });
                            model.show_custom_status_popover = false;
                        }

                        if ui.button("Clear").clicked() {
                            let _ = tx.send(UiIntent::SetCustomStatus {
                                status_text: Some(String::new()),
                                status_emoji: Some(String::new()),
                            });
                            model.show_custom_status_popover = false;
                        }

                        if ui.button("Cancel").clicked() {
                            model.show_custom_status_popover = false;
                        }
                    });
                });
        });

    // Close on Escape.
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        model.show_custom_status_popover = false;
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn commit_save(model: &mut UiModel, tx: &Sender<UiIntent>) {
    // Validate display name.
    let name = model.edit_profile_draft.display_name.trim().to_string();
    if name.is_empty() {
        return; // client-side guard; server enforces too
    }

    let desc = model.edit_profile_draft.description.clone();
    let links = model.edit_profile_draft.links.clone();

    model.profile_save_in_flight = true;
    let _ = tx.send(UiIntent::UpdateUserProfile {
        display_name: Some(name),
        description: Some(desc),
        accent_color: Some(model.edit_profile_draft.accent_color),
        links,
    });
}

fn cancel_edit(model: &mut UiModel) {
    // Reset draft back to current live profile.
    if let Some(ref p) = model.self_profile.clone() {
        model.edit_profile_draft.display_name = if is_generated_guest_name(&p.display_name) {
            model.nick.clone()
        } else {
            p.display_name.clone()
        };
        model.edit_profile_draft.description = p.description.clone();
        model.edit_profile_draft.accent_color = p.accent_color;
        model.edit_profile_draft.links = p.links.clone();
        model.edit_profile_draft.avatar_preview_url = p.avatar_url.clone();
        model.edit_profile_draft.banner_preview_url = p.banner_url.clone();
    }
    model.edit_profile_draft.avatar_preview_bytes = None;
    model.edit_profile_draft.banner_preview_bytes = None;
    model.edit_profile_draft.pending_avatar_asset_id = None;
    model.edit_profile_draft.pending_banner_asset_id = None;
    model.edit_profile_draft.add_link_platform.clear();
    model.edit_profile_draft.add_link_url.clear();
    model.avatar_upload_in_flight = false;
    model.banner_upload_in_flight = false;
}

/// Open a native file dialog restricted to image types.
fn pick_image_file(kind: &str) -> Option<std::path::PathBuf> {
    rfd::FileDialog::new()
        .set_title(format!("Choose {kind} image"))
        .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
        .pick_file()
}

/// Validate and normalize a profile link URL.
fn validate_link_url(platform: &str, raw_url: &str) -> Result<String, &'static str> {
    let parsed = url::Url::parse(raw_url).map_err(|_| "Invalid URL")?;

    if parsed.scheme() != "https" {
        return Err("Only https:// URLs are accepted");
    }

    let host = parsed.host_str().unwrap_or("");

    if platform != "Website" {
        let required: Option<&str> = match platform {
            "Steam" => Some("steamcommunity.com"),
            "GitHub" => Some("github.com"),
            "Twitter/X" => None, // twitter.com or x.com
            "Twitch" => Some("twitch.tv"),
            "YouTube" => Some("youtube.com"),
            _ => None,
        };

        if platform == "Twitter/X" {
            if host != "twitter.com" && host != "x.com" {
                return Err("Twitter/X URL must be on twitter.com or x.com");
            }
        } else if let Some(req_host) = required {
            if !host.ends_with(req_host) {
                return Err("URL hostname does not match the selected platform");
            }
        }
    }

    // Reject unsafe schemes (already done above by checking == "https")
    // Normalize: remove trailing slash from root only.
    Ok(raw_url.trim().to_string())
}

/// Strip newlines and non-printable control characters from a single-line field.
fn sanitize_single_line(current: &str) -> String {
    let cleaned: String = current
        .chars()
        .filter(|&c| c != '\n' && c != '\r' && !c.is_control())
        .collect();
    // If something was stripped and value changed, use cleaned; otherwise keep current.
    if cleaned != current {
        cleaned
    } else {
        current.to_string()
    }
}

fn is_generated_guest_name(name: &str) -> bool {
    let trimmed = name.trim();
    trimmed.starts_with("guest-") || trimmed.starts_with("user-")
}

/// Strip control characters except standard line breaks from a multiline field.
fn sanitize_multiline(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\n' || c == '\r' || !c.is_control())
        .collect()
}

fn parse_hex_color(input: &str) -> Option<u32> {
    let s = input.trim().trim_start_matches('#');
    if s.len() == 6 {
        u32::from_str_radix(s, 16).ok()
    } else {
        None
    }
}

fn accent_color32(argb: u32) -> egui::Color32 {
    if argb == 0 {
        return theme::COLOR_ACCENT;
    }
    let r = ((argb >> 16) & 0xFF) as u8;
    let g = ((argb >> 8) & 0xFF) as u8;
    let b = (argb & 0xFF) as u8;
    egui::Color32::from_rgb(r, g, b)
}

/// Populate the edit draft from the user's current profile.
pub fn init_draft_from_profile(model: &mut UiModel) {
    if let Some(ref p) = model.self_profile.clone() {
        model.edit_profile_draft.display_name = if is_generated_guest_name(&p.display_name) {
            model.nick.clone()
        } else {
            p.display_name.clone()
        };
        model.edit_profile_draft.description = p.description.clone();
        model.edit_profile_draft.accent_color = p.accent_color;
        model.edit_profile_draft.accent_hex_input = format!("#{:06X}", p.accent_color);
        model.edit_profile_draft.links = p.links.clone();
        model.edit_profile_draft.avatar_preview_url = p.avatar_url.clone();
        model.edit_profile_draft.banner_preview_url = p.banner_url.clone();
    } else {
        // Fall back to current nick if no profile loaded yet.
        if model.edit_profile_draft.display_name.is_empty() {
            model.edit_profile_draft.display_name = model.nick.clone();
        }
    }
    model.edit_profile_draft.avatar_preview_bytes = None;
    model.edit_profile_draft.banner_preview_bytes = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_single_line_keeps_user_edits() {
        assert_eq!(sanitize_single_line("Overdose"), "Overdose");
        assert_eq!(sanitize_single_line(""), "");
    }

    #[test]
    fn sanitize_single_line_strips_control_chars() {
        assert_eq!(sanitize_single_line("Over\ndose"), "Overdose");
    }
}
