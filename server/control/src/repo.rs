use crate::{
    errors::{ControlError, ControlResult},
    ids::*,
    model::*,
    outbox::OutboxRecord,
    perms::{Capability, Effect, PermissionDecision},
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use ulid::Ulid;
use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

#[async_trait]
pub trait ControlRepo: Send + Sync {
    async fn create_channel(&self, tx: &mut Transaction<'_, Postgres>, ...) -> ControlResult<...>;

    // Channels
    async fn create_channel(&self, tx: &mut Transaction<'_, Postgres>, ch: Channel) -> ControlResult<()>;
    async fn get_channel(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: ChannelId) -> ControlResult<Channel>;
    async fn list_channels(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId) -> ControlResult<Vec<Channel>>;

    // Membership
    async fn count_members(&self, tx: &mut Transaction<'_, Postgres>, channel: ChannelId) -> ControlResult<i64>;
    async fn upsert_member(&self, tx: &mut Transaction<'_, Postgres>, m: Member) -> ControlResult<()>;
    async fn delete_member(&self, tx: &mut Transaction<'_, Postgres>, channel: ChannelId, user: UserId) -> ControlResult<()>;
    async fn list_members(&self, tx: &mut Transaction<'_, Postgres>, channel: ChannelId) -> ControlResult<Vec<Member>>;
    async fn get_member(&self, tx: &mut Transaction<'_, Postgres>, channel: ChannelId, user: UserId) -> ControlResult<Member>;

    // Permissions
    async fn decide_permission(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        channel: Option<ChannelId>,
        user: UserId,
        cap: Capability,
    ) -> ControlResult<PermissionDecision>;

    // Chat
    async fn insert_chat_message(&self, tx: &mut Transaction<'_, Postgres>, msg: ChatMessage) -> ControlResult<()>;
    async fn get_chat_message(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: MessageId) -> ControlResult<ChatMessage>;

    // Outbox claiming for multi-gateway
    async fn claim_outbox_batch(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        limit: i64,
        claim_token: &str,
        claim_ttl_seconds: i64,
    ) -> ControlResult<Vec<OutboxRecord>>;

    async fn ack_outbox_published(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: &str,
        claim_token: &str,
    ) -> ControlResult<()>;


    // Outbox + audit
    async fn insert_outbox(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: String,
        server: ServerId,
        topic: &str,
        key: &str,
        payload: serde_json::Value,
    ) -> ControlResult<()>;

    async fn insert_audit(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: String,
        server: ServerId,
        actor: Option<UserId>,
        action: &str,
        target_type: &str,
        target_id: &str,
        context: serde_json::Value,
    ) -> ControlResult<()>;
}

#[derive(Clone)]
pub struct PgControlRepo {
    pool: PgPool,
}

impl PgControlRepo {
    pub fn new(pool: PgPool) -> Self { Self { pool } }
    fn now() -> DateTime<Utc> { Utc::now() }
    fn ulid() -> String { Ulid::new().to_string() }
}

#[async_trait]
pub trait ControlRepo: Send + Sync {
    async fn tx(&self) -> ControlResult<Transaction<'static, Postgres>>;
        Ok(self.pool.begin().await?)
    }

    async fn create_channel(&self, tx: &mut Transaction<'_, Postgres>, ch: Channel) -> ControlResult<()> {
        sqlx::query!(
            r#"
            INSERT INTO channels (id, server_id, name, parent_id, max_members, max_talkers, created_at, updated_at)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
            "#,
            ch.id.0,
            ch.server_id.0,
            ch.name,
            ch.parent_id.map(|x| x.0),
            ch.max_members,
            ch.max_talkers,
            ch.created_at,
            ch.updated_at
        )
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn get_channel(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: ChannelId) -> ControlResult<Channel> {
        let r = sqlx::query!(
            r#"SELECT id, server_id, name, parent_id, max_members, max_talkers, created_at, updated_at
               FROM channels WHERE server_id=$1 AND id=$2"#,
            server.0, id.0
        )
        .fetch_optional(&mut **tx)
        .await?;

        let r = r.ok_or(ControlError::NotFound("channel"))?;
        Ok(Channel {
            id: ChannelId(r.id),
            server_id: ServerId(r.server_id),
            name: r.name,
            parent_id: r.parent_id.map(ChannelId),
            max_members: r.max_members,
            max_talkers: r.max_talkers,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }

    async fn list_channels(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId) -> ControlResult<Vec<Channel>> {
        let rows = sqlx::query!(
            r#"SELECT id, server_id, name, parent_id, max_members, max_talkers, created_at, updated_at
               FROM channels WHERE server_id=$1 ORDER BY name ASC"#,
            server.0
        )
        .fetch_all(&mut **tx)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| Channel {
                id: ChannelId(r.id),
                server_id: ServerId(r.server_id),
                name: r.name,
                parent_id: r.parent_id.map(ChannelId),
                max_members: r.max_members,
                max_talkers: r.max_talkers,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect())
    }

    async fn count_members(&self, tx: &mut Transaction<'_, Postgres>, channel: ChannelId) -> ControlResult<i64> {
        let r = sqlx::query!(r#"SELECT COUNT(*)::BIGINT as "count!" FROM channel_members WHERE channel_id=$1"#, channel.0)
            .fetch_one(&mut **tx)
            .await?;
        Ok(r.count)
    }

    async fn upsert_member(&self, tx: &mut Transaction<'_, Postgres>, m: Member) -> ControlResult<()> {
        sqlx::query!(
            r#"
            INSERT INTO channel_members (channel_id, user_id, display_name, muted, deafened, joined_at)
            VALUES ($1,$2,$3,$4,$5,$6)
            ON CONFLICT (channel_id, user_id)
            DO UPDATE SET display_name=EXCLUDED.display_name, muted=EXCLUDED.muted, deafened=EXCLUDED.deafened
            "#,
            m.channel_id.0,
            m.user_id.0,
            m.display_name,
            m.muted,
            m.deafened,
            m.joined_at
        )
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn delete_member(&self, tx: &mut Transaction<'_, Postgres>, channel: ChannelId, user: UserId) -> ControlResult<()> {
        let res = sqlx::query!(r#"DELETE FROM channel_members WHERE channel_id=$1 AND user_id=$2"#, channel.0, user.0)
            .execute(&mut **tx)
            .await?;
        if res.rows_affected() == 0 {
            return Err(ControlError::NotFound("member"));
        }
        Ok(())
    }

    async fn list_members(&self, tx: &mut Transaction<'_, Postgres>, channel: ChannelId) -> ControlResult<Vec<Member>> {
        let rows = sqlx::query!(
            r#"SELECT channel_id, user_id, display_name, muted, deafened, joined_at
               FROM channel_members WHERE channel_id=$1 ORDER BY joined_at ASC"#,
            channel.0
        )
        .fetch_all(&mut **tx)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Member {
                channel_id: ChannelId(r.channel_id),
                user_id: UserId(r.user_id),
                display_name: r.display_name,
                muted: r.muted,
                deafened: r.deafened,
                joined_at: r.joined_at,
            })
            .collect())
    }

    async fn get_member(&self, tx: &mut Transaction<'_, Postgres>, channel: ChannelId, user: UserId) -> ControlResult<Member> {
        let r = sqlx::query!(
            r#"SELECT channel_id, user_id, display_name, muted, deafened, joined_at
               FROM channel_members WHERE channel_id=$1 AND user_id=$2"#,
            channel.0, user.0
        )
        .fetch_optional(&mut **tx)
        .await?;
        let r = r.ok_or(ControlError::NotFound("member"))?;
        Ok(Member {
            channel_id: ChannelId(r.channel_id),
            user_id: UserId(r.user_id),
            display_name: r.display_name,
            muted: r.muted,
            deafened: r.deafened,
            joined_at: r.joined_at,
        })
    }

    async fn decide_permission(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        channel: Option<ChannelId>,
        user: UserId,
        cap: Capability,
    ) -> ControlResult<PermissionDecision> {
        let cap_s = cap.as_str();

        // 1) Channel override deny
        if let Some(ch) = channel {
            let deny = sqlx::query_scalar!(
                r#"SELECT 1 as "one!" FROM channel_overrides
                   WHERE channel_id=$1 AND user_id=$2 AND cap=$3 AND effect='deny' LIMIT 1"#,
                ch.0, user.0, cap_s
            )
            .fetch_optional(&mut **tx)
            .await?;
            if deny.is_some() {
                return Ok(PermissionDecision::Deny);
            }
        }

        // 2) Role deny
        let role_deny = sqlx::query_scalar!(
            r#"
            SELECT 1 as "one!" FROM user_roles ur
            JOIN role_caps rc ON rc.role_id = ur.role_id
            WHERE ur.server_id=$1 AND ur.user_id=$2 AND rc.cap=$3 AND rc.effect='deny'
            LIMIT 1
            "#,
            server.0, user.0, cap_s
        )
        .fetch_optional(&mut **tx)
        .await?;
        if role_deny.is_some() {
            return Ok(PermissionDecision::Deny);
        }

        // 3) Channel override grant
        if let Some(ch) = channel {
            let grant = sqlx::query_scalar!(
                r#"SELECT 1 as "one!" FROM channel_overrides
                   WHERE channel_id=$1 AND user_id=$2 AND cap=$3 AND effect='grant' LIMIT 1"#,
                ch.0, user.0, cap_s
            )
            .fetch_optional(&mut **tx)
            .await?;
            if grant.is_some() {
                return Ok(PermissionDecision::Allow);
            }
        }

        // 4) Role grant
        let role_grant = sqlx::query_scalar!(
            r#"
            SELECT 1 as "one!" FROM user_roles ur
            JOIN role_caps rc ON rc.role_id = ur.role_id
            WHERE ur.server_id=$1 AND ur.user_id=$2 AND rc.cap=$3 AND rc.effect='grant'
            LIMIT 1
            "#,
            server.0, user.0, cap_s
        )
        .fetch_optional(&mut **tx)
        .await?;
        if role_grant.is_some() {
            return Ok(PermissionDecision::Allow);
        }

        Ok(PermissionDecision::Deny)
    }


    async fn insert_chat_message(&self, tx: &mut Transaction<'_, Postgres>, msg: ChatMessage) -> ControlResult<()> {
        sqlx::query!(
            r#"INSERT INTO chat_messages (id, server_id, channel_id, author_user_id, text, attachments)
               VALUES ($1,$2,$3,$4,$5,$6)"#,
            msg.id.0,
            msg.server_id.0,
            msg.channel_id.0,
            msg.author_user_id.0,
            msg.text,
            msg.attachments
        )
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn get_chat_message(&self, tx: &mut Transaction<'_, Postgres>, server: ServerId, id: MessageId) -> ControlResult<ChatMessage> {
        let r = sqlx::query!(
            r#"SELECT id, server_id, channel_id, author_user_id, text, attachments, created_at
               FROM chat_messages WHERE server_id=$1 AND id=$2"#,
            server.0,
            id.0
        )
        .fetch_optional(&mut **tx)
        .await?;
        let r = r.ok_or(ControlError::NotFound("chat_message"))?;
        Ok(ChatMessage {
            id: MessageId(r.id),
            server_id: ServerId(r.server_id),
            channel_id: ChannelId(r.channel_id),
            author_user_id: UserId(r.author_user_id),
            text: r.text,
            attachments: r.attachments,
            created_at: r.created_at,
        })
    }

    async fn claim_outbox_batch(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        limit: i64,
        claim_token: &str,
        claim_ttl_seconds: i64,
    ) -> ControlResult<Vec<OutboxRecord>> {
        let rows = sqlx::query!(
            r#"WITH claim AS (
                SELECT id FROM outbox_events
                WHERE server_id=$1
                  AND published_at IS NULL
                  AND (claimed_at IS NULL OR claimed_at < (now() - (make_interval(secs => $4))))
                ORDER BY created_at ASC
                LIMIT $2
                FOR UPDATE SKIP LOCKED
            )
            UPDATE outbox_events o
               SET claimed_at = now(),
                   claim_token = $3
            FROM claim
            WHERE o.id = claim.id
            RETURNING o.id, o.server_id, o.topic, o.key, o.payload, o.created_at"#,
            server.0,
            limit,
            claim_token,
            claim_ttl_seconds as i64,
        )
        .fetch_all(&mut **tx)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| OutboxRecord {
                id: r.id,
                server_id: r.server_id,
                topic: r.topic,
                key: r.key,
                payload: r.payload,
                created_at: r.created_at,
            })
            .collect())
    }

    async fn ack_outbox_published(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: &str,
        claim_token: &str,
    ) -> ControlResult<()> {
        let res = sqlx::query!(
            r#"UPDATE outbox_events
               SET published_at = now()
               WHERE id=$1 AND claim_token=$2 AND published_at IS NULL"#,
            id,
            claim_token
        )
        .execute(&mut **tx)
        .await?;
        if res.rows_affected() != 1 {
            return Err(ControlError::Conflict("outbox ack failed"));
        }
        Ok(())
    }

    async fn insert_outbox(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: String,
        server: ServerId,
        topic: &str,
        key: &str,
        payload: serde_json::Value,
    ) -> ControlResult<()> {
        sqlx::query!(
            r#"INSERT INTO outbox_events (id, server_id, topic, key, payload)
               VALUES ($1,$2,$3,$4,$5)"#,
            id, server.0, topic, key, payload
        )
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn insert_audit(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: String,
        server: ServerId,
        actor: Option<UserId>,
        action: &str,
        target_type: &str,
        target_id: &str,
        context: serde_json::Value,
    ) -> ControlResult<()> {
        sqlx::query!(
            r#"INSERT INTO audit_log (id, server_id, actor_user_id, action, target_type, target_id, context)
               VALUES ($1,$2,$3,$4,$5,$6,$7)"#,
            id, server.0, actor.map(|u| u.0), action, target_type, target_id, context
        )
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}
