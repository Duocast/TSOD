use anyhow::Result;
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;
use vp_control::{
    ids::{ChannelId, ServerId, UserId},
    model::Member,
    ControlError, ControlRepo, ControlService, PgControlRepo, RequestContext,
};

async fn setup_service() -> Result<(
    PgPool,
    ControlService<PgControlRepo>,
    RequestContext,
    ChannelId,
    UserId,
)> {
    let Ok(url) = std::env::var("VP_DATABASE_URL") else {
        anyhow::bail!("VP_DATABASE_URL not set");
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;

    let repo = PgControlRepo::new(pool.clone());
    let service = ControlService::new(repo.clone());

    let server_id = ServerId(Uuid::new_v4());
    let actor = UserId(Uuid::new_v4());
    let target = UserId(Uuid::new_v4());
    let channel = ChannelId(Uuid::new_v4());

    sqlx::query(
        "INSERT INTO roles (id, server_id, name, position, is_everyone) VALUES ($1,$2,$3,$4,TRUE)",
    )
    .bind("everyone")
    .bind(server_id.0)
    .bind("@everyone")
    .bind(0_i32)
    .execute(&pool)
    .await?;

    sqlx::query(r#"INSERT INTO channels (id, server_id, name, created_at, updated_at) VALUES ($1,$2,$3,NOW(),NOW())"#)
        .bind(channel.0)
        .bind(server_id.0)
        .bind("voice")
        .execute(&pool)
        .await?;

    let mut tx = repo.tx().await?;
    repo.upsert_member(
        &mut tx,
        server_id,
        &Member {
            channel_id: channel,
            user_id: target,
            display_name: "target".to_string(),
            muted: false,
            deafened: false,
            joined_at: Utc::now(),
        },
    )
    .await?;
    tx.commit().await?;

    Ok((
        pool,
        service,
        RequestContext {
            server_id,
            user_id: actor,
            is_admin: false,
        },
        channel,
        target,
    ))
}

#[tokio::test]
async fn unauthorized_mute_is_denied() -> Result<()> {
    let Ok((_pool, service, ctx, channel, target)) = setup_service().await else {
        return Ok(());
    };
    let err = service
        .set_voice_mute(&ctx, channel, target, true, None)
        .await
        .expect_err("mute should be denied");
    assert!(matches!(err, ControlError::PermissionDenied(_)));
    Ok(())
}

#[tokio::test]
async fn unauthorized_deafen_is_denied() -> Result<()> {
    let Ok((_pool, service, ctx, channel, target)) = setup_service().await else {
        return Ok(());
    };
    let err = service
        .set_voice_deafen(&ctx, channel, target, true, None)
        .await
        .expect_err("deafen should be denied");
    assert!(matches!(err, ControlError::PermissionDenied(_)));
    Ok(())
}

#[tokio::test]
async fn unauthorized_kick_is_denied() -> Result<()> {
    let Ok((_pool, service, ctx, channel, target)) = setup_service().await else {
        return Ok(());
    };
    let err = service
        .kick_member(&ctx, channel, target, None)
        .await
        .expect_err("kick should be denied");
    assert!(matches!(err, ControlError::PermissionDenied(_)));
    Ok(())
}
