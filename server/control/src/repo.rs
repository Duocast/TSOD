use anyhow::Context;
use async_trait::async_trait;
use serde_json::Value as Json;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{
    error::{ControlError, ControlResult},
    ids::{ChannelId, MessageId, OutboxId, ServerId, UserId},
    model::{
        AuditEntry, Channel, ChannelCreate, ChannelListItem, ChatMessage, JoinChannel,
        Member, MemberUpdate, OutboxEvent, OutboxEventRow, SendMessage,
    },
    perms::{Capability, Decision, PermissionRequest},
};

/// Repository trait used by ControlService.
///
/// NOTE: This trait is used with *static dispatch* (generic R: ControlRepo),
/// but we also provide PgControlRepo as the default implementation.
///
/// Avoid `dyn ControlRepo` if possible; async traits + trait objects are painful.
/// Where you need polymorphism, prefer `R: ControlRepo`.
#[async_trait]
pub trait ControlRepo: Send + Sync {
    // ---- Channel ----
    async fn create_channel(&self, tx: &mut Transaction<'_, Postgres>, ch: &Channel) -> ControlResult<()>;
    async fn get_channel(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: ChannelId) -> ControlResult<Option<Channel>>;
    async fn list_channels(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId) -> ControlResult<Vec<ChannelListItem>>;

    // ---- Membership ----
    async fn upsert_member(&self, tx: &mut Transaction<'_, Postgres>, m: &Member) -> ControlResult<()>;
    async fn delete_member(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId, user: UserId) -> ControlResult<()>;
    async fn get_member(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId, user: UserId) -> ControlResult<Option<Member>>;
    async fn list_members(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId) -> ControlResult<Vec<Member>>;

    // ---- Permissions ----
    async fn decide_permission(&self, tx: &mut Transaction<'_, Postgres>, req: &PermissionRequest) -> ControlResult<Decision>;

    // ---- Chat ----
    async fn insert_chat_message(&self, tx: &mut Transaction<'_, Postgres>, msg: &ChatMessage) -> ControlResult<()>;
    async fn get_chat_message(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: MessageId) -> ControlResult<Option<ChatMessage>>;

    // ---- Outbox ----
    async fn insert_outbox(&self, tx: &mut Transaction<'_, Postgres>, ev: &OutboxEvent) -> ControlResult<()>;
    async fn claim_outbox_batch(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        claim_token: Uuid,
        limit: i64,
    ) -> ControlResult<Vec<OutboxEventRow>>;
    async fn ack_outbox_published(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ids: &[OutboxId],
        claim_token: Uuid,
    ) -> ControlResult<()>;

    // ---- Audit ----
    async fn insert_audit(&self, tx: &mut Transaction<'_, Postgres>, entry: &AuditEntry) -> ControlResult<()>;
}

/// Postgres implementation.
#[derive(Clone)]
pub struct PgControlRepo {
    pool: PgPool,
}

impl PgControlRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Begin a transaction (used by ControlService).
    pub async fn tx(&self) -> ControlResult<Transaction<'_, Postgres>> {
        Ok(self.pool.begin().await?)
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl ControlRepo for PgControlRepo {
    // ---- Channel ----

    async fn create_channel(&self, tx: &mut Transaction<'_, Postgres>, ch: &Channel) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO channels (id, server_id, name, parent_id, max_members, max_talkers, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, NOW())
            "#,
        )
        .bind(ch.id.0)
        .bind(ch.server_id.0)
        .bind(&ch.name)
        .bind(ch.parent_id.map(|p| p.0))
        .bind(ch.max_members.map(|v| v as i32))
        .bind(ch.max_talkers.map(|v| v as i32))
        .execute(&mut **tx)
        .await
        .context("insert channels")?;
        Ok(())
    }

    async fn get_channel(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: ChannelId,
    ) -> ControlResult<Option<Channel>> {
        let row = sqlx::query(
            r#"
            SELECT id, server_id, name, parent_id, max_members, max_talkers
            FROM channels
            WHERE server_id = $1 AND id = $2
            "#,
        )
        .bind(server.0)
        .bind(id.0)
        .fetch_optional(&mut **tx)
        .await
        .context("select channels")?;

        Ok(row.map(|r| Channel {
            id: ChannelId(r.get::<Uuid, _>("id")),
            server_id: ServerId(r.get::<Uuid, _>("server_id")),
            name: r.get::<String, _>("name"),
            parent_id: r.get::<Option<Uuid>, _>("parent_id").map(ChannelId),
            max_members: r.get::<Option<i32>, _>("max_members").map(|v| v as u32),
            max_talkers: r.get::<Option<i32>, _>("max_talkers").map(|v| v as u32),
        }))
    }

    async fn list_channels(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId) -> ControlResult<Vec<ChannelListItem>> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, parent_id, max_members, max_talkers
            FROM channels
            WHERE server_id = $1
            ORDER BY name ASC
            "#,
        )
        .bind(server.0)
        .fetch_all(&mut **tx)
        .await
        .context("list channels")?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(ChannelListItem {
                id: ChannelId(r.get::<Uuid, _>("id")),
                name: r.get::<String, _>("name"),
                parent_id: r.get::<Option<Uuid>, _>("parent_id").map(ChannelId),
                max_members: r.get::<Option<i32>, _>("max_members").map(|v| v as u32),
                max_talkers: r.get::<Option<i32>, _>("max_talkers").map(|v| v as u32),
            });
        }
        Ok(out)
    }

    // ---- Membership ----

    async fn upsert_member(&self, tx: &mut Transaction<'_, Postgres>, m: &Member) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO members (server_id, channel_id, user_id, display_name, muted, deafened, joined_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())
            ON CONFLICT (server_id, channel_id, user_id)
            DO UPDATE SET
                display_name = EXCLUDED.display_name,
                muted = EXCLUDED.muted,
                deafened = EXCLUDED.deafened,
                updated_at = NOW()
            "#,
        )
        .bind(m.server_id.0)
        .bind(m.channel_id.0)
        .bind(m.user_id.0)
        .bind(&m.display_name)
        .bind(m.muted)
        .bind(m.deafened)
        .execute(&mut **tx)
        .await
        .context("upsert members")?;
        Ok(())
    }

    async fn delete_member(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        channel: ChannelId,
        user: UserId,
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            DELETE FROM members
            WHERE server_id = $1 AND channel_id = $2 AND user_id = $3
            "#,
        )
        .bind(server.0)
        .bind(channel.0)
        .bind(user.0)
        .execute(&mut **tx)
        .await
        .context("delete member")?;
        Ok(())
    }

    async fn get_member(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        channel: ChannelId,
        user: UserId,
    ) -> ControlResult<Option<Member>> {
        let row = sqlx::query(
            r#"
            SELECT server_id, channel_id, user_id, display_name, muted, deafened
            FROM members
            WHERE server_id = $1 AND channel_id = $2 AND user_id = $3
            "#,
        )
        .bind(server.0)
        .bind(channel.0)
        .bind(user.0)
        .fetch_optional(&mut **tx)
        .await
        .context("get member")?;

        Ok(row.map(|r| Member {
            server_id: ServerId(r.get::<Uuid, _>("server_id")),
            channel_id: ChannelId(r.get::<Uuid, _>("channel_id")),
            user_id: UserId(r.get::<Uuid, _>("user_id")),
            display_name: r.get::<String, _>("display_name"),
            muted: r.get::<bool, _>("muted"),
            deafened: r.get::<bool, _>("deafened"),
        }))
    }

    async fn list_members(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        channel: ChannelId,
    ) -> ControlResult<Vec<Member>> {
        let rows = sqlx::query(
            r#"
            SELECT server_id, channel_id, user_id, display_name, muted, deafened
            FROM members
            WHERE server_id = $1 AND channel_id = $2
            ORDER BY joined_at ASC
            "#,
        )
        .bind(server.0)
        .bind(channel.0)
        .fetch_all(&mut **tx)
        .await
        .context("list members")?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(Member {
                server_id: ServerId(r.get::<Uuid, _>("server_id")),
                channel_id: ChannelId(r.get::<Uuid, _>("channel_id")),
                user_id: UserId(r.get::<Uuid, _>("user_id")),
                display_name: r.get::<String, _>("display_name"),
                muted: r.get::<bool, _>("muted"),
                deafened: r.get::<bool, _>("deafened"),
            });
        }
        Ok(out)
    }

    // ---- Permissions ----
    //
    // This is intentionally minimal: admin bypass; otherwise allow basic capabilities.
    // You can harden this later (roles, ACL tables, etc.).
    async fn decide_permission(&self, _tx: &mut Transaction<'_, Postgres>, req: &PermissionRequest) -> ControlResult<Decision> {
        if req.is_admin {
            return Ok(Decision::Allow);
        }

        // Simple policy: allow join + send message by default; restrict moderation.
        match req.capability {
            Capability::JoinChannel => Ok(Decision::Allow),
            Capability::SendMessage => Ok(Decision::Allow),
            Capability::MuteMember => Ok(Decision::Deny),
            Capability::CreateChannel => Ok(Decision::Deny),
        }
    }

    // ---- Chat ----

    async fn insert_chat_message(&self, tx: &mut Transaction<'_, Postgres>, msg: &ChatMessage) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO chat_messages (id, server_id, channel_id, user_id, text, created_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            "#,
        )
        .bind(msg.id.0)
        .bind(msg.server_id.0)
        .bind(msg.channel_id.0)
        .bind(msg.user_id.0)
        .bind(&msg.text)
        .execute(&mut **tx)
        .await
        .context("insert chat_messages")?;
        Ok(())
    }

    async fn get_chat_message(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: MessageId,
    ) -> ControlResult<Option<ChatMessage>> {
        let row = sqlx::query(
            r#"
            SELECT id, server_id, channel_id, user_id, text
            FROM chat_messages
            WHERE server_id = $1 AND id = $2
            "#,
        )
        .bind(server.0)
        .bind(id.0)
        .fetch_optional(&mut **tx)
        .await
        .context("get chat message")?;

        Ok(row.map(|r| ChatMessage {
            id: MessageId(r.get::<Uuid, _>("id")),
            server_id: ServerId(r.get::<Uuid, _>("server_id")),
            channel_id: ChannelId(r.get::<Uuid, _>("channel_id")),
            user_id: UserId(r.get::<Uuid, _>("user_id")),
            text: r.get::<String, _>("text"),
        }))
    }

    // ---- Outbox ----

    async fn insert_outbox(&self, tx: &mut Transaction<'_, Postgres>, ev: &OutboxEvent) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO outbox_events (id, server_id, topic, payload_json, created_at)
            VALUES ($1, $2, $3, $4, NOW())
            "#,
        )
        .bind(ev.id.0)
        .bind(ev.server_id.0)
        .bind(&ev.topic)
        .bind(&ev.payload_json)
        .execute(&mut **tx)
        .await
        .context("insert outbox_events")?;
        Ok(())
    }

    async fn claim_outbox_batch(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        claim_token: Uuid,
        limit: i64,
    ) -> ControlResult<Vec<OutboxEventRow>> {
        // Requires migration adding claim_token/claimed_at columns.
        //
        // Claim rows by setting claim_token where not already published and not already claimed.
        // Use SKIP LOCKED so multiple gateways can poll concurrently.
        let rows = sqlx::query(
            r#"
            WITH cte AS (
              SELECT id
              FROM outbox_events
              WHERE server_id = $1
                AND published_at IS NULL
                AND (claim_token IS NULL OR claimed_at < NOW() - INTERVAL '30 seconds')
              ORDER BY created_at ASC
              FOR UPDATE SKIP LOCKED
              LIMIT $2
            )
            UPDATE outbox_events o
            SET claim_token = $3, claimed_at = NOW()
            FROM cte
            WHERE o.id = cte.id
            RETURNING o.id, o.server_id, o.topic, o.payload_json
            "#,
        )
        .bind(server.0)
        .bind(limit)
        .bind(claim_token)
        .fetch_all(&mut **tx)
        .await
        .context("claim outbox batch")?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(OutboxEventRow {
                id: OutboxId(r.get::<Uuid, _>("id")),
                server_id: ServerId(r.get::<Uuid, _>("server_id")),
                topic: r.get::<String, _>("topic"),
                payload_json: r.get::<Json, _>("payload_json"),
            });
        }
        Ok(out)
    }

    async fn ack_outbox_published(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ids: &[OutboxId],
        claim_token: Uuid,
    ) -> ControlResult<()> {
        // Mark published only if claim_token matches (prevents double publish).
        let uuids: Vec<Uuid> = ids.iter().map(|id| id.0).collect();

        sqlx::query(
            r#"
            UPDATE outbox_events
            SET published_at = NOW()
            WHERE id = ANY($1)
              AND claim_token = $2
            "#,
        )
        .bind(&uuids)
        .bind(claim_token)
        .execute(&mut **tx)
        .await
        .context("ack outbox published")?;

        Ok(())
    }

    // ---- Audit ----

    async fn insert_audit(&self, tx: &mut Transaction<'_, Postgres>, entry: &AuditEntry) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO audit_log (id, server_id, actor_user_id, action, target_type, target_id, context_json, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())
            "#,
        )
        .bind(entry.id.0)
        .bind(entry.server_id.0)
        .bind(entry.actor_user_id.map(|u| u.0))
        .bind(&entry.action)
        .bind(&entry.target_type)
        .bind(&entry.target_id)
        .bind(&entry.context_json)
        .execute(&mut **tx)
        .await
        .context("insert audit_log")?;
        Ok(())
    }
}
