use anyhow::{anyhow, Context, Result};
use sqlx::{Pool, Postgres, Row};

use crate::proto::voiceplatform::v1 as pb;

#[derive(Debug, Clone)]
pub struct AuthedIdentity {
    pub user_id: String,
    pub server_id: String,
    pub display_name: String,
    pub is_admin: bool,
}

#[async_trait::async_trait]
pub trait AuthProvider: Send + Sync + 'static {
    async fn authenticate(
        &self,
        req: &pb::AuthRequest,
        session_id: &str,
        auth_challenge: &[u8],
    ) -> Result<AuthedIdentity>;
}

#[derive(Debug, Clone)]
pub struct DeviceAuthProvider {
    pool: Pool<Postgres>,
    default_server_id: uuid::Uuid,
}

impl DeviceAuthProvider {
    pub fn new(pool: Pool<Postgres>, default_server_id: uuid::Uuid) -> Self {
        Self {
            pool,
            default_server_id,
        }
    }
}

#[async_trait::async_trait]
impl AuthProvider for DeviceAuthProvider {
    async fn authenticate(
        &self,
        req: &pb::AuthRequest,
        session_id: &str,
        auth_challenge: &[u8],
    ) -> Result<AuthedIdentity> {
        match req.method.as_ref() {
            Some(pb::auth_request::Method::Device(device)) => {
                let device_id = device
                    .device_id
                    .as_ref()
                    .map(|d| d.value.trim())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("missing device_id"))?;
                let parsed_device_id =
                    uuid::Uuid::parse_str(device_id).map_err(|_| anyhow!("invalid device_id"))?;
                if device.device_pubkey.len() != 32 {
                    return Err(anyhow!("invalid device_pubkey"));
                }
                if device.signature.len() != 64 {
                    return Err(anyhow!("invalid signature"));
                }

                let mut signed = Vec::with_capacity(auth_challenge.len() + session_id.len());
                signed.extend_from_slice(auth_challenge);
                signed.extend_from_slice(session_id.as_bytes());

                let verifier = ring::signature::UnparsedPublicKey::new(
                    &ring::signature::ED25519,
                    &device.device_pubkey,
                );
                verifier
                    .verify(&signed, &device.signature)
                    .map_err(|_| anyhow!("invalid device signature"))?;

                let user_id = lookup_or_create_user_for_device(
                    &self.pool,
                    self.default_server_id,
                    parsed_device_id,
                    &device.device_pubkey,
                )
                .await?;

                let is_admin = is_user_admin(&self.pool, self.default_server_id, user_id).await?;

                Ok(AuthedIdentity {
                    user_id: user_id.to_string(),
                    server_id: self.default_server_id.to_string(),
                    display_name: format!("guest-{}", &parsed_device_id.to_string()[..8]),
                    is_admin,
                })
            }
            _ => Err(anyhow!("unsupported auth method in device provider")),
        }
    }
}

async fn lookup_or_create_user_for_device(
    pool: &Pool<Postgres>,
    server_id: uuid::Uuid,
    device_id: uuid::Uuid,
    pubkey: &[u8],
) -> Result<uuid::Uuid> {
    let mut tx = pool.begin().await.context("begin device auth tx")?;

    ensure_server_rbac_defaults(&mut tx, server_id).await?;

    let existing = sqlx::query(
        r#"
        SELECT user_id, device_id, revoked_at
        FROM auth_devices
        WHERE pubkey = $1
        FOR UPDATE
        "#,
    )
    .bind(pubkey)
    .fetch_optional(&mut *tx)
    .await
    .context("lookup device by pubkey")?;

    if let Some(row) = existing {
        let existing_user_id: uuid::Uuid = row.try_get("user_id")?;
        let existing_device_id: uuid::Uuid = row.try_get("device_id")?;
        let revoked_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("revoked_at")?;
        if existing_device_id != device_id {
            return Err(anyhow!("device_id mismatch for registered key"));
        }
        if revoked_at.is_some() {
            return Err(anyhow!("device revoked"));
        }
        sqlx::query(
            r#"
            UPDATE auth_devices
            SET last_seen = now()
            WHERE pubkey = $1
            "#,
        )
        .bind(pubkey)
        .execute(&mut *tx)
        .await
        .context("update device last_seen")?;

        assign_default_member_role(&mut tx, server_id, existing_user_id).await?;

        tx.commit().await.context("commit existing device auth")?;
        return Ok(existing_user_id);
    }

    let user_id = uuid::Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO auth_users (user_id)
        VALUES ($1)
        "#,
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await
    .context("insert auth user")?;

    assign_default_member_role(&mut tx, server_id, user_id).await?;

    let insert_res = sqlx::query(
        r#"
        INSERT INTO auth_devices (device_id, user_id, pubkey)
        VALUES ($1, $2, $3)
        "#,
    )
    .bind(device_id)
    .bind(user_id)
    .bind(pubkey)
    .execute(&mut *tx)
    .await;

    if let Err(err) = insert_res {
        if let sqlx::Error::Database(db_err) = &err {
            if db_err.constraint() == Some("auth_devices_pkey") {
                return Err(anyhow!(
                    "device_id already registered to another device key"
                ));
            }
            if db_err.constraint() == Some("auth_devices_pubkey_key") {
                let row = sqlx::query(
                    r#"
                    SELECT user_id, device_id, revoked_at
                    FROM auth_devices
                    WHERE pubkey = $1
                    FOR UPDATE
                    "#,
                )
                .bind(pubkey)
                .fetch_one(&mut *tx)
                .await
                .context("lookup device by pubkey after conflict")?;
                let existing_user_id: uuid::Uuid = row.try_get("user_id")?;
                let existing_device_id: uuid::Uuid = row.try_get("device_id")?;
                let revoked_at: Option<chrono::DateTime<chrono::Utc>> =
                    row.try_get("revoked_at")?;
                if existing_device_id != device_id {
                    return Err(anyhow!("device_id mismatch for registered key"));
                }
                if revoked_at.is_some() {
                    return Err(anyhow!("device revoked"));
                }
                sqlx::query(
                    r#"
                    UPDATE auth_devices
                    SET last_seen = now()
                    WHERE pubkey = $1
                    "#,
                )
                .bind(pubkey)
                .execute(&mut *tx)
                .await
                .context("update device last_seen after conflict")?;
                assign_default_member_role(&mut tx, server_id, existing_user_id).await?;
                tx.commit().await.context("commit conflict device auth")?;
                return Ok(existing_user_id);
            }
        }
        return Err(err).context("insert auth device")?;
    }

    assign_owner_role_if_missing(&mut tx, server_id, user_id).await?;

    tx.commit().await.context("commit new device auth")?;
    Ok(user_id)
}

async fn assign_owner_role_if_missing(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    server_id: uuid::Uuid,
    user_id: uuid::Uuid,
) -> Result<()> {
    let has_owner = sqlx::query(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM user_roles
            WHERE server_id = $1
              AND role_id = 'owner'
        ) AS has_owner
        "#,
    )
    .bind(server_id)
    .fetch_one(&mut **tx)
    .await
    .context("check existing owner role")?
    .try_get::<bool, _>("has_owner")?;

    if !has_owner {
        sqlx::query(
            r#"
            INSERT INTO user_roles (server_id, user_id, role_id)
            VALUES ($1, $2, 'owner')
            ON CONFLICT (server_id, user_id, role_id) DO NOTHING
            "#,
        )
        .bind(server_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .context("assign initial owner role")?;
    }

    Ok(())
}

async fn is_user_admin(
    pool: &Pool<Postgres>,
    server_id: uuid::Uuid,
    user_id: uuid::Uuid,
) -> Result<bool> {
    let row = sqlx::query(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM user_roles
            WHERE server_id = $1
              AND user_id = $2
              AND role_id = 'admin'
        ) AS is_admin
        "#,
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .context("lookup admin role")?;
    Ok(row.try_get::<bool, _>("is_admin")?)
}

async fn ensure_server_rbac_defaults(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    server_id: uuid::Uuid,
) -> Result<()> {
    for (idx, (role_id, role_name)) in [
        ("owner", "Owner"),
        ("admin", "Admin"),
        ("mod", "Moderator"),
        ("member", "@everyone"),
        ("muted", "Muted"),
    ]
    .iter()
    .enumerate()
    {
        sqlx::query(
            r#"
            INSERT INTO roles (id, server_id, name, color, position, is_system)
            VALUES ($1, $2, $3, 0, $4, FALSE)
            ON CONFLICT (id) DO NOTHING
            "#,
        )
        .bind(role_id)
        .bind(server_id)
        .bind(role_name)
        .bind(idx as i32)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("seed role {}", role_id))?;
    }

    for (role_id, cap, effect) in [
        ("member", "join_channel", "grant"),
        ("member", "speak", "grant"),
        ("member", "send_message", "grant"),
    ] {
        sqlx::query(
            r#"
            INSERT INTO role_caps (role_id, cap, effect, server_id, allowed)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (role_id, cap, effect) DO NOTHING
            "#,
        )
        .bind(role_id)
        .bind(cap)
        .bind(effect)
        .bind(server_id)
        .bind(effect == "grant")
        .execute(&mut **tx)
        .await
        .with_context(|| format!("seed role cap {} {} {}", role_id, cap, effect))?;
    }

    Ok(())
}

async fn assign_default_member_role(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    server_id: uuid::Uuid,
    user_id: uuid::Uuid,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO user_roles (server_id, user_id, role_id)
        VALUES ($1, $2, 'member')
        ON CONFLICT (server_id, user_id, role_id) DO NOTHING
        "#,
    )
    .bind(server_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await
    .context("assign default member role")?;

    Ok(())
}
