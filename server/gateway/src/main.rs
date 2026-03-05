mod auth;
mod bootstrap;
mod config;
mod egress;
mod frame;
mod gateway;
mod media;
mod metrics_adapter;
mod outbox_dispatch;
mod state;
mod tls;

pub mod proto;

use anyhow::Result;
use bootstrap::{ensure_core_state, BootstrapConfig};
use clap::Parser;
use config::Config;
use gateway::Gateway;
use quinn::{Endpoint, ServerConfig, TransportConfig};
use rustls::ServerConfig as RustlsServerConfig;
use sqlx::postgres::PgPoolOptions;
use std::{net::SocketAddr, sync::Arc};
use tokio::time::{Duration, MissedTickBehavior};
use tracing::{info, Level};
use tracing_subscriber::EnvFilter;
use vp_metrics::{MetricsConfig, MetricsServer};

use crate::auth::DeviceAuthProvider;
use crate::metrics_adapter::{stream_metrics, voice_metrics};
use crate::outbox_dispatch::{run_outbox_dispatcher, OutboxDispatcherConfig};
use crate::state::{MembershipCache, PushHub, Sessions, VoiceTelemetryCache};

const QUIC_DATAGRAM_RECV_BUFFER_SIZE: usize = vp_voice::QUIC_MAX_DATAGRAM_BYTES;
const QUIC_DATAGRAM_SEND_BUFFER_SIZE: usize = 128 * 1024; // keep explicit latency budget; avoid turning send buffer into hidden queue latency

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let cfg = Config::parse();
    let addr: SocketAddr = cfg.listen.parse()?;

    // Metrics
    let ms = MetricsServer::install(MetricsConfig {
        listen: cfg.metrics_listen.clone(),
        namespace: "vp",
    })?;
    tokio::spawn(async move {
        let _ = ms.serve().await;
    });

    // Postgres
    let pool = PgPoolOptions::new()
        .max_connections(32)
        .connect(&cfg.database_url)
        .await?;

    // Migrations (control-plane schema)
    sqlx::migrate!("../control/migrations").run(&pool).await?;

    let media = Arc::new(
        media::MediaService::new(
            pool.clone(),
            std::path::PathBuf::from("./data/uploads"),
            vp_control::ids::ServerId(uuid::Uuid::parse_str(&cfg.default_server_id)?),
        )
        .await?,
    );

    let repo = vp_control::PgControlRepo::new(pool.clone());
    let control = Arc::new(vp_control::ControlService::new(repo.clone()));

    // Shared runtime state
    let push = PushHub::new();
    let sessions = Sessions::new();
    let membership = MembershipCache::new();
    let telemetry = VoiceTelemetryCache::new();

    let (prune_tx, _prune_rx) = tokio::sync::mpsc::channel(1024);

    // Voice forwarder
    let forwarder = Arc::new(vp_media::voice_forwarder::VoiceForwarder::new(
        vp_media::voice_forwarder::VoiceForwarderConfig::default(),
        Arc::new(sessions.clone()),
        Arc::new(membership.clone()),
        voice_metrics(),
        prune_tx.clone(),
    ));

    // Video/screenshare stream forwarder (SFU)
    let stream_forwarder = Arc::new(vp_media::stream_forwarder::StreamForwarder::new(
        vp_media::stream_forwarder::StreamForwarderConfig::default(),
        Arc::new(sessions.clone()),
        Arc::new(membership.clone()),
        stream_metrics(),
    ));

    {
        let stream_forwarder = stream_forwarder.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                stream_forwarder
                    .cleanup_stale_viewers(Duration::from_secs(120))
                    .await;
            }
        });
    }

    // Outbox dispatcher (push fanout)
    let server_id = vp_control::ids::ServerId(uuid::Uuid::parse_str(&cfg.default_server_id)?);
    tokio::spawn(run_outbox_dispatcher(
        repo.clone(),
        push.clone(),
        membership.clone(),
        OutboxDispatcherConfig {
            server_id,
            poll_interval: std::time::Duration::from_millis(cfg.outbox_poll_ms),
            batch_size: cfg.outbox_batch,
            claim_ttl_seconds: cfg.outbox_claim_ttl_s,
        },
    ));

    // QUIC listener
    let (certs, key) = tls::load_or_generate_tls(
        cfg.tls_cert_pem.as_deref(),
        cfg.tls_key_pem.as_deref(),
        &cfg.tls_self_signed_sans,
    )?;

    let mut rustls = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    rustls.alpn_protocols = vec![cfg.alpn.as_bytes().to_vec()];
    info!(
        expected_alpn = %cfg.alpn,
        advertised_alpns = ?rustls
            .alpn_protocols
            .iter()
            .map(|p| String::from_utf8_lossy(p).to_string())
            .collect::<Vec<_>>(),
        "configured QUIC/TLS ALPN"
    );

    let mut server_config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(rustls)?,
    ));

    let mut transport = TransportConfig::default();
    transport.max_concurrent_bidi_streams(64u32.into());
    transport.max_concurrent_uni_streams(64u32.into());
    // In quinn 0.11, max_datagram_frame_size is advertised from datagram_receive_buffer_size.
    transport.datagram_receive_buffer_size(Some(QUIC_DATAGRAM_RECV_BUFFER_SIZE));
    transport.datagram_send_buffer_size(QUIC_DATAGRAM_SEND_BUFFER_SIZE);
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(10)));
    server_config.transport_config(Arc::new(transport));

    let endpoint = Endpoint::server(server_config, addr)?;
    info!("listening on {}", endpoint.local_addr()?);

    let bootstrap_owner_user_id = cfg
        .bootstrap_owner_user_id
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()?;

    ensure_core_state(
        &pool,
        server_id.0,
        None,
        BootstrapConfig {
            bootstrap_owner_user_id,
            owner_bootstrap_policy: cfg.owner_bootstrap_policy,
            dev_repair_orphan_user_roles: cfg.dev_repair_orphan_user_roles,
        },
    )
    .await?;

    let auth_provider: Arc<dyn auth::AuthProvider> = Arc::new(DeviceAuthProvider::new(
        pool.clone(),
        server_id.0,
        bootstrap_owner_user_id,
        cfg.owner_bootstrap_policy,
        cfg.dev_repair_orphan_user_roles,
    ));

    let gw = Gateway::new(
        auth_provider,
        cfg.alpn,
        control,
        sessions,
        push,
        membership,
        telemetry,
        forwarder,
        stream_forwarder,
        media,
        prune_tx,
    );

    tokio::select! {
        r = gw.serve(endpoint) => r?,
        _ = tokio::signal::ctrl_c() => {
            info!("shutdown");
        }
    }

    Ok(())
}
