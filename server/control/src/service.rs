use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use crate::{
    error::{ControlError, ControlResult},
    ids::{ChannelId, MessageId, OutboxId, ServerId, UserId},
    model::{AuditEntry, Channel, ChannelCreate, ChatMessage, JoinChannel, Member, OutboxEvent, OutboxEventRow, PermissionRequest, SendMessage},
    perms::{Capability, Decision},
    repo::{ControlRepo, PgControlRepo},
};

#[derive(Clone, Copy, Debug)]
pub struct RequestContext {
    pub server_id: ServerId,
    pub user_id: UserId,
    pub is_admin: bool,
}

#[derive(Clone)]
pub struct ControlService {
    repo: PgControlRepo,
}

impl ControlService {
    pub fn new(repo: PgControlRepo) -> Self {
        Self { repo }
    }

    pub fn repo(&self) -> &PgControlRepo {
        &self.repo
    }

    // -------------------------
    // Channels
    // -------------------------

    pub async fn create_channel(&self, ctx: &RequestContext, req: ChannelCreate) -> ControlResult<Channel> {
        let mut tx = self.repo.tx().await?;

        self.require_allowed(&mut tx, ctx, Capability::CreateChannel, None, None)
            .await?;

        let now = Utc::now();
        let ch = Channel {
            id: ChannelId(Uuid::new_v4()),
            server_id: ctx.server_id,
            name: req.name,
            parent_id: req.parent_id,
            max_members: req.max_members,
            max_talkers: req.max_talkers.or(Some(4)),
            created_at: now,
            updated_at: now,
        };

        self.repo.create_channel(&mut tx, &ch).await?;

        self.repo
            .insert_outbox(&mut tx, &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "channel.created".to_string(),
                payload_json: json!({
                    "channel_id": ch.id.0.to_string(),
                    "name": ch.name,
                    "parent_id": ch.parent_id.map(|p| p.0.to_string()),
                    "max_members": ch.max_members,
                    "max_talkers": ch.max_talkers,
                }),
            })
            .await?;

        self.repo
            .insert_audit(
                &mut tx,
                &AuditEntry::new(
                    ctx.server_id,
                    Some(ctx.user_id),
                    "channel.create",
                    "channel",
                    ch.id.0.to_string(),
                    json!({ "name": ch.name }),
                ),
            )
            .await?;

        tx.commit().await?;
        Ok(ch)
    }

    pub async fn get_channel(&self, ctx: &RequestContext, id: ChannelId) -> ControlResult<Channel> {
        let mut tx = self.repo.tx().await?;
        let ch = self
            .repo
            .get_channel(&mut tx, ctx.server_id, id)
            .await?
            .ok_or_else(|| not_found("channel not found"))?;
        tx.commit().await?;
        Ok(ch)
    }

    // -------------------------
    // Membership
    // -------------------------

    pub async fn join_channel(&self, ctx: &RequestContext, req: JoinChannel) -> ControlResult<Vec<Member>> {
        let mut tx = self.repo.tx().await?;

        self.require_allowed(&mut tx, ctx, Capability::JoinChannel, Some(req.channel_id), None)
            .await?;

        // Ensure channel exists
        let _ch = self
            .repo
            .get_channel(&mut tx, ctx.server_id, req.channel_id)
            .await?
            .ok_or_else(|| not_found("channel not found"))?;

        let member = Member {
            channel_id: req.channel_id,
            user_id: ctx.user_id,
            display_name: req.display_name,
            muted: false,
            deafened: false,
            joined_at: Utc::now(),
        };

        // NOTE: repo signature is (tx, server_id, &Member)
        self.repo.upsert_member(&mut tx, ctx.server_id, &member).await?;

        self.repo
            .insert_outbox(&mut tx, &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "presence.member_joined".to_string(),
                payload_json: json!({
                    "channel_id": member.channel_id.0.to_string(),
                    "user_id": member.user_id.0.to_string(),
                    "display_name": member.display_name,
                    "muted": member.muted,
                    "deafened": member.deafened
                }),
            })
            .await?;

        self.repo
            .insert_audit(
                &mut tx,
                &AuditEntry::new(
                    ctx.server_id,
                    Some(ctx.user_id),
                    "member.join",
                    "channel",
                    member.channel_id.0.to_string(),
                    json!({ "user_id": ctx.user_id.0.to_string() }),
                ),
            )
            .await?;

        let members = self.repo.list_members(&mut tx, ctx.server_id, req.channel_id).await?;
        tx.commit().await?;
        Ok(members)
    }

    pub async fn leave_channel(&self, ctx: &RequestContext, channel_id: ChannelId) -> ControlResult<()> {
        let mut tx = self.repo.tx().await?;

        // Must exist as member
        let _ = self
            .repo
            .get_member(&mut tx, ctx.server_id, channel_id, ctx.user_id)
            .await?
            .ok_or_else(|| not_found("not a channel member"))?;

        self.repo
            .delete_member(&mut tx, ctx.server_id, channel_id, ctx.user_id)
            .await?;

        self.repo
            .insert_outbox(&mut tx, &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "presence.member_left".to_string(),
                payload_json: json!({
                    "channel_id": channel_id.0.to_string(),
                    "user_id": ctx.user_id.0.to_string()
                }),
            })
            .await?;

        self.repo
            .insert_audit(
                &mut tx,
                &AuditEntry::new(
                    ctx.server_id,
                    Some(ctx.user_id),
                    "member.leave",
                    "channel",
                    channel_id.0.to_string(),
                    json!({ "user_id": ctx.user_id.0.to_string() }),
                ),
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
        let mut tx = self.repo.tx().await?;

        // We reference Capability::MuteVoice here; if your perms enum names it differently,
        // rename this variant in perms.rs or update this callsite.
        self.require_allowed(&mut tx, ctx, Capability::MuteVoice, Some(channel_id), Some(target_user))
            .await?;

        let mut m = self
            .repo
            .get_member(&mut tx, ctx.server_id, channel_id, target_user)
            .await?
            .ok_or_else(|| not_found("target not a member"))?;

        m.muted = muted;
        self.repo.upsert_member(&mut tx, ctx.server_id, &m).await?;

        self.repo
            .insert_outbox(&mut tx, &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "presence.voice_state_changed".to_string(),
                payload_json: json!({
                    "channel_id": channel_id.0.to_string(),
                    "user_id": target_user.0.to_string(),
                    "muted": muted,
                    "deafened": m.deafened
                }),
            })
            .await?;

        self.repo
            .insert_outbox(&mut tx, &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "moderation.user_muted".to_string(),
                payload_json: json!({
                    "channel_id": channel_id.0.to_string(),
                    "target_user_id": target_user.0.to_string(),
                    "actor_user_id": ctx.user_id.0.to_string(),
                    "muted": muted,
                    "reason": reason
                }),
            })
            .await?;

        self.repo
            .insert_audit(
                &mut tx,
                &AuditEntry::new(
                    ctx.server_id,
                    Some(ctx.user_id),
                    if muted { "moderation.mute" } else { "moderation.unmute" },
                    "member",
                    target_user.0.to_string(),
                    json!({ "channel_id": channel_id.0.to_string(), "muted": muted }),
                ),
            )
            .await?;

        tx.commit().await?;
        Ok(m)
    }

    // -------------------------
    // Chat
    // -------------------------

    pub async fn send_message(&self, ctx: &RequestContext, req: SendMessage) -> ControlResult<ChatMessage> {
        let mut tx = self.repo.tx().await?;

        self.require_allowed(&mut tx, ctx, Capability::SendMessage, Some(req.channel_id), None)
            .await?;

        let text = req.text.trim();
        if text.is_empty() {
            return Err(invalid("message text empty"));
        }
        if text.len() > 2000 {
            return Err(invalid("message too long"));
        }

        // Must be channel member
        let _ = self
            .repo
            .get_member(&mut tx, ctx.server_id, req.channel_id, ctx.user_id)
            .await?
            .ok_or_else(|| forbidden("not a member of channel"))?;

        let msg = ChatMessage {
            id: MessageId(Uuid::new_v4()),
            server_id: ctx.server_id,
            channel_id: req.channel_id,
            author_user_id: ctx.user_id,
            text: text.to_string(),
            attachments: req.attachments.unwrap_or_else(|| json!([])),
            created_at: Utc::now(),
        };

        self.repo.insert_chat_message(&mut tx, &msg).await?;

        self.repo
            .insert_outbox(&mut tx, &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "chat.message_posted".to_string(),
                payload_json: json!({
                    "message_id": msg.id.0.to_string(),
                    "channel_id": msg.channel_id.0.to_string(),
                    "author_user_id": msg.author_user_id.0.to_string(),
                    "text": msg.text,
                    "attachments": msg.attachments,
                    "created_at": msg.created_at,
                }),
            })
            .await?;

        self.repo
            .insert_audit(
                &mut tx,
                &AuditEntry::new(
                    ctx.server_id,
                    Some(ctx.user_id),
                    "chat.send",
                    "channel",
                    msg.channel_id.0.to_string(),
                    json!({ "message_id": msg.id.0.to_string() }),
                ),
            )
            .await?;

        tx.commit().await?;
        Ok(msg)
    }

    // -------------------------
    // Outbox (used by gateway poller)
    // -------------------------

    pub async fn claim_outbox_batch(&self, server: ServerId, limit: i64) -> ControlResult<(Uuid, Vec<OutboxEventRow>)> {
        let mut tx = self.repo.tx().await?;
        let token = Uuid::new_v4();
        let rows = self.repo.claim_outbox_batch(&mut tx, server, token, limit).await?;
        tx.commit().await?;
        Ok((token, rows))
    }

    pub async fn ack_outbox_published(&self, token: Uuid, ids: &[OutboxId]) -> ControlResult<()> {
        let mut tx = self.repo.tx().await?;
        self.repo.ack_outbox_published(&mut tx, ids, token).await?;
        tx.commit().await?;
        Ok(())
    }

    // -------------------------
    // Internal
    // -------------------------

    async fn require_allowed(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        ctx: &RequestContext,
        capability: Capability,
        channel_id: Option<ChannelId>,
        target_user_id: Option<UserId>,
    ) -> ControlResult<()> {
        let req = PermissionRequest {
            server_id: ctx.server_id,
            user_id: ctx.user_id,
            is_admin: ctx.is_admin,
            capability,
            channel_id,
            target_user_id,
        };

        match self.repo.decide_permission(tx, &req).await? {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(forbidden("permission denied")),
        }
    }
}

// --- Error helpers (adjust here if your ControlError API differs) ---

fn forbidden(msg: &str) -> ControlError {
    ControlError::forbidden(msg)
}

fn not_found(msg: &str) -> ControlError {
    ControlError::not_found(msg)
}

fn invalid(msg: &str) -> ControlError {
    ControlError::invalid_argument(msg)
}
