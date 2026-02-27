use anyhow::{anyhow, Context, Result};
use sqlx::{Pool, Postgres, Row};

use crate::bootstrap::{
    ensure_baseline_role_assignment, ensure_core_roles, ensure_owner_exists, BootstrapConfig,
    OwnerBootstrapPolicy,
};
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
    bootstrap_owner_user_id: Option<uuid::Uuid>,
    owner_bootstrap_policy: OwnerBootstrapPolicy,
    dev_repair_orphan_user_roles: bool,
}

impl DeviceAuthProvider {
    pub fn new(
        pool: Pool<Postgres>,
        default_server_id: uuid::Uuid,
        bootstrap_owner_user_id: Option<uuid::Uuid>,
        owner_bootstrap_policy: OwnerBootstrapPolicy,
        dev_repair_orphan_user_roles: bool,
    ) -> Self {
        Self {
            pool,
            default_server_id,
            bootstrap_owner_user_id,
            owner_bootstrap_policy,
            dev_repair_orphan_user_roles,
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
                    BootstrapConfig {
                        bootstrap_owner_user_id: self.bootstrap_owner_user_id,
                        owner_bootstrap_policy: self.owner_bootstrap_policy,
                        dev_repair_orphan_user_roles: self.dev_repair_orphan_user_roles,
                    },
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
    bootstrap_cfg: BootstrapConfig,
    device_id: uuid::Uuid,
    pubkey: &[u8],
) -> Result<uuid::Uuid> {
    let mut tx = pool.begin().await.context("begin device auth tx")?;

    ensure_core_roles(&mut tx, server_id).await?;

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

        ensure_baseline_role_assignment(&mut tx, server_id, existing_user_id).await?;
        ensure_owner_exists(
            &mut tx,
            server_id,
            Some(existing_user_id),
            bootstrap_cfg.bootstrap_owner_user_id,
            bootstrap_cfg.owner_bootstrap_policy,
        )
        .await?;

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

    ensure_baseline_role_assignment(&mut tx, server_id, user_id).await?;

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
                ensure_baseline_role_assignment(&mut tx, server_id, existing_user_id).await?;
                ensure_owner_exists(
                    &mut tx,
                    server_id,
                    Some(existing_user_id),
                    bootstrap_cfg.bootstrap_owner_user_id,
                    bootstrap_cfg.owner_bootstrap_policy,
                )
                .await?;
                tx.commit().await.context("commit conflict device auth")?;
                return Ok(existing_user_id);
            }
        }
        return Err(err).context("insert auth device")?;
    }

    ensure_owner_exists(
        &mut tx,
        server_id,
        Some(user_id),
        bootstrap_cfg.bootstrap_owner_user_id,
        bootstrap_cfg.owner_bootstrap_policy,
    )
    .await?;

    tx.commit().await.context("commit new device auth")?;
    Ok(user_id)
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
              AND role_id IN ('admin', 'owner')
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
