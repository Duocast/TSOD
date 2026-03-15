use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value as Json;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::{
    errors::{ControlError, ControlResult},
    ids::{ChannelId, MessageId, OutboxId, ServerId, UserId},
    model::{
        Attachment, AuditEntry, Channel, ChannelListItem, ChatMessage, Member, OutboxEvent,
        OutboxEventRow, PermAuditRow, PermChannelOverrideRecord, PermRoleRecord,
        PermUserSummaryRecord, PermissionRequest,
    },
    perms::Decision,
};

pub async fn is_user_in_channel(
    pool: &PgPool,
    server: ServerId,
    channel: ChannelId,
    user: UserId,
) -> ControlResult<bool> {
    let exists = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT 1
        FROM members
        WHERE server_id = $1 AND channel_id = $2 AND user_id = $3
        LIMIT 1
        "#,
    )
    .bind(server.0)
    .bind(channel.0)
    .bind(user.0)
    .fetch_optional(pool)
    .await
    .context("check channel membership")?
    .is_some();

    Ok(exists)
}

#[async_trait]
pub trait ControlRepo: Send + Sync {
    async fn tx(&self) -> ControlResult<Transaction<'_, Postgres>>;
    // Channels
    async fn create_channel(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ch: &Channel,
    ) -> ControlResult<()>;
    async fn get_channel(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: ChannelId,
    ) -> ControlResult<Option<Channel>>;
    async fn list_channels(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
    ) -> ControlResult<Vec<ChannelListItem>>;
    async fn rename_channel(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: ChannelId,
        new_name: &str,
    ) -> ControlResult<Option<Channel>>;
    async fn update_channel(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: ChannelId,
        name: &str,
        bitrate_bps: i32,
        opus_profile: i32,
    ) -> ControlResult<Option<Channel>>;
    async fn delete_channel(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: ChannelId,
    ) -> ControlResult<bool>;
    async fn list_channel_descendants(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: ChannelId,
    ) -> ControlResult<Vec<ChannelId>>;

    // Members (Member has NO server_id)
    async fn upsert_member(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        m: &Member,
    ) -> ControlResult<()>;
    async fn delete_member(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        channel: ChannelId,
        user: UserId,
    ) -> ControlResult<()>;
    async fn get_member(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        channel: ChannelId,
        user: UserId,
    ) -> ControlResult<Option<Member>>;
    async fn list_members(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        channel: ChannelId,
    ) -> ControlResult<Vec<Member>>;
    async fn count_members(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        channel: ChannelId,
    ) -> ControlResult<i64>;
    async fn list_member_channels_for_user(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        user: UserId,
    ) -> ControlResult<Vec<ChannelId>>;

    async fn perm_list_roles(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
    ) -> ControlResult<Vec<PermRoleRecord>>;
    async fn perm_list_users(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
    ) -> ControlResult<Vec<PermUserSummaryRecord>>;
    async fn perm_upsert_role(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        role_id: Option<&str>,
        name: &str,
        color: i32,
        position: i32,
    ) -> ControlResult<PermRoleRecord>;
    async fn perm_delete_role(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        role_id: &str,
    ) -> ControlResult<bool>;
    async fn perm_replace_role_caps(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        role_id: &str,
        caps: &[(String, String)],
    ) -> ControlResult<()> {
        let server_id: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT server_id FROM roles WHERE id=$1")
                .bind(role_id)
                .fetch_optional(&mut **tx)
                .await
                .context("perm role server lookup")?;
        let server_id = server_id.ok_or(ControlError::NotFound("role"))?;

        sqlx::query("DELETE FROM role_caps WHERE role_id=$1 AND server_id=$2")
            .bind(role_id)
            .bind(server_id)
            .execute(&mut **tx)
            .await
            .context("perm clear role caps")?;
        for (cap, effect) in caps {
            let allowed = effect == "grant";
            sqlx::query(
                "INSERT INTO role_caps (server_id, role_id, cap, allowed) VALUES ($1,$2,$3,$4)",
            )
            .bind(server_id)
            .bind(role_id)
            .bind(cap)
            .bind(allowed)
            .execute(&mut **tx)
            .await
            .context("perm insert role cap")?;
        }
        Ok(())
    }

    async fn perm_replace_user_roles(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        user: UserId,
        role_ids: &[String],
    ) -> ControlResult<()>;
    async fn perm_list_channel_overrides(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        channel: ChannelId,
    ) -> ControlResult<Vec<PermChannelOverrideRecord>>;
    async fn perm_set_channel_override(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        rec: &PermChannelOverrideRecord,
    ) -> ControlResult<()>;
    async fn perm_query_audit(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        limit: i64,
    ) -> ControlResult<Vec<PermAuditRow>>;
    async fn perm_actor_max_role_position(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        user: UserId,
    ) -> ControlResult<i32>;
    async fn perm_user_max_role_position(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        user: UserId,
    ) -> ControlResult<i32>;
    async fn perm_get_role(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        role_id: &str,
    ) -> ControlResult<Option<PermRoleRecord>>;

    // Permissions
    async fn decide_permission(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        req: &PermissionRequest,
    ) -> ControlResult<Decision>;

    // Chat (ChatMessage uses author_user_id; no Default)
    async fn insert_chat_message(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        msg: &ChatMessage,
    ) -> ControlResult<()>;
    async fn get_chat_message(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: MessageId,
    ) -> ControlResult<Option<ChatMessage>>;

    async fn get_attachment(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
    ) -> ControlResult<Option<Attachment>>;

    // Outbox
    async fn insert_outbox(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ev: &OutboxEvent,
    ) -> ControlResult<()>;
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
    async fn insert_audit(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        entry: &AuditEntry,
    ) -> ControlResult<()>;

    // User profiles
    async fn upsert_user_profile(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
        display_name: Option<&str>,
        description: Option<&str>,
        accent_color: Option<i32>,
        custom_status_text: Option<&str>,
        custom_status_emoji: Option<&str>,
        custom_status_expires: Option<Option<DateTime<Utc>>>,
        links_json: Option<serde_json::Value>,
    ) -> ControlResult<()>;

    /// Clear custom status for all profiles whose expiry has passed.
    async fn clear_expired_custom_statuses(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> ControlResult<Vec<(UserId, ServerId)>>;

    async fn get_user_profile(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
    ) -> ControlResult<Option<crate::model::UserProfileRow>>;

    async fn set_profile_avatar(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
        avatar_url: &str,
    ) -> ControlResult<()>;

    async fn set_profile_banner(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
        banner_url: &str,
    ) -> ControlResult<()>;

    // Profile asset uploads
    async fn create_asset_upload_session(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session_id: Uuid,
        user_id: UserId,
        server_id: ServerId,
        purpose: &str,
        mime_type: &str,
        byte_length: i64,
    ) -> ControlResult<()>;

    async fn store_verified_asset(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session_id: Uuid,
        asset_data: &[u8],
    ) -> ControlResult<()>;

    async fn get_asset_upload_session(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session_id: Uuid,
        user_id: UserId,
    ) -> ControlResult<Option<crate::model::AssetUploadSession>>;

    /// Create a default profile row for a new user (ON CONFLICT DO NOTHING).
    async fn create_default_profile(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
        display_name: &str,
    ) -> ControlResult<()>;

    /// Fetch badges for a user (joined with badge_definitions).
    async fn get_user_badges(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
    ) -> ControlResult<Vec<crate::model::UserBadgeRow>>;

    /// Create a new badge definition.
    async fn create_badge_definition(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        badge: &crate::model::BadgeDefinitionRow,
    ) -> ControlResult<()>;

    /// Grant a badge to a user.
    async fn grant_badge(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        badge_id: &str,
        server_id: ServerId,
    ) -> ControlResult<()>;

    /// Revoke a badge from a user.
    async fn revoke_badge(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        badge_id: &str,
        server_id: ServerId,
    ) -> ControlResult<()>;

    /// Fetch role display info for a user.
    async fn get_user_roles_display(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
    ) -> ControlResult<Vec<crate::model::UserRoleRow>>;

    /// Verify that an asset_id exists and belongs to this user.
    async fn verify_asset_ownership(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        asset_id: &str,
        user_id: UserId,
    ) -> ControlResult<bool>;
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
    async fn tx(&self) -> ControlResult<Transaction<'_, Postgres>> {
        Ok(self.pool.begin().await?)
    }

    // -------------------------
    // Channels
    // -------------------------

    async fn create_channel(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ch: &Channel,
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO channels (id, server_id, name, parent_id, max_members, max_talkers, channel_type, description, bitrate_bps, opus_profile, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NOW(), NOW())
            "#,
        )
        .bind(ch.id.0)
        .bind(ch.server_id.0)
        .bind(&ch.name)
        .bind(ch.parent_id.map(|p| p.0))
        .bind(ch.max_members)
        .bind(ch.max_talkers)
        .bind(ch.channel_type)
        .bind(&ch.description)
        .bind(ch.bitrate_bps)
        .bind(ch.opus_profile)
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
            SELECT id, server_id, name, parent_id, max_members, max_talkers, channel_type, description, bitrate_bps, opus_profile, created_at, updated_at
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
            max_talkers: r.get::<Option<i32>, _>("max_talkers"),
            channel_type: r.get::<i32, _>("channel_type"),
            description: r.get::<String, _>("description"),
            bitrate_bps: r.get::<i32, _>("bitrate_bps"),
            opus_profile: r.get::<i32, _>("opus_profile"),
            created_at: r.get::<DateTime<Utc>, _>("created_at"),
            updated_at: r.get::<DateTime<Utc>, _>("updated_at"),
        }))
    }

    async fn list_channels(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
    ) -> ControlResult<Vec<ChannelListItem>> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, parent_id, max_members, max_talkers, channel_type, description, bitrate_bps, opus_profile
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
                channel_type: r.get::<i32, _>("channel_type"),
                description: r.get::<String, _>("description"),
                bitrate_bps: r.get::<i32, _>("bitrate_bps"),
                opus_profile: r.get::<i32, _>("opus_profile"),
            });
        }
        Ok(out)
    }

    async fn rename_channel(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: ChannelId,
        new_name: &str,
    ) -> ControlResult<Option<Channel>> {
        let row = sqlx::query(
            r#"
            UPDATE channels
            SET name = $3, updated_at = NOW()
            WHERE server_id = $1 AND id = $2
            RETURNING id, server_id, name, parent_id, max_members, max_talkers, channel_type, description, bitrate_bps, opus_profile, created_at, updated_at
            "#,
        )
        .bind(server.0)
        .bind(id.0)
        .bind(new_name)
        .fetch_optional(&mut **tx)
        .await
        .context("rename channel")?;

        Ok(row.map(|r| Channel {
            id: ChannelId(r.get::<Uuid, _>("id")),
            server_id: ServerId(r.get::<Uuid, _>("server_id")),
            name: r.get::<String, _>("name"),
            parent_id: r.get::<Option<Uuid>, _>("parent_id").map(ChannelId),
            max_members: r.get::<Option<i32>, _>("max_members"),
            max_talkers: r.get::<Option<i32>, _>("max_talkers"),
            channel_type: r.get::<i32, _>("channel_type"),
            description: r.get::<String, _>("description"),
            bitrate_bps: r.get::<i32, _>("bitrate_bps"),
            opus_profile: r.get::<i32, _>("opus_profile"),
            created_at: r.get::<DateTime<Utc>, _>("created_at"),
            updated_at: r.get::<DateTime<Utc>, _>("updated_at"),
        }))
    }

    async fn update_channel(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: ChannelId,
        name: &str,
        bitrate_bps: i32,
        opus_profile: i32,
    ) -> ControlResult<Option<Channel>> {
        let row = sqlx::query(
            r#"
            UPDATE channels
            SET name = $3, bitrate_bps = $4, opus_profile = $5, updated_at = NOW()
            WHERE server_id = $1 AND id = $2
            RETURNING id, server_id, name, parent_id, max_members, max_talkers, channel_type, description, bitrate_bps, opus_profile, created_at, updated_at
            "#,
        )
        .bind(server.0)
        .bind(id.0)
        .bind(name)
        .bind(bitrate_bps)
        .bind(opus_profile)
        .fetch_optional(&mut **tx)
        .await
        .context("update channel")?;

        Ok(row.map(|r| Channel {
            id: ChannelId(r.get::<Uuid, _>("id")),
            server_id: ServerId(r.get::<Uuid, _>("server_id")),
            name: r.get::<String, _>("name"),
            parent_id: r.get::<Option<Uuid>, _>("parent_id").map(ChannelId),
            max_members: r.get::<Option<i32>, _>("max_members"),
            max_talkers: r.get::<Option<i32>, _>("max_talkers"),
            channel_type: r.get::<i32, _>("channel_type"),
            description: r.get::<String, _>("description"),
            bitrate_bps: r.get::<i32, _>("bitrate_bps"),
            opus_profile: r.get::<i32, _>("opus_profile"),
            created_at: r.get::<DateTime<Utc>, _>("created_at"),
            updated_at: r.get::<DateTime<Utc>, _>("updated_at"),
        }))
    }

    async fn delete_channel(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: ChannelId,
    ) -> ControlResult<bool> {
        let res = sqlx::query(
            r#"
            DELETE FROM channels
            WHERE server_id = $1 AND id = $2
            "#,
        )
        .bind(server.0)
        .bind(id.0)
        .execute(&mut **tx)
        .await
        .context("delete channel")?;
        Ok(res.rows_affected() > 0)
    }

    async fn list_channel_descendants(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        id: ChannelId,
    ) -> ControlResult<Vec<ChannelId>> {
        let rows = sqlx::query(
            r#"
            WITH RECURSIVE descendants AS (
              SELECT id FROM channels WHERE server_id = $1 AND id = $2
              UNION ALL
              SELECT c.id
              FROM channels c
              INNER JOIN descendants d ON c.parent_id = d.id
              WHERE c.server_id = $1
            )
            SELECT id FROM descendants
            "#,
        )
        .bind(server.0)
        .bind(id.0)
        .fetch_all(&mut **tx)
        .await
        .context("list channel descendants")?;

        Ok(rows
            .into_iter()
            .map(|r| ChannelId(r.get::<Uuid, _>("id")))
            .collect())
    }

    // -------------------------
    // Members
    // -------------------------

    async fn upsert_member(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        m: &Member,
    ) -> ControlResult<()> {
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
        .await
        .context("upsert member")?;
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
            SELECT m.channel_id, m.user_id, m.display_name, m.muted, m.deafened, m.joined_at,
                   COALESCE(up.custom_status_text, '') AS custom_status_text,
                   COALESCE(up.custom_status_emoji, '') AS custom_status_emoji
            FROM members m
            LEFT JOIN user_profiles up ON up.user_id = m.user_id AND up.server_id = m.server_id
            WHERE m.server_id = $1 AND m.channel_id = $2 AND m.user_id = $3
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
            custom_status_text: r.get::<String, _>("custom_status_text"),
            custom_status_emoji: r.get::<String, _>("custom_status_emoji"),
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
            SELECT m.channel_id, m.user_id, m.display_name, m.muted, m.deafened, m.joined_at,
                   COALESCE(up.custom_status_text, '') AS custom_status_text,
                   COALESCE(up.custom_status_emoji, '') AS custom_status_emoji
            FROM members m
            LEFT JOIN user_profiles up ON up.user_id = m.user_id AND up.server_id = m.server_id
            WHERE m.server_id = $1 AND m.channel_id = $2
            ORDER BY m.joined_at ASC
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
                custom_status_text: r.get::<String, _>("custom_status_text"),
                custom_status_emoji: r.get::<String, _>("custom_status_emoji"),
            });
        }
        Ok(out)
    }

    async fn count_members(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        channel: ChannelId,
    ) -> ControlResult<i64> {
        let n: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)::bigint
            FROM members
            WHERE server_id = $1 AND channel_id = $2
            "#,
        )
        .bind(server.0)
        .bind(channel.0)
        .fetch_one(&mut **tx)
        .await
        .context("count members")?;

        Ok(n)
    }

    async fn list_member_channels_for_user(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        user: UserId,
    ) -> ControlResult<Vec<ChannelId>> {
        let rows = sqlx::query(
            r#"
            SELECT channel_id
            FROM members
            WHERE server_id = $1 AND user_id = $2
            "#,
        )
        .bind(server.0)
        .bind(user.0)
        .fetch_all(&mut **tx)
        .await
        .context("list member channels for user")?;

        Ok(rows
            .into_iter()
            .map(|r| ChannelId(r.get::<Uuid, _>("channel_id")))
            .collect())
    }

    // -------------------------
    // Admin permissions RPC backing ops
    // -------------------------

    async fn perm_list_roles(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
    ) -> ControlResult<Vec<PermRoleRecord>> {
        let rows = sqlx::query(
            "SELECT id, name, COALESCE(color,0) AS color, position, is_everyone FROM roles WHERE server_id=$1 ORDER BY position ASC, id ASC",
        )
        .bind(server.0)
        .fetch_all(&mut **tx)
        .await
        .context("perm list roles")?;
        Ok(rows
            .into_iter()
            .map(|r| PermRoleRecord {
                role_id: r.get("id"),
                name: r.get("name"),
                color: r.get("color"),
                role_position: r.get("position"),
                is_everyone: r.get("is_everyone"),
            })
            .collect())
    }

    async fn perm_list_users(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
    ) -> ControlResult<Vec<PermUserSummaryRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT
                au.user_id,
                COALESCE(MAX(m.display_name), CONCAT('user-', LEFT(au.user_id::text, 8))) AS display_name,
                MIN(m.joined_at) AS joined_at,
                MAX(ad.last_seen) AS last_seen,
                COALESCE(MAX(r.position), 0) AS highest_role_position,
                COALESCE(array_agg(DISTINCT ur.role_id) FILTER (WHERE ur.role_id IS NOT NULL), ARRAY[]::text[]) AS role_ids
            FROM auth_users au
            LEFT JOIN auth_devices ad
              ON ad.user_id = au.user_id
             AND ad.revoked_at IS NULL
            LEFT JOIN members m
              ON m.server_id = $1
             AND m.user_id = au.user_id
            LEFT JOIN user_roles ur
              ON ur.server_id = $1
             AND ur.user_id = au.user_id
            LEFT JOIN roles r
              ON r.server_id = ur.server_id
             AND r.id = ur.role_id
            WHERE EXISTS (
                SELECT 1
                FROM user_roles urx
                WHERE urx.server_id = $1
                  AND urx.user_id = au.user_id
            )
               OR EXISTS (
                SELECT 1
                FROM members mx
                WHERE mx.server_id = $1
                  AND mx.user_id = au.user_id
            )
            GROUP BY au.user_id
            ORDER BY lower(COALESCE(MAX(m.display_name), CONCAT('user-', LEFT(au.user_id::text, 8)))) ASC
            "#,
        )
        .bind(server.0)
        .fetch_all(&mut **tx)
        .await
        .context("perm list users")?;

        Ok(rows
            .into_iter()
            .map(|r| PermUserSummaryRecord {
                user_id: UserId(r.get("user_id")),
                display_name: r.get("display_name"),
                joined_at: r.get("joined_at"),
                last_seen: r.get("last_seen"),
                highest_role_position: r.get("highest_role_position"),
                role_ids: r.get("role_ids"),
                is_admin: false,
            })
            .collect())
    }

    async fn perm_upsert_role(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        role_id: Option<&str>,
        name: &str,
        color: i32,
        position: i32,
    ) -> ControlResult<PermRoleRecord> {
        let rid = role_id.unwrap_or("role");
        let row = if role_id.is_some() {
            sqlx::query("UPDATE roles SET name=$3, color=$4, position=$5 WHERE server_id=$1 AND id=$2 RETURNING id,name,color,position,is_everyone")
                .bind(server.0).bind(rid).bind(name).bind(color).bind(position)
                .fetch_optional(&mut **tx).await.context("perm update role")?
        } else {
            let generated = format!("role_{}", uuid::Uuid::new_v4().simple());
            sqlx::query("INSERT INTO roles (id, server_id, name, color, position, is_everyone, created_at) VALUES ($1,$2,$3,$4,$5,false,NOW()) RETURNING id,name,color,position,is_everyone")
                .bind(generated).bind(server.0).bind(name).bind(color).bind(position)
                .fetch_optional(&mut **tx).await.context("perm create role")?
        }.ok_or(ControlError::NotFound("role"))?;
        Ok(PermRoleRecord {
            role_id: row.get("id"),
            name: row.get("name"),
            color: row.get("color"),
            role_position: row.get("position"),
            is_everyone: row.get("is_everyone"),
        })
    }

    async fn perm_delete_role(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        role_id: &str,
    ) -> ControlResult<bool> {
        let n = sqlx::query("DELETE FROM roles WHERE server_id=$1 AND id=$2 AND is_everyone=false")
            .bind(server.0)
            .bind(role_id)
            .execute(&mut **tx)
            .await
            .context("perm delete role")?
            .rows_affected();
        Ok(n > 0)
    }

    async fn perm_replace_role_caps(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        role_id: &str,
        caps: &[(String, String)],
    ) -> ControlResult<()> {
        let server_id: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT server_id FROM roles WHERE id=$1")
                .bind(role_id)
                .fetch_optional(&mut **tx)
                .await
                .context("perm role server lookup")?;
        let server_id = server_id.ok_or(ControlError::NotFound("role"))?;

        sqlx::query("DELETE FROM role_caps WHERE role_id=$1 AND server_id=$2")
            .bind(role_id)
            .bind(server_id)
            .execute(&mut **tx)
            .await
            .context("perm clear role caps")?;
        for (cap, effect) in caps {
            let allowed = effect == "grant";
            sqlx::query(
                "INSERT INTO role_caps (server_id, role_id, cap, allowed) VALUES ($1,$2,$3,$4)",
            )
            .bind(server_id)
            .bind(role_id)
            .bind(cap)
            .bind(allowed)
            .execute(&mut **tx)
            .await
            .context("perm insert role cap")?;
        }
        Ok(())
    }

    async fn perm_replace_user_roles(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        user: UserId,
        role_ids: &[String],
    ) -> ControlResult<()> {
        sqlx::query("DELETE FROM user_roles WHERE server_id=$1 AND user_id=$2")
            .bind(server.0)
            .bind(user.0)
            .execute(&mut **tx)
            .await
            .context("perm clear user roles")?;
        for rid in role_ids {
            sqlx::query("INSERT INTO user_roles (server_id, user_id, role_id) VALUES ($1,$2,$3)")
                .bind(server.0)
                .bind(user.0)
                .bind(rid)
                .execute(&mut **tx)
                .await
                .context("perm insert user role")?;
        }
        Ok(())
    }

    async fn perm_list_channel_overrides(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        channel: ChannelId,
    ) -> ControlResult<Vec<PermChannelOverrideRecord>> {
        let mut out = Vec::new();
        let rows = sqlx::query(
            "SELECT server_id, channel_id, user_id, cap, effect FROM channel_user_overrides WHERE channel_id=$1",
        )
        .bind(channel.0)
        .fetch_all(&mut **tx)
        .await
        .context("perm list user overrides")?;
        for r in rows {
            out.push(PermChannelOverrideRecord {
                channel_id: ChannelId(r.get("channel_id")),
                role_id: None,
                user_id: Some(UserId(r.get("user_id"))),
                cap: r.get("cap"),
                effect: r.get("effect"),
            });
        }
        let rows2=sqlx::query("SELECT server_id, channel_id, role_id, cap, effect FROM channel_role_overrides WHERE channel_id=$1")
            .bind(channel.0).fetch_all(&mut **tx).await.context("perm list role overrides")?;
        for r in rows2 {
            out.push(PermChannelOverrideRecord {
                channel_id: ChannelId(r.get("channel_id")),
                role_id: Some(r.get("role_id")),
                user_id: None,
                cap: r.get("cap"),
                effect: r.get("effect"),
            });
        }
        Ok(out)
    }

    async fn perm_set_channel_override(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        rec: &PermChannelOverrideRecord,
    ) -> ControlResult<()> {
        let server_id: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT server_id FROM channels WHERE id=$1")
                .bind(rec.channel_id.0)
                .fetch_optional(&mut **tx)
                .await
                .context("perm channel server lookup")?;
        let server_id = server_id.ok_or(ControlError::NotFound("channel"))?;

        if rec.effect == "inherit" {
            if let Some(role_id) = &rec.role_id {
                sqlx::query("DELETE FROM channel_role_overrides WHERE server_id=$1 AND channel_id=$2 AND role_id=$3 AND cap=$4")
                    .bind(server_id)
                    .bind(rec.channel_id.0)
                    .bind(role_id)
                    .bind(&rec.cap)
                    .execute(&mut **tx)
                    .await
                    .context("perm delete role override")?;
            } else if let Some(user_id) = rec.user_id {
                sqlx::query(
                    "DELETE FROM channel_user_overrides WHERE server_id=$1 AND channel_id=$2 AND user_id=$3 AND cap=$4",
                )
                .bind(server_id)
                .bind(rec.channel_id.0)
                .bind(user_id.0)
                .bind(&rec.cap)
                .execute(&mut **tx)
                .await
                .context("perm delete user override")?;
            }
            return Ok(());
        }

        if let Some(role_id) = &rec.role_id {
            sqlx::query(
                "DELETE FROM channel_role_overrides WHERE server_id=$1 AND channel_id=$2 AND role_id=$3 AND cap=$4",
            )
            .bind(server_id)
            .bind(rec.channel_id.0)
            .bind(role_id)
            .bind(&rec.cap)
            .execute(&mut **tx)
            .await?;
            sqlx::query("INSERT INTO channel_role_overrides (server_id, channel_id, role_id, cap, effect) VALUES ($1,$2,$3,$4,$5)")
                .bind(server_id)
                .bind(rec.channel_id.0)
                .bind(role_id)
                .bind(&rec.cap)
                .bind(&rec.effect)
                .execute(&mut **tx)
                .await
                .context("perm upsert role override")?;
        } else if let Some(user_id) = rec.user_id {
            sqlx::query(
                "DELETE FROM channel_user_overrides WHERE server_id=$1 AND channel_id=$2 AND user_id=$3 AND cap=$4",
            )
            .bind(server_id)
            .bind(rec.channel_id.0)
            .bind(user_id.0)
            .bind(&rec.cap)
            .execute(&mut **tx)
            .await?;
            sqlx::query("INSERT INTO channel_user_overrides (server_id, channel_id, user_id, cap, effect) VALUES ($1,$2,$3,$4,$5)")
                .bind(server_id)
                .bind(rec.channel_id.0)
                .bind(user_id.0)
                .bind(&rec.cap)
                .bind(&rec.effect)
                .execute(&mut **tx)
                .await
                .context("perm upsert user override")?;
        }
        Ok(())
    }

    async fn perm_query_audit(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        limit: i64,
    ) -> ControlResult<Vec<PermAuditRow>> {
        let rows=sqlx::query("SELECT action,target_type,target_id,created_at FROM audit_log WHERE server_id=$1 ORDER BY created_at DESC LIMIT $2")
            .bind(server.0).bind(limit).fetch_all(&mut **tx).await.context("perm query audit")?;
        Ok(rows
            .into_iter()
            .map(|r| PermAuditRow {
                action: r.get("action"),
                target_type: r.get("target_type"),
                target_id: r.get("target_id"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    async fn perm_actor_max_role_position(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        user: UserId,
    ) -> ControlResult<i32> {
        let pos: Option<i32> = sqlx::query_scalar(
            "SELECT MAX(r.position) FROM roles r LEFT JOIN user_roles ur ON ur.server_id=$1 AND ur.user_id=$2 AND ur.role_id=r.id WHERE r.server_id=$1 AND (r.is_everyone = TRUE OR ur.role_id IS NOT NULL)",
        )
        .bind(server.0)
        .bind(user.0)
        .fetch_one(&mut **tx)
        .await
        .context("perm actor max role position")?;
        Ok(pos.unwrap_or(0))
    }

    async fn perm_user_max_role_position(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        user: UserId,
    ) -> ControlResult<i32> {
        let pos: Option<i32> = sqlx::query_scalar(
            "SELECT MAX(r.position) FROM roles r LEFT JOIN user_roles ur ON ur.server_id=$1 AND ur.user_id=$2 AND ur.role_id=r.id WHERE r.server_id=$1 AND (r.is_everyone = TRUE OR ur.role_id IS NOT NULL)",
        )
        .bind(server.0)
        .bind(user.0)
        .fetch_one(&mut **tx)
        .await
        .context("perm target max role position")?;
        Ok(pos.unwrap_or(0))
    }

    async fn perm_get_role(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        server: ServerId,
        role_id: &str,
    ) -> ControlResult<Option<PermRoleRecord>> {
        let row = sqlx::query(
            "SELECT id, name, COALESCE(color,0) AS color, position, is_everyone FROM roles WHERE server_id=$1 AND id=$2",
        )
        .bind(server.0)
        .bind(role_id)
        .fetch_optional(&mut **tx)
        .await
        .context("perm get role")?;
        Ok(row.map(|r| PermRoleRecord {
            role_id: r.get("id"),
            name: r.get("name"),
            color: r.get("color"),
            role_position: r.get("position"),
            is_everyone: r.get("is_everyone"),
        }))
    }

    // -------------------------
    // Permissions
    // -------------------------

    async fn decide_permission(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        req: &PermissionRequest,
    ) -> ControlResult<Decision> {
        // Admin/owner bypass.
        if req.is_admin {
            return Ok(Decision::Allow);
        }

        let cap = req.capability.as_str();

        // Base permission from roles in hierarchy order:
        // @everyone first, then regular roles by position (low -> high),
        // where later rows override earlier ones.
        let base_role_allowed: Option<bool> = sqlx::query_scalar(
            r#"
            SELECT rc.allowed
            FROM role_caps rc
            JOIN roles r ON r.id = rc.role_id
            LEFT JOIN user_roles ur
              ON ur.server_id = $1
             AND ur.user_id = $2
             AND ur.role_id = r.id
            WHERE r.server_id = $1
              AND rc.cap = $3
              AND rc.server_id = $1
              AND (r.is_everyone = TRUE OR ur.role_id IS NOT NULL)
            ORDER BY r.is_everyone DESC, r.position ASC, r.id ASC
            "#,
        )
        .bind(req.server_id.0)
        .bind(req.user_id.0)
        .bind(cap)
        .fetch_optional(&mut **tx)
        .await
        .context("decide_permission base role effect")?;

        let base_allowed = base_role_allowed.unwrap_or(false);

        let overwrite_decision = if let Some(channel_id) = req.channel_id {
            // Discord-like channel overwrite evaluation:
            // @everyone role overwrite -> role overwrites -> user overwrite,
            // with deny winning over allow at overwrite layer.
            let everyone_role_override: Option<String> = sqlx::query_scalar(
                r#"
                SELECT cro.effect
                FROM channel_role_overrides cro
                JOIN roles r ON r.id = cro.role_id
                WHERE cro.server_id = $2
                  AND cro.channel_id = $1
                  AND r.server_id = $2
                  AND r.is_everyone = TRUE
                  AND cro.cap = $3
                ORDER BY cro.effect DESC
                "#,
            )
            .bind(channel_id.0)
            .bind(req.server_id.0)
            .bind(cap)
            .fetch_optional(&mut **tx)
            .await
            .context("decide_permission everyone overwrite")?;

            let role_overwrite_effects: Vec<String> = sqlx::query_scalar(
                r#"
                SELECT cro.effect
                FROM channel_role_overrides cro
                JOIN user_roles ur
                  ON ur.server_id = $2
                 AND ur.user_id = $3
                 AND ur.role_id = cro.role_id
                JOIN roles r ON r.id = ur.role_id
                WHERE cro.server_id = $2
                  AND cro.channel_id = $1
                  AND cro.cap = $4
                ORDER BY r.position ASC, r.id ASC
                "#,
            )
            .bind(channel_id.0)
            .bind(req.server_id.0)
            .bind(req.user_id.0)
            .bind(cap)
            .fetch_all(&mut **tx)
            .await
            .context("decide_permission role overwrites")?;

            let member_override_effect: Option<String> = sqlx::query_scalar(
                r#"
                SELECT effect
                FROM channel_user_overrides
                WHERE server_id = $4
                  AND channel_id = $1
                  AND user_id = $2
                  AND cap = $3
                ORDER BY effect DESC
                "#,
            )
            .bind(channel_id.0)
            .bind(req.user_id.0)
            .bind(cap)
            .bind(req.server_id.0)
            .fetch_optional(&mut **tx)
            .await
            .context("decide_permission member overwrite")?;

            let mut has_allow = false;
            let mut has_deny = false;

            for effect in everyone_role_override
                .iter()
                .chain(role_overwrite_effects.iter())
                .chain(member_override_effect.iter())
            {
                match effect.as_str() {
                    "deny" => has_deny = true,
                    "grant" => has_allow = true,
                    _ => {}
                }
            }

            if has_deny {
                Some(Decision::Deny)
            } else if has_allow {
                Some(Decision::Allow)
            } else {
                None
            }
        } else {
            None
        };

        if let Some(decision) = overwrite_decision {
            Ok(decision)
        } else if base_allowed {
            Ok(Decision::Allow)
        } else {
            Ok(Decision::Deny)
        }
    }

    // -------------------------
    // Chat
    // -------------------------

    async fn insert_chat_message(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        msg: &ChatMessage,
    ) -> ControlResult<()> {
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
            SELECT id, server_id, channel_id, author_user_id, text, attachments, created_at
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
            author_user_id: UserId(r.get::<Uuid, _>("author_user_id")),
            text: r.get::<String, _>("text"),
            attachments: r.get::<Json, _>("attachments"),
            created_at: r.get::<DateTime<Utc>, _>("created_at"),
        }))
    }

    async fn get_attachment(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
    ) -> ControlResult<Option<Attachment>> {
        let row = sqlx::query(
            r#"
            SELECT id, server_id, channel_id, uploader_user_id, filename, content_type, size_bytes, sha256, quarantined
            FROM attachments
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .context("get attachment")?;

        Ok(row.map(|r| Attachment {
            id: r.get::<Uuid, _>("id"),
            server_id: ServerId(r.get::<Uuid, _>("server_id")),
            channel_id: ChannelId(r.get::<Uuid, _>("channel_id")),
            uploader_user_id: UserId(r.get::<Uuid, _>("uploader_user_id")),
            filename: r.get::<String, _>("filename"),
            content_type: r.get::<String, _>("content_type"),
            size_bytes: r.get::<i64, _>("size_bytes"),
            sha256: r.get::<Option<String>, _>("sha256"),
            quarantined: r.get::<bool, _>("quarantined"),
        }))
    }

    // -------------------------
    // Outbox
    // -------------------------

    async fn insert_outbox(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ev: &OutboxEvent,
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO outbox_events (id, server_id, topic, payload, payload_json, created_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            "#,
        )
        .bind(ev.id.0)
        .bind(ev.server_id.0)
        .bind(&ev.topic)
        .bind(&ev.payload_json)
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
        // Requires your migration adding claim_token/claimed_at columns.
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
            let id = r
                .try_get::<Uuid, _>("id")
                .context("decode outbox_events.id as uuid")?;
            let server_id = r
                .try_get::<Uuid, _>("server_id")
                .context("decode outbox_events.server_id as uuid")?;
            let topic = r
                .try_get::<String, _>("topic")
                .context("decode outbox_events.topic")?;
            let payload_json = r
                .try_get::<Json, _>("payload_json")
                .context("decode outbox_events.payload_json as jsonb")?;

            out.push(OutboxEventRow {
                id: OutboxId(id),
                server_id: ServerId(server_id),
                topic,
                payload_json,
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

    async fn insert_audit(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        entry: &AuditEntry,
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO audit_log (
                id,
                server_id,
                actor_user_id,
                action,
                target_type,
                target_id,
                context,
                context_json,
                created_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(entry.id.0)
        .bind(entry.server_id.0)
        .bind(entry.actor_user_id.map(|u| u.0))
        .bind(&entry.action)
        .bind(&entry.target_type)
        .bind(&entry.target_id)
        .bind(&entry.context_json)
        .bind(&entry.context_json)
        .bind(entry.created_at)
        .execute(&mut **tx)
        .await
        .context("insert audit")?;
        Ok(())
    }

    // ── User profiles ──────────────────────────────────────────────────

    async fn upsert_user_profile(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
        display_name: Option<&str>,
        description: Option<&str>,
        accent_color: Option<i32>,
        custom_status_text: Option<&str>,
        custom_status_emoji: Option<&str>,
        custom_status_expires: Option<Option<DateTime<Utc>>>,
        links_json: Option<Json>,
    ) -> ControlResult<()> {
        // $9 is a bool flag: true means "update custom_status_expires", false means "leave it alone".
        // $10 is the actual expiry value (nullable).
        let update_expires = custom_status_expires.is_some();
        let expires_val = custom_status_expires.flatten();
        sqlx::query(
            r#"
            INSERT INTO user_profiles
                (user_id, server_id, display_name, description, accent_color,
                 custom_status_text, custom_status_emoji, links,
                 custom_status_expires, created_at, updated_at)
            VALUES ($1, $2,
                COALESCE($3, ''),
                COALESCE($4, ''),
                COALESCE($5, 0),
                COALESCE($6, ''),
                COALESCE($7, ''),
                COALESCE($8, '[]'::jsonb),
                $10,
                NOW(), NOW())
            ON CONFLICT (user_id) DO UPDATE SET
                server_id          = $2,
                display_name       = CASE WHEN $3 IS NOT NULL THEN $3 ELSE user_profiles.display_name END,
                description        = CASE WHEN $4 IS NOT NULL THEN $4 ELSE user_profiles.description END,
                accent_color       = CASE WHEN $5 IS NOT NULL THEN $5 ELSE user_profiles.accent_color END,
                custom_status_text = CASE WHEN $6 IS NOT NULL THEN $6 ELSE user_profiles.custom_status_text END,
                custom_status_emoji= CASE WHEN $7 IS NOT NULL THEN $7 ELSE user_profiles.custom_status_emoji END,
                links              = CASE WHEN $8 IS NOT NULL THEN $8 ELSE user_profiles.links END,
                custom_status_expires = CASE WHEN $9 THEN $10 ELSE user_profiles.custom_status_expires END,
                updated_at         = NOW()
            "#,
        )
        .bind(user_id.0)
        .bind(server_id.0)
        .bind(display_name)
        .bind(description)
        .bind(accent_color)
        .bind(custom_status_text)
        .bind(custom_status_emoji)
        .bind(links_json)
        .bind(update_expires)
        .bind(expires_val)
        .execute(&mut **tx)
        .await
        .context("upsert user profile")?;
        Ok(())
    }

    async fn clear_expired_custom_statuses(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> ControlResult<Vec<(UserId, ServerId)>> {
        let rows = sqlx::query(
            r#"
            UPDATE user_profiles
            SET custom_status_text = '',
                custom_status_emoji = '',
                custom_status_expires = NULL,
                updated_at = NOW()
            WHERE custom_status_expires IS NOT NULL
              AND custom_status_expires <= NOW()
            RETURNING user_id, server_id
            "#,
        )
        .fetch_all(&mut **tx)
        .await
        .context("clear expired custom statuses")?;

        Ok(rows
            .into_iter()
            .map(|r| (UserId(r.get("user_id")), ServerId(r.get("server_id"))))
            .collect())
    }

    async fn get_user_profile(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
    ) -> ControlResult<Option<crate::model::UserProfileRow>> {
        let row = sqlx::query(
            r#"
            SELECT
                user_id, server_id,
                COALESCE(display_name, '') AS display_name,
                COALESCE(description, '')  AS description,
                COALESCE(accent_color, 0)  AS accent_color,
                COALESCE(custom_status_text, '')  AS custom_status_text,
                COALESCE(custom_status_emoji, '') AS custom_status_emoji,
                custom_status_expires,
                COALESCE(avatar_asset_url, '')    AS avatar_asset_url,
                COALESCE(banner_asset_url, '')    AS banner_asset_url,
                COALESCE(links, '[]'::jsonb)      AS links,
                created_at, updated_at
            FROM user_profiles
            WHERE user_id = $1 AND server_id = $2
            "#,
        )
        .bind(user_id.0)
        .bind(server_id.0)
        .fetch_optional(&mut **tx)
        .await
        .context("get user profile")?;

        Ok(row.map(|r| crate::model::UserProfileRow {
            user_id: UserId(r.get("user_id")),
            server_id: ServerId(r.get("server_id")),
            display_name: r.get("display_name"),
            description: r.get("description"),
            accent_color: r.get("accent_color"),
            custom_status_text: r.get("custom_status_text"),
            custom_status_emoji: r.get("custom_status_emoji"),
            custom_status_expires: r.get("custom_status_expires"),
            avatar_asset_url: r.get("avatar_asset_url"),
            banner_asset_url: r.get("banner_asset_url"),
            links: r.get("links"),
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
        }))
    }

    async fn set_profile_avatar(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
        avatar_url: &str,
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO user_profiles (user_id, server_id, avatar_asset_url, created_at, updated_at)
            VALUES ($1, $2, $3, NOW(), NOW())
            ON CONFLICT (user_id) DO UPDATE SET
                server_id = $2,
                avatar_asset_url = $3,
                updated_at = NOW()
            "#,
        )
        .bind(user_id.0)
        .bind(server_id.0)
        .bind(avatar_url)
        .execute(&mut **tx)
        .await
        .context("set profile avatar")?;
        Ok(())
    }

    async fn set_profile_banner(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
        banner_url: &str,
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO user_profiles (user_id, server_id, banner_asset_url, created_at, updated_at)
            VALUES ($1, $2, $3, NOW(), NOW())
            ON CONFLICT (user_id) DO UPDATE SET
                server_id = $2,
                banner_asset_url = $3,
                updated_at = NOW()
            "#,
        )
        .bind(user_id.0)
        .bind(server_id.0)
        .bind(banner_url)
        .execute(&mut **tx)
        .await
        .context("set profile banner")?;
        Ok(())
    }

    async fn create_asset_upload_session(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session_id: Uuid,
        user_id: UserId,
        server_id: ServerId,
        purpose: &str,
        mime_type: &str,
        byte_length: i64,
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO profile_asset_uploads
                (session_id, user_id, server_id, purpose, mime_type, byte_length, status, created_at, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6, 'pending', NOW(), NOW() + INTERVAL '10 minutes')
            "#,
        )
        .bind(session_id)
        .bind(user_id.0)
        .bind(server_id.0)
        .bind(purpose)
        .bind(mime_type)
        .bind(byte_length)
        .execute(&mut **tx)
        .await
        .context("create asset upload session")?;
        Ok(())
    }

    async fn store_verified_asset(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session_id: Uuid,
        asset_data: &[u8],
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            UPDATE profile_asset_uploads
            SET status = 'verified', asset_data = $2
            WHERE session_id = $1
            "#,
        )
        .bind(session_id)
        .bind(asset_data)
        .execute(&mut **tx)
        .await
        .context("store verified asset")?;
        Ok(())
    }

    async fn get_asset_upload_session(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session_id: Uuid,
        user_id: UserId,
    ) -> ControlResult<Option<crate::model::AssetUploadSession>> {
        let row = sqlx::query(
            r#"
            SELECT session_id, user_id, server_id, purpose, mime_type, byte_length, status, created_at, expires_at
            FROM profile_asset_uploads
            WHERE session_id = $1 AND user_id = $2 AND expires_at > NOW()
            "#,
        )
        .bind(session_id)
        .bind(user_id.0)
        .fetch_optional(&mut **tx)
        .await
        .context("get asset upload session")?;

        Ok(row.map(|r| crate::model::AssetUploadSession {
            session_id: r.get("session_id"),
            user_id: UserId(r.get("user_id")),
            server_id: ServerId(r.get("server_id")),
            purpose: r.get("purpose"),
            mime_type: r.get("mime_type"),
            byte_length: r.get("byte_length"),
            status: r.get("status"),
            created_at: r.get("created_at"),
            expires_at: r.get("expires_at"),
        }))
    }

    async fn create_default_profile(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
        display_name: &str,
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO user_profiles (user_id, server_id, display_name, created_at, updated_at)
            VALUES ($1, $2, $3, NOW(), NOW())
            ON CONFLICT (user_id) DO NOTHING
            "#,
        )
        .bind(user_id.0)
        .bind(server_id.0)
        .bind(display_name)
        .execute(&mut **tx)
        .await
        .context("create default profile")?;
        Ok(())
    }

    async fn get_user_badges(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
    ) -> ControlResult<Vec<crate::model::UserBadgeRow>> {
        let rows = sqlx::query(
            r#"
            SELECT bd.id AS badge_id, bd.label, bd.icon_url, bd.tooltip
            FROM user_badges ub
            JOIN badge_definitions bd ON bd.id = ub.badge_id AND bd.server_id = ub.server_id
            WHERE ub.user_id = $1 AND ub.server_id = $2
            ORDER BY bd.position ASC
            "#,
        )
        .bind(user_id.0)
        .bind(server_id.0)
        .fetch_all(&mut **tx)
        .await
        .context("get user badges")?;

        Ok(rows
            .into_iter()
            .map(|r| crate::model::UserBadgeRow {
                badge_id: r.get("badge_id"),
                label: r.get("label"),
                icon_url: r.get("icon_url"),
                tooltip: r.get("tooltip"),
            })
            .collect())
    }

    async fn create_badge_definition(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        badge: &crate::model::BadgeDefinitionRow,
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO badge_definitions (id, server_id, label, icon_url, tooltip, position, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, NOW())
            ON CONFLICT (id) DO UPDATE SET label = $3, icon_url = $4, tooltip = $5, position = $6
            "#,
        )
        .bind(&badge.id)
        .bind(badge.server_id.0)
        .bind(&badge.label)
        .bind(&badge.icon_url)
        .bind(&badge.tooltip)
        .bind(badge.position)
        .execute(&mut **tx)
        .await
        .context("create badge definition")?;
        Ok(())
    }

    async fn grant_badge(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        badge_id: &str,
        server_id: ServerId,
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            INSERT INTO user_badges (user_id, badge_id, server_id, granted_at)
            VALUES ($1, $2, $3, NOW())
            ON CONFLICT (user_id, badge_id) DO NOTHING
            "#,
        )
        .bind(user_id.0)
        .bind(badge_id)
        .bind(server_id.0)
        .execute(&mut **tx)
        .await
        .context("grant badge")?;
        Ok(())
    }

    async fn revoke_badge(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        badge_id: &str,
        server_id: ServerId,
    ) -> ControlResult<()> {
        sqlx::query(
            r#"
            DELETE FROM user_badges
            WHERE user_id = $1 AND badge_id = $2 AND server_id = $3
            "#,
        )
        .bind(user_id.0)
        .bind(badge_id)
        .bind(server_id.0)
        .execute(&mut **tx)
        .await
        .context("revoke badge")?;
        Ok(())
    }

    async fn get_user_roles_display(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        user_id: UserId,
        server_id: ServerId,
    ) -> ControlResult<Vec<crate::model::UserRoleRow>> {
        let rows = sqlx::query(
            r#"
            SELECT r.id AS role_id, r.name, r.color, r.position
            FROM user_roles ur
            JOIN roles r ON r.id = ur.role_id AND r.server_id = ur.server_id
            WHERE ur.user_id = $1 AND ur.server_id = $2
            ORDER BY r.position DESC
            "#,
        )
        .bind(user_id.0)
        .bind(server_id.0)
        .fetch_all(&mut **tx)
        .await
        .context("get user roles display")?;

        Ok(rows
            .into_iter()
            .map(|r| crate::model::UserRoleRow {
                role_id: r.get("role_id"),
                name: r.get("name"),
                color: r.get("color"),
                position: r.get("position"),
            })
            .collect())
    }

    async fn verify_asset_ownership(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        asset_id: &str,
        user_id: UserId,
    ) -> ControlResult<bool> {
        let parsed = asset_id.parse::<Uuid>();
        let Ok(asset_uuid) = parsed else {
            return Ok(false);
        };
        let exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS(
                SELECT 1
                FROM profile_asset_uploads
                WHERE session_id = $1 AND user_id = $2 AND status = 'verified'
            )
            "#,
        )
        .bind(asset_uuid)
        .bind(user_id.0)
        .fetch_one(&mut **tx)
        .await
        .context("verify asset ownership")?;
        Ok(exists)
    }
}
