use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value as Json;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::{
    error::{ControlError, ControlResult},
    ids::{ChannelId, MessageId, OutboxId, ServerId, UserId},
    model::{
        AuditEntry, Channel, ChannelListItem, ChatMessage, Member, OutboxEvent, OutboxEventRow,
        PermissionRequest,
    },
    perms::{Capability, Decision},
};

/// Repo trait (static dispatch preferred)
#[async_trait]
pub trait ControlRepo: Send + Sync {
    // Channels
    async fn create_channel(&self, tx: &mut Transaction<'_, Postgres>, ch: &Channel) -> ControlResult<()>;
    async fn get_channel(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: ChannelId) -> ControlResult<Option<Channel>>;
    async fn list_channels(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId) -> ControlResult<Vec<ChannelListItem>>;

    // Members (NOTE: Member has NO server_id field in your codebase)
    async fn upsert_member(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, m: &Member) -> ControlResult<()>;
    async fn delete_member(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId, user: UserId) -> ControlResult<()>;
    async fn get_member(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId, user: UserId) -> ControlResult<Option<Member>>;
    async fn list_members(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId) -> ControlResult<Vec<Member>>;

    // Permissions
    async fn decide_permission(&self, tx: &mut Transaction<'_, Postgres>, req: &PermissionRequest) -> ControlResult<Decision>;

    // Chat (NOTE: ChatMessage uses author_user_id, not user_id)
    async fn insert_chat_message(&self, tx: &mut Transaction<'_, Postgres>, msg: &ChatMessage) -> ControlResult<()>;
    async fn get_chat_message(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: MessageId) -> ControlResult<Option<ChatMessage>>;

    // Outbox
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

    // Audit
    async fn insert_audit(&self, tx: &mut Transaction<'_, Postgres>, entry: &AuditEntry) -> ControlResult<()>;
}

#[derive(Clone)]
pub struct PgControlRepo {
    pool: PgPool,
}

impl PgControlRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Begin a transaction (used by ControlService).
    pub async fn tx(&self) -> ControlResult<Transaction<'_, Postgres>> {
        Ok(self.pool.begin().await?)
    }
}

#[async_trait]
impl ControlRepo for PgControlRepo {
    // -------------------------
    // Channels
    // -------------------------

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
        .bind(ch.max_members) // keep as Option<i32> if that’s what your Channel uses
        .bind(ch.max_talkers) // FIX: Option<i32> (per your compiler)
        .execute(&mut **tx)
        .await
        .context("insert channels")?;
        Ok(())
    }

    async fn get_channel(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: ChannelId) -> ControlResult<Option<Channel>> {
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
        .context("get channel")?;

        Ok(row.map(|r| Channel {
            id: ChannelId(r.get::<Uuid, _>("id")),
            server_id: ServerId(r.get::<Uuid, _>("server_id")),
            name: r.get::<String, _>("name"),
            parent_id: r.get::<Option<Uuid>, _>("parent_id").map(ChannelId),
            max_members: r.get::<Option<i32>, _>("max_members"),
            max_talkers: r.get::<Option<i32>, _>("max_talkers"), // FIX: no u32 mapping
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
                max_members: r.get::<Option<i32>, _>("max_members"),
                max_talkers: r.get::<Option<i32>, _>("max_talkers"),
            });
        }
        Ok(out)
    }

    // -------------------------
    // Members (no server_id field on Member)
    // -------------------------

    async fn upsert_member(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, m: &Member) -> ControlResult<()> {
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
        .bind(server.0)
        .bind(m.channel_id.0)
        .bind(m.user_id.0)
        .bind(&m.display_name)
        .bind(m.muted)
        .bind(m.deafened)
        .execute(&mut **tx)
        .await
        .context("upsert member")?;
        Ok(())
    }

    async fn delete_member(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId, user: UserId) -> ControlResult<()> {
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
            SELECT channel_id, user_id, display_name, muted, deafened, joined_at
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
            channel_id: ChannelId(r.get::<Uuid, _>("channel_id")),
            user_id: UserId(r.get::<Uuid, _>("user_id")),
            display_name: r.get::<String, _>("display_name"),
            muted: r.get::<bool, _>("muted"),
            deafened: r.get::<bool, _>("deafened"),
            joined_at: r.get::<DateTime<Utc>, _>("joined_at"),
        }))
    }

    async fn list_members(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId) -> ControlResult<Vec<Member>> {
        let rows = sqlx::query(
            r#"
            SELECT channel_id, user_id, display_name, muted, deafened, joined_at
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
                channel_id: ChannelId(r.get::<Uuid, _>("channel_id")),
                user_id: UserId(r.get::<Uuid, _>("user_id")),
                display_name: r.get::<String, _>("display_name"),
                muted: r.get::<bool, _>("muted"),
                deafened: r.get::<bool, _>("deafened"),
                joined_at: r.get::<DateTime<Utc>, _>("joined_at"),
            });
        }
        Ok(out)
    }

    // -------------------------
    // Permissions
    // -------------------------

    async fn decide_permission(&self, _tx: &mut Transaction<'_, Postgres>, req: &PermissionRequest) -> ControlResult<Decision> {
        if req.is_admin {
            return Ok(Decision::Allow);
        }

        // FIX: don't reference non-existent Capability variants.
        // Allow only safe baseline caps. Deny everything else by default.
        match req.capability {
            Capability::JoinChannel => Ok(Decision::Allow),
            Capability::SendMessage => Ok(Decision::Allow),
            Capability::CreateChannel => Ok(Decision::Deny),
            _ => Ok(Decision::Deny),
        }
    }

    // -------------------------
    // Chat (author_user_id)
    // -------------------------

    async fn insert_chat_message(&self, tx: &mut Transaction<'_, Postgres>, msg: &ChatMessage) -> ControlResult<()> {
        // NOTE: This matches the fields your compiler revealed:
        // id, server_id, channel_id, author_user_id, text (+ attachments, created_at... managed by DB)
        //
        // If your schema requires attachments not null, update this insert accordingly.
        sqlx::query(
            r#"
            INSERT INTO chat_messages (id, server_id, channel_id, author_user_id, text, created_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            "#,
        )
        .bind(msg.id.0)
        .bind(msg.server_id.0)
        .bind(msg.channel_id.0)
        .bind(msg.author_user_id.0) // FIX
        .bind(&msg.text)
        .execute(&mut **tx)
        .await
        .context("insert chat_messages")?;
        Ok(())
    }

    async fn get_chat_message(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: MessageId) -> ControlResult<Option<ChatMessage>> {
        let row = sqlx::query(
            r#"
            SELECT id, server_id, channel_id, author_user_id, text, created_at
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
            author_user_id: UserId(r.get::<Uuid, _>("author_user_id")), // FIX
            text: r.get::<String, _>("text"),
            // If your ChatMessage struct has more required fields (attachments, etc.),
            // you must fill them here. If they're Option/Vec with defaults, this compiles.
            ..Default::default()
        }))
    }

    // -------------------------
    // Outbox
    // -------------------------

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
        .context("insert outbox")?;
        Ok(())
    }

    async fn claim_outbox_batch(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        claim_token: Uuid,
        limit: i64,
    ) -> ControlResult<Vec<OutboxEventRow>> {
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
        .context("claim outbox")?;

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

    async fn ack_outbox_published(&self, tx: &mut Transaction<'_, Postgres>, ids: &[OutboxId], claim_token: Uuid) -> ControlResult<()> {
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
        .context("ack outbox")?;

        Ok(())
    }

    // -------------------------
    // Audit
    // -------------------------

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
        .context("insert audit")?;
        Ok(())
    }
}
