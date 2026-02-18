// server/control/src/service.rs
//
// Fixes E0282 “cannot infer type” by:
// 1) Using UFCS (`<R as ControlRepo>::method(...)`) so the compiler picks the ControlRepo trait method
//    even if there are overlapping inherent/trait methods.
// 2) Never using “dangling” awaited values in a way that leaves method resolution ambiguous.
//
// This file assumes the “production” repo signatures we’ve been converging on:
// - get_channel(tx, server_id, channel_id) -> ControlResult<Channel>
// - get_member(tx, server_id, channel_id, user_id) -> ControlResult<Member>
// - upsert_member(tx, server_id, &Member) -> ControlResult<()>
// - delete_member(tx, server_id, channel_id, user_id) -> ControlResult<()>
// - list_members(tx, server_id, channel_id) -> ControlResult<Vec<Member>>
// - count_members(tx, server_id, channel_id) -> ControlResult<i64>
// - insert_chat_message(tx, &ChatMessage) -> ControlResult<()>
// - insert_outbox(tx, &OutboxEvent) -> ControlResult<()>
// - decide_permission(tx, &PermissionRequest) -> ControlResult<Decision>

use crate::{
    audit::AuditWriter,
    errors::{ControlError, ControlResult},
    ids::*,
    model::*,
    perms::{Capability, Decision, PermissionRequest},
    repo::ControlRepo,
};
use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

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

    #[inline]
    pub fn repo(&self) -> &R {
        &self.repo
    }

    // -------------------------------------------------------------------------
    // Channels
    // -------------------------------------------------------------------------

    pub async fn create_channel(&self, ctx: &RequestContext, create: ChannelCreate) -> ControlResult<Channel> {
        let name = create.name.trim();
        if name.is_empty() {
            return Err(ControlError::InvalidArgument("channel name empty"));
        }
        if name.len() > 64 {
            return Err(ControlError::InvalidArgument("channel name too long"));
        }

        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;

        self.require(&mut tx, ctx, None, Capability::CreateChannel).await?;

        let now = Utc::now();
        let ch = Channel {
            id: ChannelId(Uuid::new_v4()),
            server_id: ctx.server_id,
            name: name.to_string(),
            parent_id: create.parent_id,
            max_members: create.max_members,
            max_talkers: create.max_talkers,
            created_at: now,
            updated_at: now,
        };

        <R as ControlRepo>::create_channel(&self.repo, &mut tx, &ch).await?;

        // audit
        self.audit
            .write(
                &self.repo,
                &mut tx,
                ctx.server_id,
                Some(ctx.user_id),
                "channel.create",
                "channel",
                &ch.id.0.to_string(),
                json!({"name": ch.name, "parent_id": ch.parent_id.map(|p| p.0)}),
            )
            .await?;

        // outbox push
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent::new(
                ctx.server_id,
                "channel.created",
                json!({
                    "channel_id": ch.id.0,
                    "name": ch.name,
                    "parent_id": ch.parent_id.map(|p| p.0),
                    "max_members": ch.max_members,
                    "max_talkers": ch.max_talkers,
                }),
            ),
        )
        .await?;

        tx.commit().await?;
        Ok(ch)
    }

    pub async fn get_channel(&self, ctx: &RequestContext, channel_id: ChannelId) -> ControlResult<Channel> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, Some(channel_id), Capability::JoinChannel).await?;

        let ch = <R as ControlRepo>::get_channel(&self.repo, &mut tx, ctx.server_id, channel_id).await?;
        tx.commit().await?;
        Ok(ch)
    }

    // -------------------------------------------------------------------------
    // Membership
    // -------------------------------------------------------------------------

    pub async fn join_channel(&self, ctx: &RequestContext, join: JoinChannel) -> ControlResult<Vec<Member>> {
        let dn = join.display_name.trim();
        if dn.is_empty() {
            return Err(ControlError::InvalidArgument("display name empty"));
        }
        if dn.len() > 64 {
            return Err(ControlError::InvalidArgument("display name too long"));
        }

        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, Some(join.channel_id), Capability::JoinChannel).await?;

        // Ensure channel exists (UFCS avoids E0282 ambiguity)
        let ch = <R as ControlRepo>::get_channel(&self.repo, &mut tx, ctx.server_id, join.channel_id).await?;

        if let Some(max) = ch.max_members {
            let cur = <R as ControlRepo>::count_members(&self.repo, &mut tx, ctx.server_id, join.channel_id).await?;
            if cur >= max as i64 {
                return Err(ControlError::ResourceExhausted("channel full"));
            }
        }

        let m = Member {
            channel_id: join.channel_id,
            user_id: ctx.user_id,
            display_name: dn.to_string(),
            muted: false,
            deafened: false,
            joined_at: Utc::now(),
        };

        <R as ControlRepo>::upsert_member(&self.repo, &mut tx, ctx.server_id, &m).await?;

        self.audit
            .write(
                &self.repo,
                &mut tx,
                ctx.server_id,
                Some(ctx.user_id),
                "member.join",
                "channel",
                &join.channel_id.0.to_string(),
                json!({"user_id": ctx.user_id.0}),
            )
            .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent::new(
                ctx.server_id,
                "presence.member_joined",
                json!({
                    "channel_id": join.channel_id.0,
                    "user_id": ctx.user_id.0,
                    "display_name": m.display_name,
                    "muted": m.muted,
                    "deafened": m.deafened,
                }),
            ),
        )
        .await?;

        let members = <R as ControlRepo>::list_members(&self.repo, &mut tx, ctx.server_id, join.channel_id).await?;
        tx.commit().await?;
        Ok(members)
    }

    pub async fn leave_channel(&self, ctx: &RequestContext, channel_id: ChannelId) -> ControlResult<()> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;

        // Ensure member exists (useful error; also avoids weird downstream states)
        let _m = <R as ControlRepo>::get_member(&self.repo, &mut tx, ctx.server_id, channel_id, ctx.user_id).await?;

        <R as ControlRepo>::delete_member(&self.repo, &mut tx, ctx.server_id, channel_id, ctx.user_id).await?;

        self.audit
            .write(
                &self.repo,
                &mut tx,
                ctx.server_id,
                Some(ctx.user_id),
                "member.leave",
                "channel",
                &channel_id.0.to_string(),
                json!({"user_id": ctx.user_id.0}),
            )
            .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent::new(
                ctx.server_id,
                "presence.member_left",
                json!({"channel_id": channel_id.0, "user_id": ctx.user_id.0}),
            ),
        )
        .await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn set_mute(&self, ctx: &RequestContext, channel_id: ChannelId, target: UserId, muted: bool) -> ControlResult<Member> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, Some(channel_id), Capability::ModerateMembers).await?;

        let mut member = <R as ControlRepo>::get_member(&self.repo, &mut tx, ctx.server_id, channel_id, target).await?;
        member.muted = muted;

        <R as ControlRepo>::upsert_member(&self.repo, &mut tx, ctx.server_id, &member).await?;

        self.audit
            .write(
                &self.repo,
                &mut tx,
                ctx.server_id,
                Some(ctx.user_id),
                if muted { "member.mute" } else { "member.unmute" },
                "user",
                &target.0.to_string(),
                json!({"channel_id": channel_id.0, "muted": muted}),
            )
            .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent::new(
                ctx.server_id,
                "presence.voice_state_changed",
                json!({"channel_id": channel_id.0, "user_id": target.0, "muted": muted, "deafened": member.deafened}),
            ),
        )
        .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent::new(
                ctx.server_id,
                "moderation.user_muted",
                json!({"channel_id": channel_id.0, "target_user_id": target.0, "muted": muted, "duration_seconds": 0, "actor_user_id": ctx.user_id.0}),
            ),
        )
        .await?;

        tx.commit().await?;
        Ok(member)
    }

    // -------------------------------------------------------------------------
    // Chat
    // -------------------------------------------------------------------------

    pub async fn send_message(&self, ctx: &RequestContext, msg: SendMessage) -> ControlResult<ChatMessage> {
        let text = msg.text.trim();
        if text.is_empty() {
            return Err(ControlError::InvalidArgument("message text empty"));
        }
        if text.len() > 2000 {
            return Err(ControlError::InvalidArgument("message too long"));
        }

        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, Some(msg.channel_id), Capability::SendMessage).await?;

        // Ensure member exists (UFCS avoids E0282 ambiguity)
        let _m = <R as ControlRepo>::get_member(&self.repo, &mut tx, ctx.server_id, msg.channel_id, ctx.user_id).await?;

        let rec = ChatMessage {
            id: MessageId(Uuid::new_v4()),
            server_id: ctx.server_id,
            channel_id: msg.channel_id,
            author_user_id: ctx.user_id,
            text: text.to_string(),
            attachments: msg.attachments,
            created_at: Utc::now(),
        };

        <R as ControlRepo>::insert_chat_message(&self.repo, &mut tx, &rec).await?;

        self.audit
            .write(
                &self.repo,
                &mut tx,
                ctx.server_id,
                Some(ctx.user_id),
                "chat.message",
                "channel",
                &msg.channel_id.0.to_string(),
                json!({"message_id": rec.id.0, "text_len": rec.text.len()}),
            )
            .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent::new(
                ctx.server_id,
                "chat.message_posted",
                json!({
                    "message_id": rec.id.0,
                    "channel_id": rec.channel_id.0,
                    "author_user_id": rec.author_user_id.0,
                    "text": rec.text,
                    "attachments": rec.attachments,
                }),
            ),
        )
        .await?;

        tx.commit().await?;
        Ok(rec)
    }

    // -------------------------------------------------------------------------
    // Permissions
    // -------------------------------------------------------------------------

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

        let req = PermissionRequest {
            server_id: ctx.server_id,
            channel_id: channel,
            user_id: ctx.user_id,
            capability: cap,
        };

        match <R as ControlRepo>::decide_permission(&self.repo, tx, &req).await? {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(ControlError::PermissionDenied("capability denied")),
        }
    }
}
