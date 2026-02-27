use anyhow::{Context, Result};
use sqlx::{Pool, Postgres, Row, Transaction};
use tracing::{error, info, warn};

#[derive(Debug, Clone, Copy, Eq, PartialEq, clap::ValueEnum)]
pub enum OwnerBootstrapPolicy {
    FirstLoginWins,
    ConfigOnly,
}

#[derive(Debug, Clone, Copy)]
pub struct BootstrapConfig {
    pub bootstrap_owner_user_id: Option<uuid::Uuid>,
    pub owner_bootstrap_policy: OwnerBootstrapPolicy,
    pub dev_repair_orphan_user_roles: bool,
}

pub async fn ensure_core_state(
    pool: &Pool<Postgres>,
    server_id: uuid::Uuid,
    maybe_user_id_from_login: Option<uuid::Uuid>,
    cfg: BootstrapConfig,
) -> Result<()> {
    let mut tx = pool.begin().await.context("begin bootstrap tx")?;
    ensure_core_roles(&mut tx, server_id).await?;
    ensure_owner_exists(
        &mut tx,
        server_id,
        maybe_user_id_from_login,
        cfg.bootstrap_owner_user_id,
        cfg.owner_bootstrap_policy,
    )
    .await?;
    repair_orphaned_user_roles(&mut tx, server_id, cfg.dev_repair_orphan_user_roles).await?;
    tx.commit().await.context("commit bootstrap tx")?;

    info!(%server_id, "bootstrapped roles for server_id");
    Ok(())
}

pub async fn ensure_core_roles(
    tx: &mut Transaction<'_, Postgres>,
    server_id: uuid::Uuid,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO roles (id, server_id, name, role_position, is_everyone, color, position, is_system)
        VALUES
          ('everyone', $1, '@everyone', 0, true, 0, 0, true),
          ('member',   $1, 'Member',    10,false,0,10,true),
          ('admin',    $1, 'Admin',     90,false,0,90,true),
          ('owner',    $1, 'Owner',     100,false,0,100,true)
        ON CONFLICT (id) DO UPDATE
        SET server_id=EXCLUDED.server_id,
            name=EXCLUDED.name,
            role_position=EXCLUDED.role_position,
            is_everyone=EXCLUDED.is_everyone,
            color=EXCLUDED.color,
            position=EXCLUDED.position,
            is_system=EXCLUDED.is_system
        "#,
    )
    .bind(server_id)
    .execute(&mut **tx)
    .await
    .context("upsert core roles")?;

    ensure_baseline_capabilities(tx, server_id).await?;
    Ok(())
}

pub async fn ensure_baseline_role_assignment(
    tx: &mut Transaction<'_, Postgres>,
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
    .context("assign baseline member role")?;

    Ok(())
}

pub async fn ensure_owner_exists(
    tx: &mut Transaction<'_, Postgres>,
    server_id: uuid::Uuid,
    maybe_user_id_from_login: Option<uuid::Uuid>,
    bootstrap_owner_user_id_opt: Option<uuid::Uuid>,
    policy: OwnerBootstrapPolicy,
) -> Result<()> {
    let owner_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM user_roles WHERE server_id=$1 AND role_id='owner')",
    )
    .bind(server_id)
    .fetch_one(&mut **tx)
    .await
    .context("check whether owner exists")?;

    if owner_exists {
        return Ok(());
    }

    if let Some(bootstrap_owner_user_id) = bootstrap_owner_user_id_opt {
        sqlx::query(
            "INSERT INTO user_roles(server_id,user_id,role_id) VALUES ($1,$2,'owner') ON CONFLICT DO NOTHING",
        )
        .bind(server_id)
        .bind(bootstrap_owner_user_id)
        .execute(&mut **tx)
        .await
        .context("assign owner role to configured bootstrap owner")?;
        info!(%server_id, user_id=%bootstrap_owner_user_id, "assigned owner role from bootstrap_owner_user_id");
        return Ok(());
    }

    if policy == OwnerBootstrapPolicy::FirstLoginWins {
        if let Some(user_id) = maybe_user_id_from_login {
            sqlx::query(
                "INSERT INTO user_roles(server_id,user_id,role_id) VALUES ($1,$2,'owner') ON CONFLICT DO NOTHING",
            )
            .bind(server_id)
            .bind(user_id)
            .execute(&mut **tx)
            .await
            .context("assign first-login owner role")?;
            info!(%server_id, user_id=%user_id, "assigned owner role from first login policy");
            return Ok(());
        }
    }

    warn!(
        %server_id,
        ?policy,
        "no owner exists and no bootstrap_owner_user_id configured; owner was not auto-granted"
    );
    Ok(())
}

pub async fn repair_orphaned_user_roles(
    tx: &mut Transaction<'_, Postgres>,
    server_id: uuid::Uuid,
    dev_repair_orphan_user_roles: bool,
) -> Result<()> {
    let orphans = sqlx::query(
        r#"
        SELECT ur.user_id, ur.role_id
        FROM user_roles ur
        LEFT JOIN roles r ON r.id=ur.role_id AND r.server_id=ur.server_id
        WHERE ur.server_id=$1 AND r.id IS NULL
        LIMIT 50
        "#,
    )
    .bind(server_id)
    .fetch_all(&mut **tx)
    .await
    .context("check orphaned user_roles")?;

    if orphans.is_empty() {
        return Ok(());
    }

    let orphan_role_ids = orphans
        .iter()
        .map(|row| {
            format!(
                "{}:{}",
                row.get::<uuid::Uuid, _>("user_id"),
                row.get::<String, _>("role_id")
            )
        })
        .collect::<Vec<_>>();

    error!(
        %server_id,
        orphan_count = orphans.len(),
        orphan_user_roles = ?orphan_role_ids,
        "orphaned user_roles detected; run ensure_core_roles and verify role IDs/server IDs are consistent"
    );

    if dev_repair_orphan_user_roles {
        sqlx::query(
            r#"
            DELETE FROM user_roles ur
            WHERE ur.server_id=$1
              AND NOT EXISTS (
                SELECT 1
                FROM roles r
                WHERE r.id=ur.role_id
                  AND r.server_id=ur.server_id
              )
            "#,
        )
        .bind(server_id)
        .execute(&mut **tx)
        .await
        .context("delete orphaned user_roles")?;
        warn!(%server_id, "deleted orphaned user_roles due to dev_repair_orphan_user_roles=true");
    }

    Ok(())
}

async fn ensure_baseline_capabilities(
    tx: &mut Transaction<'_, Postgres>,
    server_id: uuid::Uuid,
) -> Result<()> {
    for (role_id, cap) in [
        ("everyone", "join_channel"),
        ("member", "send_message"),
        ("member", "speak"),
    ] {
        sqlx::query(
            r#"
            INSERT INTO role_caps (server_id, role_id, cap, allowed)
            VALUES ($1, $2, $3, TRUE)
            ON CONFLICT (server_id, role_id, cap) DO UPDATE
            SET allowed=TRUE
            "#,
        )
        .bind(server_id)
        .bind(role_id)
        .bind(cap)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("seed baseline capability {role_id}:{cap}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ensure_core_state, BootstrapConfig, OwnerBootstrapPolicy};
    use anyhow::Result;
    use sqlx::PgPool;
    use uuid::Uuid;

    #[tokio::test]
    async fn ensure_core_state_is_idempotent_when_database_is_available() -> Result<()> {
        let Ok(url) = std::env::var("VP_DATABASE_URL") else {
            return Ok(());
        };

        let pool = PgPool::connect(&url).await?;
        sqlx::migrate!("../control/migrations").run(&pool).await?;
        let server_id = Uuid::new_v4();

        let cfg = BootstrapConfig {
            bootstrap_owner_user_id: None,
            owner_bootstrap_policy: OwnerBootstrapPolicy::ConfigOnly,
            dev_repair_orphan_user_roles: false,
        };

        ensure_core_state(&pool, server_id, None, cfg).await?;
        ensure_core_state(&pool, server_id, None, cfg).await?;

        let role_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM roles WHERE server_id=$1")
            .bind(server_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(role_count, 4);
        Ok(())
    }

    #[tokio::test]
    async fn first_login_policy_assigns_owner() -> Result<()> {
        let Ok(url) = std::env::var("VP_DATABASE_URL") else {
            return Ok(());
        };

        let pool = PgPool::connect(&url).await?;
        sqlx::migrate!("../control/migrations").run(&pool).await?;
        let server_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();

        let cfg = BootstrapConfig {
            bootstrap_owner_user_id: None,
            owner_bootstrap_policy: OwnerBootstrapPolicy::FirstLoginWins,
            dev_repair_orphan_user_roles: false,
        };

        ensure_core_state(&pool, server_id, Some(user_id), cfg).await?;

        let owner_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM user_roles WHERE server_id=$1 AND user_id=$2 AND role_id='owner'",
        )
        .bind(server_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(owner_count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn config_only_without_bootstrap_owner_does_not_assign_owner() -> Result<()> {
        let Ok(url) = std::env::var("VP_DATABASE_URL") else {
            return Ok(());
        };

        let pool = PgPool::connect(&url).await?;
        sqlx::migrate!("../control/migrations").run(&pool).await?;
        let server_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();

        let cfg = BootstrapConfig {
            bootstrap_owner_user_id: None,
            owner_bootstrap_policy: OwnerBootstrapPolicy::ConfigOnly,
            dev_repair_orphan_user_roles: false,
        };

        ensure_core_state(&pool, server_id, Some(user_id), cfg).await?;

        let owner_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM user_roles WHERE server_id=$1 AND role_id='owner'",
        )
        .bind(server_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(owner_count, 0);
        Ok(())
    }
}
