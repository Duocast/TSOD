use crate::ui::model::{
    PermissionOverrideDraft, PermissionOverrideTab, PermissionValue, PermissionViewAsMode,
    PermissionsTab, UiModel,
};
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

const CHANNEL_CAPABILITIES: &[&str] = &["View Channel", "Connect", "Speak", "Send Messages"];

pub fn show_permissions_center(ctx: &egui::Context, model: &mut UiModel) {
    if !model.show_permissions_center {
        return;
    }

    let mut open = true;
    egui::Window::new("Permissions Center")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(1180.0)
        .default_height(700.0)
        .min_width(920.0)
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
                PermissionsTab::Advanced => show_advanced_tab(ui, model),
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
    ui.columns(3, |columns| {
        let (left_slice, right_slice) = columns.split_at_mut(1);
        let left = &mut left_slice[0];
        let (center_slice, view_slice) = right_slice.split_at_mut(1);
        let center = &mut center_slice[0];
        let view = &mut view_slice[0];

        left.heading("Channels");
        left.separator();
        show_channel_tree(left, model);

        center.heading("Channel Permissions");
        center.colored_label(
            theme::text_muted(),
            format!("Editing #{}", model.permissions_channel_scope_name),
        );
        center.separator();
        center
            .checkbox(&mut model.permissions_private_channel, "Private channel")
            .on_hover_text(
                "Deny @everyone View/Connect and then explicitly allow selected roles or members.",
            );

        if model.permissions_private_channel {
            center.colored_label(
                theme::text_muted(),
                "Private mode applies: deny @everyone view/join, then add explicit allows.",
            );
        }

        center.add_space(8.0);
        center.horizontal(|ui| {
            ui.label("Overrides:");
            ui.selectable_value(
                &mut model.permissions_override_tab,
                PermissionOverrideTab::Roles,
                "Role overrides",
            );
            ui.selectable_value(
                &mut model.permissions_override_tab,
                PermissionOverrideTab::Members,
                "Member overrides",
            );
        });

        center.separator();
        show_overrides_editor(center, model);

        view.heading("View as…");
        view.separator();
        ui_view_as_panel(view, model);
    });
}

fn show_channel_tree(ui: &mut egui::Ui, model: &mut UiModel) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        for channel in &model.channels {
            let selected = model
                .permissions_selected_channel_id
                .as_ref()
                .is_some_and(|id| id == &channel.id);
            let label = match channel.channel_type {
                crate::ui::model::ChannelType::Category => format!("📁 {}", channel.name),
                crate::ui::model::ChannelType::Voice => format!("🔊 {}", channel.name),
                crate::ui::model::ChannelType::Text => format!("# {}", channel.name),
            };
            if ui.selectable_label(selected, label).clicked() {
                model.permissions_selected_channel_id = Some(channel.id.clone());
                model.permissions_channel_scope_name = channel.name.clone();
            }
        }
    });
}

fn show_overrides_editor(ui: &mut egui::Ui, model: &mut UiModel) {
    let overrides = if model.permissions_override_tab == PermissionOverrideTab::Roles {
        &mut model.permissions_role_overrides
    } else {
        &mut model.permissions_member_overrides
    };

    egui::ScrollArea::vertical().show(ui, |ui| {
        for row in overrides {
            show_override_row(ui, row);
            ui.add_space(4.0);
        }
    });

    ui.horizontal(|ui| {
        if ui.button("Add override").clicked() {
            overrides.push(PermissionOverrideDraft {
                subject_name: if model.permissions_override_tab == PermissionOverrideTab::Roles {
                    "New Role".into()
                } else {
                    "New Member".into()
                },
                capabilities: vec![PermissionValue::Inherit; CHANNEL_CAPABILITIES.len()],
            });
        }
    });
}

fn show_override_row(ui: &mut egui::Ui, row: &mut PermissionOverrideDraft) {
    ui.group(|ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label("Role/User");
            ui.text_edit_singleline(&mut row.subject_name);
            if ui.small_button("Reset").clicked() {
                row.capabilities.fill(PermissionValue::Inherit);
            }
        });

        for (idx, cap) in CHANNEL_CAPABILITIES.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(*cap);
                if idx >= row.capabilities.len() {
                    row.capabilities.push(PermissionValue::Inherit);
                }
                tri_state_button(ui, &mut row.capabilities[idx], *cap);
            });
        }
    });
}

fn tri_state_button(ui: &mut egui::Ui, value: &mut PermissionValue, capability: &str) {
    let (text, fill, source) = match *value {
        PermissionValue::Inherit => ("·", theme::text_muted(), "Inherited from role/server base"),
        PermissionValue::Allow => (
            "✓",
            egui::Color32::from_rgb(69, 179, 107),
            "Explicitly allowed on this channel",
        ),
        PermissionValue::Deny => (
            "✕",
            theme::COLOR_DANGER,
            "Explicitly denied on this channel",
        ),
    };

    let response = ui.add(
        egui::Button::new(text)
            .fill(fill)
            .min_size(egui::vec2(24.0, 20.0)),
    );
    if response.clicked() {
        *value = value.cycle();
    }
    response.on_hover_text(format!("{}\nSource: {}", capability, source));
}

fn ui_view_as_panel(ui: &mut egui::Ui, model: &mut UiModel) {
    ui.horizontal(|ui| {
        ui.selectable_value(
            &mut model.permissions_view_as_mode,
            PermissionViewAsMode::Role,
            "Role",
        );
        ui.selectable_value(
            &mut model.permissions_view_as_mode,
            PermissionViewAsMode::Member,
            "Member",
        );
    });
    ui.horizontal(|ui| {
        ui.label("Subject");
        ui.text_edit_singleline(&mut model.permissions_view_as_name);
    });

    ui.separator();
    ui.label("Computed effective permissions");

    let effective = compute_effective_permissions(model);
    for (cap, state, reason) in effective {
        let (symbol, color) = match state {
            PermissionValue::Allow => ("✓", egui::Color32::from_rgb(69, 179, 107)),
            PermissionValue::Deny => ("✕", theme::COLOR_DANGER),
            PermissionValue::Inherit => ("·", theme::text_muted()),
        };
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(color, symbol);
            ui.label(cap);
            ui.colored_label(theme::text_muted(), format!("— {}", reason));
        });
    }
}

fn compute_effective_permissions(model: &UiModel) -> Vec<(&'static str, PermissionValue, String)> {
    CHANNEL_CAPABILITIES
        .iter()
        .enumerate()
        .map(|(idx, cap)| {
            let mut state = PermissionValue::Inherit;
            let mut reason = "Inherited from server base".to_string();

            if model.permissions_private_channel && (*cap == "View Channel" || *cap == "Connect") {
                state = PermissionValue::Deny;
                reason = "Private channel baseline denies @everyone".into();
            }

            let overrides = if model.permissions_view_as_mode == PermissionViewAsMode::Role {
                &model.permissions_role_overrides
            } else {
                &model.permissions_member_overrides
            };

            if let Some(row) = overrides.iter().find(|r| {
                r.subject_name
                    .eq_ignore_ascii_case(&model.permissions_view_as_name)
            }) {
                if let Some(v) = row.capabilities.get(idx).copied() {
                    if v != PermissionValue::Inherit {
                        state = v;
                        reason = format!("Channel override on {}", row.subject_name);
                    }
                }
            }

            (*cap, state, reason)
        })
        .collect()
}

fn show_members_tab(ui: &mut egui::Ui, model: &mut UiModel) {
    ui.columns(2, |columns| {
        let (left_slice, right_slice) = columns.split_at_mut(1);
        let left = &mut left_slice[0];
        let right = &mut right_slice[0];

        left.heading("Members");
        left.horizontal(|ui| {
            ui.label("Search");
            ui.text_edit_singleline(&mut model.permissions_member_search);
        });
        left.separator();

        let filter = model.permissions_member_search.trim().to_ascii_lowercase();
        let mut visible_indexes: Vec<usize> = model
            .permissions_members
            .iter()
            .enumerate()
            .filter(|(_, member)| {
                filter.is_empty()
                    || member.display_name.to_ascii_lowercase().contains(&filter)
                    || member.user_id.to_ascii_lowercase().contains(&filter)
            })
            .map(|(idx, _)| idx)
            .collect();

        if visible_indexes.is_empty() {
            left.colored_label(theme::text_muted(), "No members match your filter.");
        } else {
            if !visible_indexes.contains(&model.permissions_selected_member) {
                model.permissions_selected_member = visible_indexes[0];
            }

            egui::ScrollArea::vertical().show(left, |ui| {
                for idx in visible_indexes.drain(..) {
                    let member = &model.permissions_members[idx];
                    if ui
                        .selectable_label(
                            model.permissions_selected_member == idx,
                            format!("{} ({})", member.display_name, member.user_id),
                        )
                        .clicked()
                    {
                        model.permissions_selected_member = idx;
                    }
                }
            });
        }

        right.heading("Member Editor");
        right.separator();

        if model.permissions_selected_member >= model.permissions_members.len() {
            model.permissions_selected_member = 0;
        }

        if let Some(member) = model
            .permissions_members
            .get_mut(model.permissions_selected_member)
        {
            right.label(format!("Editing {}", member.display_name));
            right.colored_label(theme::text_muted(), format!("User ID: {}", member.user_id));

            let editor_highest = model.permissions_current_user_max_role;
            let can_manage_this_member = member.highest_role_index < editor_highest;
            if !can_manage_this_member {
                right.colored_label(
                    theme::COLOR_DANGER,
                    "You cannot modify this member because their top role is not below yours.",
                );
            }

            right.separator();
            right.label("Role assignments");
            right.colored_label(
                theme::text_muted(),
                "Only roles below your highest role are editable (Discord Manage Roles rule).",
            );

            if member.role_assignments.len() < model.permissions_roles.len() {
                member.role_assignments.resize(model.permissions_roles.len(), false);
            }

            for (role_idx, role) in model.permissions_roles.iter().enumerate() {
                let can_edit_role = role_idx < editor_highest && can_manage_this_member;
                let mut assigned = member.role_assignments[role_idx];
                let response = right.add_enabled(
                    can_edit_role,
                    egui::Checkbox::new(
                        &mut assigned,
                        format!("{} ({})", role.name, role.member_count),
                    ),
                );

                if response.changed() {
                    member.role_assignments[role_idx] = assigned;
                    if assigned && role_idx > member.highest_role_index {
                        member.highest_role_index = role_idx;
                    }
                }

                if !can_edit_role {
                    response.on_disabled_hover_text(
                        "Manage Roles only applies to roles below your highest role and members below your role hierarchy.",
                    );
                }
            }

            right.separator();
            right.label("Quick moderation");

            let quick_actions = [
                (
                    "Mute",
                    member.can_mute_members && can_manage_this_member,
                    "Missing permission: Mute Members or target is above/equal your top role",
                ),
                (
                    "Deafen",
                    member.can_deafen_members && can_manage_this_member,
                    "Missing permission: Deafen Members or target is above/equal your top role",
                ),
                (
                    "Move",
                    member.can_move_members && can_manage_this_member,
                    "Missing permission: Move Members or target is above/equal your top role",
                ),
                (
                    "Kick",
                    member.can_kick_members && can_manage_this_member,
                    "Missing permission: Kick Members or target is above/equal your top role",
                ),
            ];

            right.horizontal_wrapped(|ui| {
                for (label, enabled, tooltip) in quick_actions {
                    ui.add_enabled(enabled, egui::Button::new(label))
                        .on_disabled_hover_text(tooltip);
                }
            });
        }
    });
}

fn show_audit_tab(ui: &mut egui::Ui) {
    ui.label("Audit log events will appear here.");
    ui.label("Recent:");
    ui.monospace("[12:01] role.update  moderator -> mentionable=true");
    ui.monospace("[11:42] channel.override  #general deny SEND_MESSAGES @everyone");
}

fn show_advanced_tab(ui: &mut egui::Ui, model: &mut UiModel) {
    ui.colored_label(theme::COLOR_DANGER, "Advanced settings are sensitive.");
    ui.label("Recommended flow: use Roles and Channels tabs for most changes.");
    ui.add_space(8.0);

    ui.checkbox(
        &mut model.permissions_advanced_enabled,
        "Enable advanced permissions system",
    )
    .on_hover_text("Hidden by default, similar to TeamSpeak advanced permissions.");

    if !model.permissions_advanced_enabled {
        ui.colored_label(
            theme::text_muted(),
            "Advanced power controls are hidden until explicitly enabled by an admin.",
        );
        return;
    }

    ui.separator();
    ui.group(|ui| {
        ui.label("Rule");
        ui.colored_label(
            theme::text_muted(),
            "You can act on a target if your power ≥ target’s needed power.",
        );
    });

    ui.separator();
    ui.columns(2, |columns| {
        let (left_slice, right_slice) = columns.split_at_mut(1);
        let left = &mut left_slice[0];
        let right = &mut right_slice[0];

        left.heading("Power values");
        power_editor(left, "Actor power", &mut model.permissions_actor_power);

        right.heading("Needed values");
        power_editor(
            right,
            "Target needed power",
            &mut model.permissions_target_needed_power,
        );
    });

    ui.separator();
    ui.heading("Target preview");
    if model.permissions_members.is_empty() {
        ui.colored_label(theme::text_muted(), "No members available for preview.");
        return;
    }

    if model.permissions_actor_preview >= model.permissions_members.len() {
        model.permissions_actor_preview = 0;
    }
    if model.permissions_target_preview >= model.permissions_members.len() {
        model.permissions_target_preview = 0;
    }

    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("advanced_actor_preview")
            .selected_text(
                model.permissions_members[model.permissions_actor_preview]
                    .display_name
                    .clone(),
            )
            .show_ui(ui, |ui| {
                for (idx, member) in model.permissions_members.iter().enumerate() {
                    ui.selectable_value(
                        &mut model.permissions_actor_preview,
                        idx,
                        &member.display_name,
                    );
                }
            });

        egui::ComboBox::from_id_salt("advanced_target_preview")
            .selected_text(
                model.permissions_members[model.permissions_target_preview]
                    .display_name
                    .clone(),
            )
            .show_ui(ui, |ui| {
                for (idx, member) in model.permissions_members.iter().enumerate() {
                    ui.selectable_value(
                        &mut model.permissions_target_preview,
                        idx,
                        &member.display_name,
                    );
                }
            });
    });

    let checks = [
        (
            "Mute",
            model.permissions_actor_power.mute_power,
            model.permissions_target_needed_power.mute_power,
        ),
        (
            "Move",
            model.permissions_actor_power.move_power,
            model.permissions_target_needed_power.move_power,
        ),
        (
            "Kick",
            model.permissions_actor_power.kick_power,
            model.permissions_target_needed_power.kick_power,
        ),
        (
            "Manage Roles",
            model.permissions_actor_power.manage_roles_power,
            model.permissions_target_needed_power.manage_roles_power,
        ),
    ];

    for (action, actor, needed) in checks {
        let allowed = actor >= needed;
        let (symbol, color) = if allowed {
            ("Allowed", egui::Color32::from_rgb(69, 179, 107))
        } else {
            ("Denied", theme::COLOR_DANGER)
        };
        ui.horizontal(|ui| {
            ui.colored_label(color, symbol);
            if allowed {
                ui.label(format!("{}: {} ≥ {}", action, actor, needed));
            } else {
                ui.label(format!(
                    "{}: {} < {} (failed value: actor {} vs needed {})",
                    action, actor, needed, actor, needed
                ));
            }
        });
    }
}

fn power_editor(
    ui: &mut egui::Ui,
    title: &str,
    power: &mut crate::ui::model::PermissionPowerDraft,
) {
    ui.group(|ui| {
        ui.label(title);
        ui.horizontal(|ui| {
            ui.label("mute_power");
            ui.add(egui::DragValue::new(&mut power.mute_power).range(0..=1000));
        });
        ui.horizontal(|ui| {
            ui.label("move_power");
            ui.add(egui::DragValue::new(&mut power.move_power).range(0..=1000));
        });
        ui.horizontal(|ui| {
            ui.label("kick_power");
            ui.add(egui::DragValue::new(&mut power.kick_power).range(0..=1000));
        });
        ui.horizontal(|ui| {
            ui.label("manage_roles_power");
            ui.add(egui::DragValue::new(&mut power.manage_roles_power).range(0..=1000));
        });
    });
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
