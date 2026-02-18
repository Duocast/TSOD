use chrono::Utc;
use serde_json::{json, Value as JsonValue};
use uuid::Uuid;

use crate::{
    errors::{ControlError, ControlResult},
    ids::{ChannelId, MessageId, OutboxId, ServerId, UserId},
    model::{
        AuditEntry, Channel, ChannelCreate, ChatMessage, JoinChannel, Member, OutboxEvent,
        OutboxEventRow, PermissionRequest, SendMessage,
    },
    perms::{Capability, PermissionDecision},
    repo::ControlRepo,
};

#[derive(Clone, Copy, Debug)]
pub struct RequestContext {
    pub server_id: ServerId,
    pub user_id: UserId,
    pub is_admin: bool,
}

#[derive(Clone)]
pub struct ControlService<R: ControlRepo> {
    repo: R,
}

impl<R: ControlRepo> ControlService<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    #[inline]
    pub fn repo(&self) -> &R {
        &self.repo
    }

    // -------------------------------------------------------------------------
    // Channels
    // -------------------------------------------------------------------------

    pub async fn create_channel(&self, ctx: &RequestContext, req: ChannelCreate) -> ControlResult<Channel> {
        let name = req.name.trim();
        if name.is_empty() {
            return Err(ControlError::InvalidArgument("channel name empty"));
        }
        if name.len() > 64 {
            return Err(ControlError::InvalidArgument("channel name too long"));
        }

        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, None, None, Capability::CreateChannel).await?;

        let now = Utc::now();
        let ch = Channel {
            id: ChannelId(Uuid::new_v4()),
            server_id: ctx.server_id,
            name: name.to_string(),
            parent_id: req.parent_id,
            max_members: req.max_members,
            max_talkers: req.max_talkers,
            created_at: now,
            updated_at: now,
        };

        <R as ControlRepo>::create_channel(&self.repo, &mut tx, &ch).await?;

        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                "channel.create",
                "channel",
                ch.id.0.to_string(),
                json!({ "name": ch.name, "parent_id": ch.parent_id.map(|p| p.0) }),
            ),
        )
        .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "channel.created".to_string(),
                payload_json: json!({
                    "channel_id": ch.id.0,
                    "name": ch.name,
                    "parent_id": ch.parent_id.map(|p| p.0),
                    "max_members": ch.max_members,
                    "max_talkers": ch.max_talkers,
                }),
            },
        )
        .await?;

        tx.commit().await?;
        Ok(ch)
    }

    pub async fn get_channel(&self, ctx: &RequestContext, channel_id: ChannelId) -> ControlResult<Channel> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, Some(channel_id), None, Capability::JoinChannel).await?;

        let ch = <R as ControlRepo>::get_channel(&self.repo, &mut tx, ctx.server_id, channel_id)
            .await?
            .ok_or(ControlError::NotFound("channel"))?;
        tx.commit().await?;
        Ok(ch)
    }

    // -------------------------------------------------------------------------
    // Membership
    // -------------------------------------------------------------------------

    pub async fn join_channel(&self, ctx: &RequestContext, req: JoinChannel) -> ControlResult<Vec<Member>> {
        let dn = req.display_name.trim();
        if dn.is_empty() {
            return Err(ControlError::InvalidArgument("display name empty"));
        }
        if dn.len() > 64 {
            return Err(ControlError::InvalidArgument("display name too long"));
        }

        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, Some(req.channel_id), None, Capability::JoinChannel).await?;

        // Ensure channel exists
        let ch = <R as ControlRepo>::get_channel(&self.repo, &mut tx, ctx.server_id, req.channel_id)
            .await?
            .ok_or(ControlError::NotFound("channel"))?;

        // Optional capacity check
        if let Some(max) = ch.max_members {
            let cur = <R as ControlRepo>::count_members(&self.repo, &mut tx, ctx.server_id, req.channel_id).await?;
            if cur >= max as i64 {
                return Err(ControlError::ResourceExhausted("channel full"));
            }
        }

        let m = Member {
            channel_id: req.channel_id,
            user_id: ctx.user_id,
            display_name: dn.to_string(),
            muted: false,
            deafened: false,
            joined_at: Utc::now(),
        };

        <R as ControlRepo>::upsert_member(&self.repo, &mut tx, ctx.server_id, &m).await?;

        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                "member.join",
                "channel",
                req.channel_id.0.to_string(),
                json!({ "user_id": ctx.user_id.0 }),
            ),
        )
        .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "presence.member_joined".to_string(),
                payload_json: json!({
                    "channel_id": req.channel_id.0,
                    "user_id": ctx.user_id.0,
                    "display_name": m.display_name,
                    "muted": m.muted,
                    "deafened": m.deafened,
                }),
            },
        )
        .await?;

        let members = <R as ControlRepo>::list_members(&self.repo, &mut tx, ctx.server_id, req.channel_id).await?;
        tx.commit().await?;
        Ok(members)
    }

    pub async fn leave_channel(&self, ctx: &RequestContext, channel_id: ChannelId) -> ControlResult<()> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;

        self.require(&mut tx, ctx, Some(channel_id), None, Capability::JoinChannel).await?;
        let _m = <R as ControlRepo>::get_member(&self.repo, &mut tx, ctx.server_id, channel_id, ctx.user_id)
            .await?
            .ok_or(ControlError::NotFound("member"))?;

        <R as ControlRepo>::delete_member(&self.repo, &mut tx, ctx.server_id, channel_id, ctx.user_id).await?;

        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                "member.leave",
                "channel",
                channel_id.0.to_string(),
                json!({ "user_id": ctx.user_id.0 }),
            ),
        )
        .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "presence.member_left".to_string(),
                payload_json: json!({
                    "channel_id": channel_id.0,
                    "user_id": ctx.user_id.0
                }),
            },
        )
        .await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn set_voice_mute(
        &self,
        ctx: &RequestContext,
        channel_id: ChannelId,
        target_user: UserId,
        muted: bool,
        reason: Option<String>,
    ) -> ControlResult<Member> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(
            &mut tx,
            ctx,
            Some(channel_id),
            Some(target_user),
            Capability::MuteVoice,
        )
        .await?;

        let mut m = <R as ControlRepo>::get_member(&self.repo, &mut tx, ctx.server_id, channel_id, target_user)
            .await?
            .ok_or(ControlError::NotFound("member"))?;
        m.muted = muted;

        <R as ControlRepo>::upsert_member(&self.repo, &mut tx, ctx.server_id, &m).await?;

        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                if muted { "moderation.mute" } else { "moderation.unmute" },
                "user",
                target_user.0.to_string(),
                json!({ "channel_id": channel_id.0, "muted": muted, "reason": reason }),
            ),
        )
        .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "presence.voice_state_changed".to_string(),
                payload_json: json!({
                    "channel_id": channel_id.0,
                    "user_id": target_user.0,
                    "muted": muted,
                    "deafened": m.deafened
                }),
            },
        )
        .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "moderation.user_muted".to_string(),
                payload_json: json!({
                    "channel_id": channel_id.0,
                    "target_user_id": target_user.0,
                    "actor_user_id": ctx.user_id.0,
                    "muted": muted,
                    "reason": reason
                }),
            },
        )
        .await?;

        tx.commit().await?;
        Ok(m)
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
        self.require(&mut tx, ctx, Some(msg.channel_id), None, Capability::SendMessage).await?;

        // Ensure member exists
        let _m = <R as ControlRepo>::get_member(&self.repo, &mut tx, ctx.server_id, msg.channel_id, ctx.user_id)
            .await?
            .ok_or(ControlError::NotFound("member"))?;

        let rec = ChatMessage {
            id: MessageId(Uuid::new_v4()),
            server_id: ctx.server_id,
            channel_id: msg.channel_id,
            author_user_id: ctx.user_id,
            text: text.to_string(),
            attachments: msg.attachments.unwrap_or_else(|| json!([])),
            created_at: Utc::now(),
        };

        <R as ControlRepo>::insert_chat_message(&self.repo, &mut tx, &rec).await?;

        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                "chat.message",
                "channel",
                msg.channel_id.0.to_string(),
                json!({ "message_id": rec.id.0, "text_len": rec.text.len() }),
            ),
        )
        .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "chat.message_posted".to_string(),
                payload_json: json!({
                    "message_id": rec.id.0,
                    "channel_id": rec.channel_id.0,
                    "author_user_id": rec.author_user_id.0,
                    "text": rec.text,
                    "attachments": rec.attachments,
                    "created_at": rec.created_at,
                }),
            },
        )
        .await?;

        tx.commit().await?;
        Ok(rec)
    }

    // -------------------------------------------------------------------------
    // Outbox helpers (optional – if your gateway uses these)
    // -------------------------------------------------------------------------

    pub async fn claim_outbox_batch(&self, server: ServerId, limit: i64) -> ControlResult<(Uuid, Vec<OutboxEventRow>)> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        let token = Uuid::new_v4();
        let rows = <R as ControlRepo>::claim_outbox_batch(&self.repo, &mut tx, server, token, limit).await?;
        tx.commit().await?;
        Ok((token, rows))
    }

    pub async fn ack_outbox_published(&self, token: Uuid, ids: &[OutboxId]) -> ControlResult<()> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        <R as ControlRepo>::ack_outbox_published(&self.repo, &mut tx, ids, token).await?;
        tx.commit().await?;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Permission gate
    // -------------------------------------------------------------------------

    async fn require(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        ctx: &RequestContext,
        channel_id: Option<ChannelId>,
        target_user_id: Option<UserId>,
        capability: Capability,
    ) -> ControlResult<()> {
        let req = PermissionRequest {
            server_id: ctx.server_id,
            user_id: ctx.user_id,
            is_admin: ctx.is_admin,
            capability,
            channel_id,
            target_user_id,
        };

        match <R as ControlRepo>::decide_permission(&self.repo, tx, &req).await? {
            PermissionDecision::Allow => Ok(()),
            PermissionDecision::Deny => Err(ControlError::PermissionDenied("permission denied")),
        }
    }
}
