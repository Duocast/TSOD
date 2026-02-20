mod auth;
mod config;
mod frame;
mod gateway;
mod metrics_adapter;
mod outbox_dispatch;
mod state;
mod tls;

pub mod proto;

use anyhow::Result;
use clap::Parser;
use config::Config;
use gateway::Gateway;
use quinn::{Endpoint, ServerConfig, TransportConfig};
use rustls::ServerConfig as RustlsServerConfig;
use sqlx::postgres::PgPoolOptions;
use std::{net::SocketAddr, sync::Arc};
use tracing::{info, Level};
use tracing_subscriber::EnvFilter;
use vp_metrics::{MetricsConfig, MetricsServer};

use crate::auth::DevAuthProvider;
use crate::metrics_adapter::voice_metrics;
use crate::outbox_dispatch::{run_outbox_dispatcher, OutboxDispatcherConfig};
use crate::state::{MembershipCache, PushHub, Sessions};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .init();

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

    let repo = vp_control::PgControlRepo::new(pool.clone());
    let control = Arc::new(vp_control::ControlService::new(repo.clone()));

    // Shared runtime state
    let push = PushHub::new();
    let sessions = Sessions::new();
    let membership = MembershipCache::new();

    // Media forwarder
    let forwarder = Arc::new(vp_media::voice_forwarder::VoiceForwarder::new(
        vp_media::voice_forwarder::VoiceForwarderConfig::default(),
        Arc::new(sessions.clone()),
        Arc::new(membership.clone()),
        voice_metrics(),
    ));

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
    let (certs, key) =
        tls::load_or_generate_tls(cfg.tls_cert_pem.as_deref(), cfg.tls_key_pem.as_deref())?;

    let mut rustls = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    rustls.alpn_protocols = vec![cfg.alpn.as_bytes().to_vec()];

    let mut server_config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(rustls)?,
    ));

    let mut transport = TransportConfig::default();
    transport.max_concurrent_bidi_streams(64u32.into());
    transport.max_concurrent_uni_streams(64u32.into());
    transport.datagram_receive_buffer_size(Some(1024 * 1024));
    transport.datagram_send_buffer_size(1024 * 1024);
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(10)));
    server_config.transport_config(Arc::new(transport));

    let endpoint = Endpoint::server(server_config, addr)?;
    info!("listening on {}", endpoint.local_addr()?);

    let auth_provider: Arc<dyn auth::AuthProvider> = if cfg.dev_mode {
        Arc::new(DevAuthProvider)
    } else {
        Arc::new(DevAuthProvider)
    };

    let gw = Gateway::new(
        auth_provider,
        cfg.alpn,
        control,
        sessions,
        membership,
        forwarder,
    );

    tokio::select! {
        r = gw.serve(endpoint) => r?,
        _ = tokio::signal::ctrl_c() => {
            info!("shutdown");
        }
    }

    Ok(())
}
