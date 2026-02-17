use std::collections::{HashMap, HashSet};

use crate::{ChannelId, ControlError, ControlResult, ServerId, UserId};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoleId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    // Channel
    JoinChannel,
    Speak,
    Stream,
    Upload,

    // Admin
    CreateChannel,
    ManageChannel,
    ModerateMembers,
    ManageRoles,
}

#[derive(Clone, Debug)]
pub struct Role {
    pub id: RoleId,
    pub name: String,
    pub grants: HashSet<Capability>,
    pub denies: HashSet<Capability>, // explicit denies for "negate" semantics
}

impl Role {
    pub fn new(id: RoleId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            grants: HashSet::new(),
            denies: HashSet::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PermissionContext {
    pub server_id: ServerId,
    pub user_id: UserId,
    pub roles: Vec<RoleId>,
    pub channel_overrides: HashMap<ChannelId, ChannelOverride>,
}

/// Optional per-channel overrides (grant/deny set).
#[derive(Clone, Debug, Default)]
pub struct ChannelOverride {
    pub grants: HashSet<Capability>,
    pub denies: HashSet<Capability>,
}

/// In-memory permission database (server-wide).
/// Later you can back this by Postgres.
#[derive(Clone, Debug, Default)]
pub struct PermissionDb {
    pub roles: HashMap<RoleId, Role>,
    pub user_roles: HashMap<UserId, Vec<RoleId>>,
    pub channel_overrides: HashMap<(ChannelId, UserId), ChannelOverride>,
}

impl PermissionDb {
    pub fn new_with_defaults() -> Self {
        let mut db = Self::default();

        // Default roles
        let mut admin = Role::new(RoleId("admin".into()), "admin");
        admin.grants.extend([
            Capability::JoinChannel,
            Capability::Speak,
            Capability::Stream,
            Capability::Upload,
            Capability::CreateChannel,
            Capability::ManageChannel,
            Capability::ModerateMembers,
            Capability::ManageRoles,
        ]);

        let mut member = Role::new(RoleId("member".into()), "member");
        member.grants.extend([
            Capability::JoinChannel,
            Capability::Speak,
            Capability::Stream,
            Capability::Upload,
        ]);

        db.roles.insert(admin.id.clone(), admin);
        db.roles.insert(member.id.clone(), member);

        db
    }

    pub fn set_user_roles(&mut self, user_id: UserId, roles: Vec<RoleId>) {
        self.user_roles.insert(user_id, roles);
    }

    pub fn set_channel_override(&mut self, channel_id: ChannelId, user_id: UserId, ov: ChannelOverride) {
        self.channel_overrides.insert((channel_id, user_id), ov);
    }

    pub fn build_context(&self, server_id: ServerId, user_id: UserId) -> PermissionContext {
        let roles = self.user_roles.get(&user_id).cloned().unwrap_or_else(|| vec![RoleId("member".into())]);

        let mut channel_overrides = HashMap::new();
        for ((cid, uid), ov) in &self.channel_overrides {
            if *uid == user_id {
                channel_overrides.insert(cid.clone(), ov.clone());
            }
        }

        PermissionContext { server_id, user_id, roles, channel_overrides }
    }

    pub fn check(&self, ctx: &PermissionContext, channel: Option<&ChannelId>, cap: Capability) -> ControlResult<()> {
        // Channel override denies win first (TS-like negate)
        if let Some(cid) = channel {
            if let Some(ov) = ctx.channel_overrides.get(cid) {
                if ov.denies.contains(&cap) {
                    return Err(ControlError::PermissionDenied("capability denied by channel override"));
                }
            }
        }

        // Role denies next
        for rid in &ctx.roles {
            if let Some(role) = self.roles.get(rid) {
                if role.denies.contains(&cap) {
                    return Err(ControlError::PermissionDenied("capability denied by role"));
                }
            }
        }

        // Channel override grants
        if let Some(cid) = channel {
            if let Some(ov) = ctx.channel_overrides.get(cid) {
                if ov.grants.contains(&cap) {
                    return Ok(());
                }
            }
        }

        // Role grants
        for rid in &ctx.roles {
            if let Some(role) = self.roles.get(rid) {
                if role.grants.contains(&cap) {
                    return Ok(());
                }
            }
        }

        Err(ControlError::PermissionDenied("capability not granted"))
    }
}
