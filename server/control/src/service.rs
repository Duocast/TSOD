use chrono::Utc;
use serde_json::json;
use tracing::debug;
use uuid::Uuid;

use crate::{
    errors::{ControlError, ControlResult},
    ids::{ChannelId, MessageId, OutboxId, ServerId, UserId},
    model::{
        AuditEntry, Channel, ChannelCreate, ChatMessage, JoinChannel, Member, OutboxEvent,
        OutboxEventRow, PermAuditRow, PermChannelOverrideRecord, PermRoleRecord,
        PermUserSummaryRecord, PermissionRequest, SendMessage,
    },
    perms::{Capability, Decision},
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

    pub async fn create_channel(
        &self,
        ctx: &RequestContext,
        req: ChannelCreate,
    ) -> ControlResult<Channel> {
        let name = req.name.trim();
        if name.is_empty() {
            return Err(ControlError::InvalidArgument("channel name empty"));
        }
        if name.len() > 64 {
            return Err(ControlError::InvalidArgument("channel name too long"));
        }

        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, None, None, Capability::CreateChannel)
            .await?;

        let now = Utc::now();
        let bitrate_bps = req.bitrate_bps.clamp(8_000, 510_000);
        let opus_profile = match req.opus_profile {
            1 | 2 => req.opus_profile,
            _ => 1,
        };
        let ch = Channel {
            id: ChannelId(Uuid::new_v4()),
            server_id: ctx.server_id,
            name: name.to_string(),
            parent_id: req.parent_id,
            max_members: req.max_members,
            max_talkers: req.max_talkers,
            channel_type: req.channel_type,
            description: req.description,
            bitrate_bps,
            opus_profile,
            created_at: now,
            updated_at: now,
        };

        <R as ControlRepo>::create_channel(&self.repo, &mut tx, &ch).await?;

        // audit
        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                "channel.create",
                "channel",
                ch.id.0.to_string(),
                json!({
                    "name": ch.name,
                    "parent_id": ch.parent_id.map(|p| p.0),
                    "channel_type": ch.channel_type,
                    "description": ch.description,
                    "bitrate_bps": ch.bitrate_bps,
                    "opus_profile": ch.opus_profile,
                    "max_members": ch.max_members,
                }),
            ),
        )
        .await?;

        // outbox push
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "channel.created".to_string(),
                payload_json: json!({
                    "server_id": ctx.server_id.0,
                    "channel_id": ch.id.0,
                    "name": ch.name,
                    "parent_channel_id": ch.parent_id.map(|p| p.0),
                    "max_members": ch.max_members,
                    "max_talkers": ch.max_talkers,
                    "channel_type": ch.channel_type,
                    "description": ch.description,
                    "bitrate_bps": ch.bitrate_bps,
                    "opus_profile": ch.opus_profile,
                    "created_at": ch.created_at,
                    "updated_at": ch.updated_at,
                }),
            },
        )
        .await?;

        debug!(server_id=%ctx.server_id.0, channel_id=%ch.id.0, topic="channel.created", "produced outbox event");

        tx.commit().await?;
        debug!(server_id=%ctx.server_id.0, channel_id=%ch.id.0, "create_channel transaction committed");
        Ok(ch)
    }

    pub async fn get_channel(
        &self,
        ctx: &RequestContext,
        channel_id: ChannelId,
    ) -> ControlResult<Channel> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(
            &mut tx,
            ctx,
            Some(channel_id),
            None,
            Capability::JoinChannel,
        )
        .await?;

        let ch = <R as ControlRepo>::get_channel(&self.repo, &mut tx, ctx.server_id, channel_id)
            .await?
            .ok_or(ControlError::NotFound("channel"))?;
        tx.commit().await?;
        Ok(ch)
    }

    pub async fn rename_channel(
        &self,
        ctx: &RequestContext,
        channel_id: ChannelId,
        new_name: &str,
    ) -> ControlResult<Channel> {
        let name = new_name.trim();
        if name.is_empty() {
            return Err(ControlError::InvalidArgument("channel name empty"));
        }
        if name.len() > 64 {
            return Err(ControlError::InvalidArgument("channel name too long"));
        }

        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(
            &mut tx,
            ctx,
            Some(channel_id),
            None,
            Capability::CreateChannel,
        )
        .await?;

        let renamed = <R as ControlRepo>::rename_channel(
            &self.repo,
            &mut tx,
            ctx.server_id,
            channel_id,
            name,
        )
        .await?
        .ok_or(ControlError::NotFound("channel"))?;

        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                "channel.rename",
                "channel",
                renamed.id.0.to_string(),
                json!({ "new_name": renamed.name }),
            ),
        )
        .await?;

        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "channel.renamed".to_string(),
                payload_json: json!({
                    "server_id": ctx.server_id.0,
                    "channel_id": renamed.id.0,
                    "name": renamed.name,
                    "parent_channel_id": renamed.parent_id.map(|p| p.0),
                    "max_members": renamed.max_members,
                    "channel_type": renamed.channel_type,
                    "description": renamed.description,
                    "bitrate_bps": renamed.bitrate_bps,
                    "opus_profile": renamed.opus_profile,
                    "updated_at": renamed.updated_at,
                }),
            },
        )
        .await?;

        debug!(server_id=%ctx.server_id.0, channel_id=%renamed.id.0, topic="channel.renamed", "produced outbox event");
        tx.commit().await?;
        Ok(renamed)
    }

    pub async fn delete_channel(
        &self,
        ctx: &RequestContext,
        channel_id: ChannelId,
    ) -> ControlResult<Vec<ChannelId>> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(
            &mut tx,
            ctx,
            Some(channel_id),
            None,
            Capability::CreateChannel,
        )
        .await?;

        let descendants = <R as ControlRepo>::list_channel_descendants(
            &self.repo,
            &mut tx,
            ctx.server_id,
            channel_id,
        )
        .await?;
        if descendants.is_empty() {
            return Err(ControlError::NotFound("channel"));
        }

        let deleted =
            <R as ControlRepo>::delete_channel(&self.repo, &mut tx, ctx.server_id, channel_id)
                .await?;
        if !deleted {
            return Err(ControlError::NotFound("channel"));
        }

        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                "channel.delete",
                "channel",
                channel_id.0.to_string(),
                json!({ "cascade_count": descendants.len() }),
            ),
        )
        .await?;

        for deleted_channel_id in &descendants {
            <R as ControlRepo>::insert_outbox(
                &self.repo,
                &mut tx,
                &OutboxEvent {
                    id: OutboxId(Uuid::new_v4()),
                    server_id: ctx.server_id,
                    topic: "channel.deleted".to_string(),
                    payload_json: json!({
                        "server_id": ctx.server_id.0,
                        "channel_id": deleted_channel_id.0,
                        "updated_at": Utc::now(),
                    }),
                },
            )
            .await?;
            debug!(server_id=%ctx.server_id.0, channel_id=%deleted_channel_id.0, topic="channel.deleted", "produced outbox event");
        }

        tx.commit().await?;
        Ok(descendants)
    }

    // -------------------------------------------------------------------------
    // Membership
    // -------------------------------------------------------------------------

    pub async fn join_channel(
        &self,
        ctx: &RequestContext,
        req: JoinChannel,
    ) -> ControlResult<Vec<Member>> {
        let dn = req.display_name.trim();
        if dn.is_empty() {
            return Err(ControlError::InvalidArgument("display name empty"));
        }
        if dn.len() > 64 {
            return Err(ControlError::InvalidArgument("display name too long"));
        }

        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(
            &mut tx,
            ctx,
            Some(req.channel_id),
            None,
            Capability::JoinChannel,
        )
        .await?;

        // Ensure channel exists
        let ch =
            <R as ControlRepo>::get_channel(&self.repo, &mut tx, ctx.server_id, req.channel_id)
                .await?
                .ok_or(ControlError::NotFound("channel"))?;

        // Optional capacity check (if your repo supports count_members)
        if let Some(max) = ch.max_members {
            let cur = <R as ControlRepo>::count_members(
                &self.repo,
                &mut tx,
                ctx.server_id,
                req.channel_id,
            )
            .await?;
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

        debug!(
            server_id = %ctx.server_id.0,
            channel_id = %req.channel_id.0,
            user_id = %ctx.user_id.0,
            display_name = %m.display_name,
            "join_channel member upsert"
        );
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

        debug!(server_id=%ctx.server_id.0, channel_id=%req.channel_id.0, user_id=%ctx.user_id.0, topic="presence.member_joined", "produced outbox event");

        let members =
            <R as ControlRepo>::list_members(&self.repo, &mut tx, ctx.server_id, req.channel_id)
                .await?;
        for member in &members {
            debug!(
                server_id = %ctx.server_id.0,
                channel_id = %req.channel_id.0,
                member_user_id = %member.user_id.0,
                member_display_name = %member.display_name,
                "join_channel member listed"
            );
        }
        tx.commit().await?;
        debug!(server_id=%ctx.server_id.0, channel_id=%req.channel_id.0, user_id=%ctx.user_id.0, "join_channel transaction committed");
        Ok(members)
    }

    pub async fn leave_channel(
        &self,
        ctx: &RequestContext,
        channel_id: ChannelId,
    ) -> ControlResult<()> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;

        // Ensure member exists (and enforce permission)
        self.require(
            &mut tx,
            ctx,
            Some(channel_id),
            None,
            Capability::JoinChannel,
        )
        .await?;
        let _m = <R as ControlRepo>::get_member(
            &self.repo,
            &mut tx,
            ctx.server_id,
            channel_id,
            ctx.user_id,
        )
        .await?
        .ok_or(ControlError::NotFound("member"))?;

        <R as ControlRepo>::delete_member(
            &self.repo,
            &mut tx,
            ctx.server_id,
            channel_id,
            ctx.user_id,
        )
        .await?;

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

        let mut m = <R as ControlRepo>::get_member(
            &self.repo,
            &mut tx,
            ctx.server_id,
            channel_id,
            target_user,
        )
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
                if muted {
                    "moderation.mute"
                } else {
                    "moderation.unmute"
                },
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
                    "deafened": m.deafened,
                    "reason": reason
                }),
            },
        )
        .await?;

        tx.commit().await?;
        Ok(m)
    }

    pub async fn set_voice_deafen(
        &self,
        ctx: &RequestContext,
        channel_id: ChannelId,
        target_user: UserId,
        deafened: bool,
        reason: Option<String>,
    ) -> ControlResult<Member> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        let mut m = <R as ControlRepo>::get_member(
            &self.repo,
            &mut tx,
            ctx.server_id,
            channel_id,
            target_user,
        )
        .await?
        .ok_or(ControlError::NotFound("member"))?;
        m.deafened = deafened;
        <R as ControlRepo>::upsert_member(&self.repo, &mut tx, ctx.server_id, &m).await?;
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
                    "muted": m.muted,
                    "deafened": deafened
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
                topic: "moderation.user_deafened".to_string(),
                payload_json: json!({
                    "channel_id": channel_id.0,
                    "target_user_id": target_user.0,
                    "actor_user_id": ctx.user_id.0,
                    "deafened": deafened,
                    "reason": reason
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(m)
    }

    pub async fn kick_member(
        &self,
        ctx: &RequestContext,
        channel_id: ChannelId,
        target_user: UserId,
        reason: Option<String>,
    ) -> ControlResult<()> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        <R as ControlRepo>::delete_member(
            &self.repo,
            &mut tx,
            ctx.server_id,
            channel_id,
            target_user,
        )
        .await?;
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "moderation.user_kicked".to_string(),
                payload_json: json!({
                    "channel_id": channel_id.0,
                    "target_user_id": target_user.0,
                    "actor_user_id": ctx.user_id.0,
                    "reason": reason
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
                topic: "presence.member_left".to_string(),
                payload_json: json!({
                    "channel_id": channel_id.0,
                    "user_id": target_user.0
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn poke_user(
        &self,
        ctx: &RequestContext,
        target_user: UserId,
        from_display_name: &str,
        message: String,
    ) -> ControlResult<()> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "poke.received".to_string(),
                payload_json: json!({
                    "target_user_id": target_user.0,
                    "from_user_id": ctx.user_id.0,
                    "from_display_name": from_display_name,
                    "message": message,
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Chat
    // -------------------------------------------------------------------------

    pub async fn send_message(
        &self,
        ctx: &RequestContext,
        msg: SendMessage,
    ) -> ControlResult<ChatMessage> {
        let text = msg.text.trim();
        if text.is_empty() {
            return Err(ControlError::InvalidArgument("message text empty"));
        }
        if text.len() > 2000 {
            return Err(ControlError::InvalidArgument("message too long"));
        }

        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(
            &mut tx,
            ctx,
            Some(msg.channel_id),
            None,
            Capability::SendMessage,
        )
        .await?;

        // Ensure member exists
        let _m = <R as ControlRepo>::get_member(
            &self.repo,
            &mut tx,
            ctx.server_id,
            msg.channel_id,
            ctx.user_id,
        )
        .await?
        .ok_or(ControlError::NotFound("member"))?;

        let rec = ChatMessage {
            id: MessageId(Uuid::new_v4()),
            server_id: ctx.server_id,
            channel_id: msg.channel_id,
            author_user_id: ctx.user_id,
            text: text.to_string(),
            // FIX: Option<JsonValue> -> JsonValue
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
    // Admin permissions RPCs
    // -------------------------------------------------------------------------

    pub async fn perm_list_roles(
        &self,
        ctx: &RequestContext,
    ) -> ControlResult<Vec<PermRoleRecord>> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, None, None, Capability::ManageRoles)
            .await?;
        let roles = <R as ControlRepo>::perm_list_roles(&self.repo, &mut tx, ctx.server_id).await?;
        tx.commit().await?;
        Ok(roles)
    }

    pub async fn perm_list_users(
        &self,
        ctx: &RequestContext,
    ) -> ControlResult<(Vec<PermUserSummaryRecord>, i32, bool)> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, None, None, Capability::ManageRoles)
            .await?;
        let users = <R as ControlRepo>::perm_list_users(&self.repo, &mut tx, ctx.server_id).await?;
        let editor_highest_role_position = self.actor_max_role_position(&mut tx, ctx).await?;
        tx.commit().await?;
        Ok((users, editor_highest_role_position, ctx.is_admin))
    }

    pub async fn perm_upsert_role(
        &self,
        ctx: &RequestContext,
        role_id: Option<&str>,
        name: &str,
        color: i32,
        position: i32,
    ) -> ControlResult<PermRoleRecord> {
        if name.trim().is_empty() {
            return Err(ControlError::InvalidArgument("role name empty"));
        }
        if name.trim().eq_ignore_ascii_case("owner") && !ctx.is_admin {
            return Err(ControlError::PermissionDenied(
                "owner role editable only by server owner",
            ));
        }
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, None, None, Capability::ManageRoles)
            .await?;
        if let Some(existing_role_id) = role_id {
            let _ = self
                .require_manageable_role(&mut tx, ctx, existing_role_id)
                .await?;
        }
        let role = <R as ControlRepo>::perm_upsert_role(
            &self.repo,
            &mut tx,
            ctx.server_id,
            role_id,
            name.trim(),
            color,
            position,
        )
        .await?;
        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                "perm.role.upsert",
                "role",
                role.role_id.clone(),
                json!({"name": role.name, "position": role.role_position}),
            ),
        )
        .await?;
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "perm.audit.appended".to_string(),
                payload_json: json!({"action": "perm.role.upsert", "target_type": "role", "target_id": role.role_id}),
            },
        )
        .await?;
        <R as ControlRepo>::insert_outbox(&self.repo, &mut tx, &OutboxEvent { id: OutboxId(Uuid::new_v4()), server_id: ctx.server_id, topic: "perm.role.upserted".to_string(), payload_json: json!({"role_id": role.role_id, "name": role.name, "position": role.role_position}) }).await?;
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "perm.role.order_changed".to_string(),
                payload_json: json!({"role_ids": [role.role_id.clone()]}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(role)
    }

    pub async fn perm_delete_role(&self, ctx: &RequestContext, role_id: &str) -> ControlResult<()> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, None, None, Capability::ManageRoles)
            .await?;
        let _ = self.require_manageable_role(&mut tx, ctx, role_id).await?;
        let deleted =
            <R as ControlRepo>::perm_delete_role(&self.repo, &mut tx, ctx.server_id, role_id)
                .await?;
        if !deleted {
            return Err(ControlError::NotFound("role"));
        }
        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                "perm.role.delete",
                "role",
                role_id.to_string(),
                json!({}),
            ),
        )
        .await?;
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "perm.audit.appended".to_string(),
                payload_json: json!({"action": "perm.role.delete", "target_type": "role", "target_id": role_id}),
            },
        )
        .await?;
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "perm.role.deleted".to_string(),
                payload_json: json!({"role_id": role_id}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn perm_set_role_caps(
        &self,
        ctx: &RequestContext,
        role_id: &str,
        caps: &[(String, String)],
    ) -> ControlResult<()> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, None, None, Capability::ManageRoles)
            .await?;
        let _ = self.require_manageable_role(&mut tx, ctx, role_id).await?;
        <R as ControlRepo>::perm_replace_role_caps(&self.repo, &mut tx, role_id, caps).await?;
        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                "perm.role.caps",
                "role",
                role_id.to_string(),
                json!({"caps": caps}),
            ),
        )
        .await?;
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "perm.audit.appended".to_string(),
                payload_json: json!({"action": "perm.role.caps", "target_type": "role", "target_id": role_id}),
            },
        )
        .await?;
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "perm.role.caps_changed".to_string(),
                payload_json: json!({"role_id": role_id, "caps": caps}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn perm_assign_roles(
        &self,
        ctx: &RequestContext,
        user_id: UserId,
        role_ids: &[String],
    ) -> ControlResult<()> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, None, Some(user_id), Capability::ManageRoles)
            .await?;
        self.require_manageable_target_user(&mut tx, ctx, user_id)
            .await?;
        let actor_max = self.actor_max_role_position(&mut tx, ctx).await?;
        for rid in role_ids {
            let role = <R as ControlRepo>::perm_get_role(&self.repo, &mut tx, ctx.server_id, rid)
                .await?
                .ok_or(ControlError::NotFound("role"))?;
            if role.role_position >= actor_max {
                return Err(ControlError::PermissionDenied(
                    "cannot assign roles above or equal to your highest role",
                ));
            }
            if role.name.eq_ignore_ascii_case("owner") && !ctx.is_admin {
                return Err(ControlError::PermissionDenied(
                    "owner role editable only by server owner",
                ));
            }
        }
        <R as ControlRepo>::perm_replace_user_roles(
            &self.repo,
            &mut tx,
            ctx.server_id,
            user_id,
            role_ids,
        )
        .await?;
        <R as ControlRepo>::insert_audit(
            &self.repo,
            &mut tx,
            &AuditEntry::new(
                ctx.server_id,
                Some(ctx.user_id),
                "perm.user.roles",
                "user",
                user_id.0.to_string(),
                json!({"roles": role_ids}),
            ),
        )
        .await?;
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "perm.audit.appended".to_string(),
                payload_json: json!({"action": "perm.user.roles", "target_type": "user", "target_id": user_id.0}),
            },
        )
        .await?;
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "perm.user.roles_changed".to_string(),
                payload_json: json!({"user_id": user_id.0, "roles": role_ids}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn perm_list_channel_overrides(
        &self,
        ctx: &RequestContext,
        channel_id: ChannelId,
    ) -> ControlResult<Vec<PermChannelOverrideRecord>> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(
            &mut tx,
            ctx,
            Some(channel_id),
            None,
            Capability::ManageRoles,
        )
        .await?;
        self.require(
            &mut tx,
            ctx,
            Some(channel_id),
            None,
            Capability::ManageChannel,
        )
        .await?;
        let rows = <R as ControlRepo>::perm_list_channel_overrides(&self.repo, &mut tx, channel_id)
            .await?;
        tx.commit().await?;
        Ok(rows)
    }

    pub async fn perm_set_channel_override(
        &self,
        ctx: &RequestContext,
        rec: &PermChannelOverrideRecord,
    ) -> ControlResult<()> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(
            &mut tx,
            ctx,
            Some(rec.channel_id),
            rec.user_id,
            Capability::ManageRoles,
        )
        .await?;
        self.require(
            &mut tx,
            ctx,
            Some(rec.channel_id),
            rec.user_id,
            Capability::ManageChannel,
        )
        .await?;
        if let Some(target_user) = rec.user_id {
            self.require_manageable_target_user(&mut tx, ctx, target_user)
                .await?;
        }
        if let Some(ref role_id) = rec.role_id {
            let _ = self.require_manageable_role(&mut tx, ctx, role_id).await?;
        }
        <R as ControlRepo>::perm_set_channel_override(&self.repo, &mut tx, rec).await?;
        <R as ControlRepo>::insert_audit(&self.repo, &mut tx, &AuditEntry::new(ctx.server_id, Some(ctx.user_id), "perm.channel.override", "channel", rec.channel_id.0.to_string(), json!({"role_id": rec.role_id, "user_id": rec.user_id.map(|u| u.0), "cap": rec.cap, "effect": rec.effect}))).await?;
        <R as ControlRepo>::insert_outbox(
            &self.repo,
            &mut tx,
            &OutboxEvent {
                id: OutboxId(Uuid::new_v4()),
                server_id: ctx.server_id,
                topic: "perm.audit.appended".to_string(),
                payload_json: json!({"action": "perm.channel.override", "target_type": "channel", "target_id": rec.channel_id.0}),
            },
        )
        .await?;
        <R as ControlRepo>::insert_outbox(&self.repo, &mut tx, &OutboxEvent { id: OutboxId(Uuid::new_v4()), server_id: ctx.server_id, topic: "perm.channel.overrides_changed".to_string(), payload_json: json!({"channel_id": rec.channel_id.0, "role_id": rec.role_id, "user_id": rec.user_id.map(|u| u.0), "cap": rec.cap, "effect": rec.effect}) }).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn perm_audit_query(
        &self,
        ctx: &RequestContext,
        limit: i64,
    ) -> ControlResult<Vec<PermAuditRow>> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(&mut tx, ctx, None, None, Capability::ManageRoles)
            .await?;
        let rows = <R as ControlRepo>::perm_query_audit(
            &self.repo,
            &mut tx,
            ctx.server_id,
            limit.max(1).min(200),
        )
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    pub async fn perm_eval_effective(
        &self,
        ctx: &RequestContext,
        user_id: UserId,
        channel_id: Option<ChannelId>,
        caps: &[String],
    ) -> ControlResult<Vec<(String, bool)>> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        self.require(
            &mut tx,
            ctx,
            channel_id,
            Some(user_id),
            Capability::ManageRoles,
        )
        .await?;
        let mut out = Vec::with_capacity(caps.len());
        for cap in caps {
            let c = Capability::from_str(cap)
                .ok_or(ControlError::InvalidArgument("unknown capability"))?;
            let req = PermissionRequest {
                server_id: ctx.server_id,
                user_id,
                is_admin: false,
                capability: c,
                channel_id,
                target_user_id: None,
            };
            let allowed = matches!(
                <R as ControlRepo>::decide_permission(&self.repo, &mut tx, &req).await?,
                Decision::Allow
            );
            out.push((cap.clone(), allowed));
        }
        tx.commit().await?;
        Ok(out)
    }

    async fn actor_max_role_position(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        ctx: &RequestContext,
    ) -> ControlResult<i32> {
        <R as ControlRepo>::perm_actor_max_role_position(&self.repo, tx, ctx.server_id, ctx.user_id)
            .await
    }

    async fn require_manageable_role(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        ctx: &RequestContext,
        role_id: &str,
    ) -> ControlResult<PermRoleRecord> {
        let role = <R as ControlRepo>::perm_get_role(&self.repo, tx, ctx.server_id, role_id)
            .await?
            .ok_or(ControlError::NotFound("role"))?;
        if role.is_everyone {
            return Err(ControlError::FailedPrecondition(
                "@everyone role is immutable for this action",
            ));
        }
        if role.name.eq_ignore_ascii_case("owner") && !ctx.is_admin {
            return Err(ControlError::PermissionDenied(
                "owner role editable only by server owner",
            ));
        }
        let actor_max = self.actor_max_role_position(tx, ctx).await?;
        if role.role_position >= actor_max {
            return Err(ControlError::PermissionDenied(
                "can only manage roles below your highest role",
            ));
        }
        Ok(role)
    }

    async fn require_manageable_target_user(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        ctx: &RequestContext,
        target_user_id: UserId,
    ) -> ControlResult<()> {
        let actor_max = self.actor_max_role_position(tx, ctx).await?;
        let target_max = <R as ControlRepo>::perm_user_max_role_position(
            &self.repo,
            tx,
            ctx.server_id,
            target_user_id,
        )
        .await?;
        if target_max >= actor_max {
            return Err(ControlError::PermissionDenied(
                "target user is not below your highest role",
            ));
        }
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Outbox helpers (optional – if your gateway uses these)
    // -------------------------------------------------------------------------

    pub async fn claim_outbox_batch(
        &self,
        server: ServerId,
        limit: i64,
    ) -> ControlResult<(Uuid, Vec<OutboxEventRow>)> {
        let mut tx = <R as ControlRepo>::tx(&self.repo).await?;
        let token = Uuid::new_v4();
        let rows =
            <R as ControlRepo>::claim_outbox_batch(&self.repo, &mut tx, server, token, limit)
                .await?;
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
            Decision::Allow => Ok(()),
            Decision::Deny => Err(ControlError::PermissionDenied("permission denied")),
        }
    }
}
