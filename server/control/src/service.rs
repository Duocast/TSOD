use crate::{
    audit::AuditWriter,
    errors::{ControlError, ControlResult},
    ids::*,
    model::*,
    perms::{Capability, PermissionDecision},
    repo::ControlRepo,
};
use chrono::Utc;
use serde_json::json;
use ulid::Ulid;

#[derive(Clone, Debug)]
pub struct RequestContext {
    pub server_id: ServerId,
    pub user_id: UserId,
    pub is_admin: bool,
}

#[derive(Clone)]
pub struct ControlService<R: ControlRepo> {
    repo: R,
    audit: AuditWriter,
}

impl<R: ControlRepo> ControlService<R> {
    pub fn new(repo: R) -> Self {
        Self { repo, audit: AuditWriter }
    }

    pub async fn create_channel(&self, ctx: &RequestContext, create: ChannelCreate) -> ControlResult<Channel> {
        if create.name.trim().is_empty() {
            return Err(ControlError::InvalidArgument("channel name empty"));
        }

        let mut tx = self.repo.tx().await?;

        self.require(&mut tx, ctx, None, Capability::CreateChannel).await?;

        let now = Utc::now();
        let ch = Channel {
            id: ChannelId::new(),
            server_id: ctx.server_id,
            name: create.name,
            parent_id: create.parent_id,
            max_members: create.max_members,
            max_talkers: create.max_talkers,
            created_at: now,
            updated_at: now,
        };

        self.repo.create_channel(&mut tx, ch.clone()).await?;

        // audit
        self.audit.write(
            &self.repo,
            &mut tx,
            ctx.server_id,
            Some(ctx.user_id),
            "channel.create",
            "channel",
            &ch.id.0.to_string(),
            json!({"name": ch.name, "parent_id": ch.parent_id.map(|p| p.0)}),
        ).await?;

        // outbox event
        self.repo.insert_outbox(
            &mut tx,
            Ulid::new().to_string(),
            ctx.server_id,
            "channel",
            &ch.id.0.to_string(),
            json!({"type":"channel.created","channel_id": ch.id.0, "name": ch.name}),
        ).await?;

        tx.commit().await?;
        Ok(ch)
    }

    pub async fn join_channel(&self, ctx: &RequestContext, join: JoinChannel) -> ControlResult<Vec<Member>> {
        if join.display_name.trim().is_empty() {
            return Err(ControlError::InvalidArgument("display name empty"));
        }

        let mut tx = self.repo.tx().await?;

        self.require(&mut tx, ctx, Some(join.channel_id), Capability::JoinChannel).await?;

        let ch = self.repo.get_channel(&mut tx, ctx.server_id, join.channel_id).await?;

        if let Some(max) = ch.max_members {
            let cur = self.repo.count_members(&mut tx, join.channel_id).await?;
            if cur >= max as i64 {
                return Err(ControlError::ResourceExhausted("channel full"));
            }
        }

        let m = Member {
            channel_id: join.channel_id,
            user_id: ctx.user_id,
            display_name: join.display_name,
            muted: false,
            deafened: false,
            joined_at: Utc::now(),
        };

        self.repo.upsert_member(&mut tx, m.clone()).await?;

        self.audit.write(
            &self.repo,
            &mut tx,
            ctx.server_id,
            Some(ctx.user_id),
            "member.join",
            "channel",
            &join.channel_id.0.to_string(),
            json!({"user_id": ctx.user_id.0}),
        ).await?;

        self.repo.insert_outbox(
            &mut tx,
            Ulid::new().to_string(),
            ctx.server_id,
            "presence",
            &join.channel_id.0.to_string(),
            json!({"type":"presence.member_joined","channel_id": join.channel_id.0, "user_id": ctx.user_id.0, "display_name": m.display_name}),
        ).await?;

        let members = self.repo.list_members(&mut tx, join.channel_id).await?;
        tx.commit().await?;
        Ok(members)
    }

    pub async fn leave_channel(&self, ctx: &RequestContext, channel_id: ChannelId) -> ControlResult<()> {
        let mut tx = self.repo.tx().await?;

        self.repo.delete_member(&mut tx, channel_id, ctx.user_id).await?;

        self.audit.write(
            &self.repo,
            &mut tx,
            ctx.server_id,
            Some(ctx.user_id),
            "member.leave",
            "channel",
            &channel_id.0.to_string(),
            json!({"user_id": ctx.user_id.0}),
        ).await?;

        self.repo.insert_outbox(
            &mut tx,
            Ulid::new().to_string(),
            ctx.server_id,
            "presence",
            &channel_id.0.to_string(),
            json!({"type":"presence.member_left","channel_id": channel_id.0, "user_id": ctx.user_id.0}),
        ).await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn set_mute(
        &self,
        ctx: &RequestContext,
        channel_id: ChannelId,
        target: UserId,
        muted: bool,
    ) -> ControlResult<Member> {
        let mut tx = self.repo.tx().await?;

        self.require(&mut tx, ctx, Some(channel_id), Capability::ModerateMembers).await?;

        let mut member = self.repo.get_member(&mut tx, channel_id, target).await?;
        member.muted = muted;
        self.repo.upsert_member(&mut tx, member.clone()).await?;

        self.audit.write(
            &self.repo,
            &mut tx,
            ctx.server_id,
            Some(ctx.user_id),
            "member.mute",
            "user",
            &target.0.to_string(),
            json!({"channel_id": channel_id.0, "muted": muted}),
        ).await?;

        self.repo.insert_outbox(
            &mut tx,
            Ulid::new().to_string(),
            ctx.server_id,
            "presence",
            &channel_id.0.to_string(),
            json!({"type":"presence.voice_state_changed","channel_id": channel_id.0, "user_id": target.0, "muted": muted, "deafened": member.deafened}),
        ).await?;

        tx.commit().await?;
        Ok(member)
    }

    async fn require(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        ctx: &RequestContext,
        channel: Option<ChannelId>,
        cap: Capability,
    ) -> ControlResult<()> {
        if ctx.is_admin {
            return Ok(());
        }
        let decision = self.repo.decide_permission(tx, ctx.server_id, channel, ctx.user_id, cap).await?;
        match decision {
            PermissionDecision::Allow => Ok(()),
            PermissionDecision::Deny => Err(ControlError::PermissionDenied("capability denied")),
        }
    }
}
