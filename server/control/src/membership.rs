use crate::{
    channels::ChannelService,
    errors::{ControlError, ControlResult},
    events::{ControlEvent, PresenceKind},
    perms::{Capability, PermissionContext},
    ChannelId, UserId,
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct MemberState {
    pub user_id: UserId,
    pub display_name: String,
    pub muted: bool,
    pub deafened: bool,
}

#[derive(Default)]
pub struct MembershipService {
    // channel -> set(user_id)
    members: HashMap<ChannelId, HashSet<UserId>>,
    // (channel,user) -> state
    state: HashMap<(ChannelId, UserId), MemberState>,
}

impl MembershipService {
    pub fn new() -> Self { Self::default() }

    pub fn join(
        &mut self,
        ch_svc: &ChannelService,
        ctx: &PermissionContext,
        channel_id: &ChannelId,
        display_name: String,
    ) -> ControlResult<Vec<ControlEvent>> {
        ch_svc.perms().check(ctx, Some(channel_id), Capability::JoinChannel)?;
        ch_svc.perms().check(ctx, Some(channel_id), Capability::Speak)?; // policy: join implies can speak unless muted later

        let channel = ch_svc.get(channel_id)?;
        if let Some(max) = channel.cfg.max_members {
            let cur = self.members.get(channel_id).map(|s| s.len()).unwrap_or(0);
            if cur >= max {
                return Err(ControlError::ResourceExhausted("channel full").into());
            }
        }

        let entry = self.members.entry(channel_id.clone()).or_default();
        if entry.contains(&ctx.user_id) {
            return Err(ControlError::FailedPrecondition("already in channel"));
        }

        entry.insert(ctx.user_id.clone());
        let st = MemberState {
            user_id: ctx.user_id.clone(),
            display_name,
            muted: false,
            deafened: false,
        };
        self.state.insert((channel_id.clone(), ctx.user_id.clone()), st.clone());

        Ok(vec![ControlEvent::Presence {
            channel_id: channel_id.clone(),
            kind: PresenceKind::MemberJoined(st),
        }])
    }

    pub fn leave(
        &mut self,
        ctx: &PermissionContext,
        channel_id: &ChannelId,
    ) -> ControlResult<Vec<ControlEvent>> {
        let entry = self.members.get_mut(channel_id).ok_or(ControlError::NotFound("channel membership"))?;
        if !entry.remove(&ctx.user_id) {
            return Err(ControlError::FailedPrecondition("not in channel"));
        }
        self.state.remove(&(channel_id.clone(), ctx.user_id.clone()));

        Ok(vec![ControlEvent::Presence {
            channel_id: channel_id.clone(),
            kind: PresenceKind::MemberLeft(ctx.user_id.clone()),
        }])
    }

    pub fn list_members(&self, channel_id: &ChannelId) -> Vec<MemberState> {
        self.members
            .get(channel_id)
            .into_iter()
            .flat_map(|set| set.iter())
            .filter_map(|uid| self.state.get(&(channel_id.clone(), uid.clone())).cloned())
            .collect()
    }

    pub fn set_mute(
        &mut self,
        ch_svc: &ChannelService,
        actor: &PermissionContext,
        channel_id: &ChannelId,
        target: &UserId,
        muted: bool,
    ) -> ControlResult<Vec<ControlEvent>> {
        ch_svc.perms().check(actor, Some(channel_id), Capability::ModerateMembers)?;

        let key = (channel_id.clone(), target.clone());
        let st = self.state.get_mut(&key).ok_or(ControlError::NotFound("member state"))?;
        st.muted = muted;

        Ok(vec![ControlEvent::Presence {
            channel_id: channel_id.clone(),
            kind: PresenceKind::VoiceStateChanged {
                user_id: target.clone(),
                muted: st.muted,
                deafened: st.deafened,
            },
        }])
    }

    pub fn set_deafen(
        &mut self,
        ch_svc: &ChannelService,
        actor: &PermissionContext,
        channel_id: &ChannelId,
        target: &UserId,
        deafened: bool,
    ) -> ControlResult<Vec<ControlEvent>> {
        ch_svc.perms().check(actor, Some(channel_id), Capability::ModerateMembers)?;

        let key = (channel_id.clone(), target.clone());
        let st = self.state.get_mut(&key).ok_or(ControlError::NotFound("member state"))?;
        st.deafened = deafened;

        Ok(vec![ControlEvent::Presence {
            channel_id: channel_id.clone(),
            kind: PresenceKind::VoiceStateChanged {
                user_id: target.clone(),
                muted: st.muted,
                deafened: st.deafened,
            },
        }])
    }
}

// Tiny helper to map error variant used above.
trait ResourceExhaustedExt<T> {
    fn into(self) -> ControlResult<T>;
}

impl<T> ResourceExhaustedExt<T> for ControlError {
    fn into(self) -> ControlResult<T> {
        Err(self)
    }
}
