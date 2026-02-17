use crate::{ChannelId, ControlError, ControlResult, PermissionContext, ServerId};
use crate::perms::{Capability, PermissionDb};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct ChannelConfig {
    pub name: String,
    pub parent: Option<ChannelId>,
    pub max_members: Option<usize>,
    pub max_talkers: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Channel {
    pub id: ChannelId,
    pub server_id: ServerId,
    pub cfg: ChannelConfig,
}

pub struct ChannelService {
    server_id: ServerId,
    perms: PermissionDb,
    channels: HashMap<ChannelId, Channel>,
}

impl ChannelService {
    pub fn new(server_id: ServerId, perms: PermissionDb) -> Self {
        Self { server_id, perms, channels: HashMap::new() }
    }

    pub fn perms(&self) -> &PermissionDb { &self.perms }
    pub fn perms_mut(&mut self) -> &mut PermissionDb { &mut self.perms }

    pub fn create_channel(&mut self, ctx: &PermissionContext, cfg: ChannelConfig) -> ControlResult<Channel> {
        self.perms.check(ctx, None, Capability::CreateChannel)?;

        if cfg.name.trim().is_empty() {
            return Err(ControlError::InvalidArgument("channel name empty"));
        }

        let id = ChannelId::new();
        let ch = Channel { id: id.clone(), server_id: self.server_id.clone(), cfg };
        self.channels.insert(id.clone(), ch.clone());
        Ok(ch)
    }

    pub fn get(&self, id: &ChannelId) -> ControlResult<Channel> {
        self.channels.get(id).cloned().ok_or(ControlError::NotFound("channel"))
    }

    pub fn list(&self) -> Vec<Channel> {
        self.channels.values().cloned().collect()
    }
}
