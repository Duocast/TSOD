use crate::ui::model::{PermissionsTab, UiModel};
use crate::ui::theme;
use eframe::egui;

const PERMISSION_GROUPS: &[(&str, &[&str])] = &[
    (
        "Voice",
        &[
            "Connect",
            "Speak",
            "Priority Speaker",
            "Mute Members",
            "Deafen Members",
            "Move Members",
        ],
    ),
    (
        "Chat",
        &[
            "View Channels",
            "Send Messages",
            "Manage Messages",
            "Create Public Threads",
        ],
    ),
    (
        "Channels",
        &["Manage Channels", "Manage Events", "Manage Webhooks"],
    ),
    (
        "Moderation",
        &["Kick Members", "Ban Members", "Timeout Members"],
    ),
    ("Admin", &["Administrator / All permissions"]),
];

pub fn show_permissions_center(ctx: &egui::Context, model: &mut UiModel) {
    if !model.show_permissions_center {
        return;
    }

    let mut open = true;
    egui::Window::new("Permissions Center")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(980.0)
        .default_height(640.0)
        .min_width(780.0)
        .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                for tab in PermissionsTab::ALL {
                    if ui
                        .selectable_label(model.permissions_tab == tab, tab.label())
                        .clicked()
                    {
                        model.permissions_tab = tab;
                    }
                }
            });
            ui.separator();

            match model.permissions_tab {
                PermissionsTab::Roles => show_roles_tab(ui, model),
                PermissionsTab::Channels => show_channels_tab(ui, model),
                PermissionsTab::Members => show_members_tab(ui, model),
                PermissionsTab::AuditLog => show_audit_tab(ui),
                PermissionsTab::Advanced => show_advanced_tab(ui),
            }
        });

    if !open {
        model.show_permissions_center = false;
    }
}

fn show_roles_tab(ui: &mut egui::Ui, model: &mut UiModel) {
    ui.columns(2, |columns| {
        let (left_slice, right_slice) = columns.split_at_mut(1);
        let left = &mut left_slice[0];
        let right = &mut right_slice[0];

        left.heading("Roles");
        left.horizontal(|ui| {
            ui.small_button("Create");
            ui.small_button("Clone");
            let protected = model
                .permissions_roles
                .get(model.permissions_selected_role)
                .is_some_and(|r| r.protected);
            ui.add_enabled(!protected, egui::Button::new("Delete"));
        });
        left.separator();

        for (idx, role) in model.permissions_roles.iter().enumerate() {
            left.horizontal(|ui| {
                ui.colored_label(parse_hex_color(&role.color_hex), "■");
                if ui
                    .selectable_label(
                        model.permissions_selected_role == idx,
                        format!("{} ({})", role.name, role.member_count),
                    )
                    .clicked()
                {
                    model.permissions_selected_role = idx;
                }
                if idx > 0 && ui.small_button("↑").clicked() {
                    model.permissions_roles.swap(idx, idx - 1);
                    model.permissions_selected_role = idx - 1;
                }
                if idx + 1 < model.permissions_roles.len() && ui.small_button("↓").clicked() {
                    model.permissions_roles.swap(idx, idx + 1);
                    model.permissions_selected_role = idx + 1;
                }
            });
        }

        right.heading("Role Editor");
        right.separator();

        if model.permissions_selected_role >= model.permissions_roles.len() {
            model.permissions_selected_role = 0;
        }

        if let Some(role) = model
            .permissions_roles
            .get_mut(model.permissions_selected_role)
        {
            if model.permissions_selected_role >= model.permissions_current_user_max_role {
                right.colored_label(
                    theme::COLOR_DANGER,
                    "⚠ You can only affect roles lower than your highest role.",
                );
            }

            right.label("General");
            right.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut role.name);
            });
            right.horizontal(|ui| {
                ui.label("Color");
                ui.text_edit_singleline(&mut role.color_hex);
            });
            right.checkbox(&mut role.hoist, "Display role members separately (hoist)");
            right.checkbox(&mut role.mentionable, "Allow anyone to mention this role");

            right.separator();
            right.label("Permissions");
            right.text_edit_singleline(&mut model.permissions_search);

            let filter = model.permissions_search.trim().to_ascii_lowercase();
            for (group, perms) in PERMISSION_GROUPS {
                right.collapsing(*group, |ui| {
                    for perm in *perms {
                        if !filter.is_empty() && !perm.to_ascii_lowercase().contains(&filter) {
                            continue;
                        }
                        let mut allowed = role.administrative || *perm == "View Channels";
                        if *perm == "Administrator / All permissions" {
                            if ui
                                .checkbox(&mut role.administrative, *perm)
                                .on_hover_text(
                                    "Administrator bypasses channel-specific restrictions.",
                                )
                                .changed()
                                && role.administrative
                            {
                                ui.ctx().copy_text(
                                    "Warning: Administrator bypasses channel overrides.".into(),
                                );
                            }
                        } else {
                            ui.checkbox(&mut allowed, *perm);
                        }
                    }
                });
            }

            right.separator();
            right.colored_label(theme::text_muted(), "Effective permissions preview");
            right.label(format!(
                "This role grants: {}",
                if role.administrative {
                    "All permissions"
                } else {
                    "Selected base permissions"
                }
            ));
            right.label(format!(
                "Denied by channel overrides in #{}: Send Messages",
                model.permissions_channel_scope_name
            ));

            right.separator();
            right.colored_label(theme::COLOR_DANGER, "Danger zone");
            right.label("Lockout prevention is active for owner/admin paths.");

            right.separator();
            right.group(|ui| {
                ui.label("Pending changes");
                ui.label("• Role metadata updated");
                ui.label("• Permission toggles changed");
                ui.horizontal(|ui| {
                    ui.button("Save");
                    ui.button("Discard");
                });
            });
        }
    });
}

fn show_channels_tab(ui: &mut egui::Ui, model: &mut UiModel) {
    ui.label("Channel permission overrides are managed from Edit Channel → Permissions.");
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label("Selected channel scope:");
        ui.text_edit_singleline(&mut model.permissions_channel_scope_name);
    });
    ui.separator();
    ui.label(
        "Tip: right-click a channel in the server tree and choose Edit Channel… then Permissions…",
    );
}

fn show_members_tab(ui: &mut egui::Ui, _model: &mut UiModel) {
    ui.label("Manage role assignments for members from Server Settings → Roles.");
    ui.label("Per-member moderation actions are gated by role hierarchy and permissions.");
}

fn show_audit_tab(ui: &mut egui::Ui) {
    ui.label("Audit log events will appear here.");
    ui.label("Recent:");
    ui.monospace("[12:01] role.update  moderator -> mentionable=true");
    ui.monospace("[11:42] channel.override  #general deny SEND_MESSAGES @everyone");
}

fn show_advanced_tab(ui: &mut egui::Ui) {
    ui.colored_label(theme::COLOR_DANGER, "Advanced settings are sensitive.");
    ui.label("Recommended flow: use Roles and Channels tabs for most changes.");
}

fn parse_hex_color(hex: &str) -> egui::Color32 {
    let s = hex.trim_start_matches('#');
    if s.len() != 6 {
        return theme::text_muted();
    }
    let Ok(r) = u8::from_str_radix(&s[0..2], 16) else {
        return theme::text_muted();
    };
    let Ok(g) = u8::from_str_radix(&s[2..4], 16) else {
        return theme::text_muted();
    };
    let Ok(b) = u8::from_str_radix(&s[4..6], 16) else {
        return theme::text_muted();
    };
    egui::Color32::from_rgb(r, g, b)
}
