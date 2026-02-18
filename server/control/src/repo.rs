use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value as Json;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::{
    errors::ControlResult,
    ids::{AuditId, ChannelId, MessageId, OutboxId, ServerId, UserId},
    model::{
        AuditEntry, Channel, ChannelListItem, ChatMessage, Member, OutboxEvent, OutboxEventRow,
        PermissionRequest,
    },
    perms::{Capability, PermissionDecision},
};

#[async_trait]
pub trait ControlRepo: Send + Sync {
    // Transactions
    async fn tx(&self) -> ControlResult<Transaction<'_, Postgres>>;

    // Channels
    async fn create_channel(&self, tx: &mut Transaction<'_, Postgres>, ch: &Channel) -> ControlResult<()>;
    async fn get_channel(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: ChannelId) -> ControlResult<Option<Channel>>;
    async fn list_channels(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId) -> ControlResult<Vec<ChannelListItem>>;

    // Members (Member has NO server_id)
    async fn upsert_member(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, m: &Member) -> ControlResult<()>;
    async fn delete_member(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId, user: UserId) -> ControlResult<()>;
    async fn get_member(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId, user: UserId) -> ControlResult<Option<Member>>;
    async fn list_members(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId) -> ControlResult<Vec<Member>>;
    async fn count_members(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId) -> ControlResult<i64>;

    // Permissions
    async fn decide_permission(&self, tx: &mut Transaction<'_, Postgres>, req: &PermissionRequest) -> ControlResult<PermissionDecision>;

    // Chat (ChatMessage uses author_user_id; no Default)
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
}

#[async_trait]
impl ControlRepo for PgControlRepo {
    async fn tx(&self) -> ControlResult<Transaction<'_, Postgres>> {
        Ok(self.pool.begin().await?)
    }

    // -------------------------
    // Channels
    // -------------------------

    async fn create_channel(&self, tx: &mut Transaction<'_, Postgres>, ch: &Channel) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO channels (id, server_id, name, parent_id, max_members, max_talkers, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())
            "#,
        )
        .bind(ch.id.0)
        .bind(ch.server_id.0)
        .bind(&ch.name)
        .bind(ch.parent_id.map(|p| p.0))
        .bind(ch.max_members)
        .bind(ch.max_talkers)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn get_channel(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: ChannelId) -> ControlResult<Option<Channel>> {
        let row = sqlx::query(
            r#"
            SELECT id, server_id, name, parent_id, max_members, max_talkers, created_at, updated_at
            FROM channels
            WHERE server_id = $1 AND id = $2
            "#,
        )
        .bind(server.0)
        .bind(id.0)
        .fetch_optional(&mut **tx)
        .await?;

        Ok(row.map(|r| Channel {
            id: ChannelId(r.get::<Uuid, _>("id")),
            server_id: ServerId(r.get::<Uuid, _>("server_id")),
            name: r.get::<String, _>("name"),
            parent_id: r.get::<Option<Uuid>, _>("parent_id").map(ChannelId),
            max_members: r.get::<Option<i32>, _>("max_members"),
            max_talkers: r.get::<Option<i32>, _>("max_talkers"),
            created_at: r.get::<DateTime<Utc>, _>("created_at"),
            updated_at: r.get::<DateTime<Utc>, _>("updated_at"),
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
        .await?;

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
    // Members
    // -------------------------

    async fn upsert_member(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, m: &Member) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO members (server_id, channel_id, user_id, display_name, muted, deafened, joined_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, NOW()), NOW())
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
        .bind(Some(m.joined_at))
        .execute(&mut **tx)
        .await?;
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
        .await?;
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
        .await?;

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
        .await?;

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

    async fn count_members(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, channel: ChannelId) -> ControlResult<i64> {
        let row = sqlx::query(
            r#"
            SELECT COUNT(*) as cnt
            FROM members
            WHERE server_id = $1 AND channel_id = $2
            "#,
        )
        .bind(server.0)
        .bind(channel.0)
        .fetch_one(&mut **tx)
        .await?;
        Ok(row.get::<i64, _>("cnt"))
    }

    // -------------------------
    // Permissions
    // -------------------------

    async fn decide_permission(&self, _tx: &mut Transaction<'_, Postgres>, req: &PermissionRequest) -> ControlResult<PermissionDecision> {
        if req.is_admin {
            return Ok(PermissionDecision::Allow);
        }

        match req.capability {
            Capability::JoinChannel => Ok(PermissionDecision::Allow),
            Capability::SendMessage => Ok(PermissionDecision::Allow),
            Capability::CreateChannel => Ok(PermissionDecision::Deny),
            _ => Ok(PermissionDecision::Deny),
        }
    }

    // -------------------------
    // Chat
    // -------------------------

    async fn insert_chat_message(&self, tx: &mut Transaction<'_, Postgres>, msg: &ChatMessage) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO chat_messages (id, server_id, channel_id, author_user_id, text, attachments, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(msg.id.0)
        .bind(msg.server_id.0)
        .bind(msg.channel_id.0)
        .bind(msg.author_user_id.0)
        .bind(&msg.text)
        .bind(&msg.attachments)
        .bind(msg.created_at)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn get_chat_message(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: MessageId) -> ControlResult<Option<ChatMessage>> {
        let row = sqlx::query(
            r#"
            SELECT id, server_id, channel_id, author_user_id, text, attachments, created_at
            FROM chat_messages
            WHERE server_id = $1 AND id = $2
            "#,
        )
        .bind(server.0)
        .bind(id.0)
        .fetch_optional(&mut **tx)
        .await?;

        Ok(row.map(|r| ChatMessage {
            id: MessageId(r.get::<Uuid, _>("id")),
            server_id: ServerId(r.get::<Uuid, _>("server_id")),
            channel_id: ChannelId(r.get::<Uuid, _>("channel_id")),
            author_user_id: UserId(r.get::<Uuid, _>("author_user_id")),
            text: r.get::<String, _>("text"),
            attachments: r.get::<Json, _>("attachments"),
            created_at: r.get::<DateTime<Utc>, _>("created_at"),
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
        .await?;
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
        .await?;

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
        .await?;
        Ok(())
    }

    // -------------------------
    // Audit
    // -------------------------

    async fn insert_audit(&self, tx: &mut Transaction<'_, Postgres>, entry: &AuditEntry) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO audit_log (id, server_id, actor_user_id, action, target_type, target_id, context_json, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(entry.id.0)
        .bind(entry.server_id.0)
        .bind(entry.actor_user_id.map(|u| u.0))
        .bind(&entry.action)
        .bind(&entry.target_type)
        .bind(&entry.target_id)
        .bind(&entry.context_json)
        .bind(entry.created_at)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}
