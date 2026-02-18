mod auth;
mod config;
mod frame;
mod gateway;
mod tls;
mod state;

pub mod proto;

use anyhow::Result;
use clap::Parser;
use config::Config;
use gateway::Gateway;
use quinn::{Endpoint, ServerConfig, TransportConfig};
use rustls::ServerConfig as RustlsServerConfig;
use std::{net::SocketAddr, sync::Arc};
use tracing::{info, Level};
use tracing_subscriber::EnvFilter;

use crate::auth::DevAuthProvider;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .init();

    let cfg = Config::parse();
    let addr: SocketAddr = cfg.listen.parse()?;

    let (certs, key) = tls::load_or_generate_tls(cfg.tls_cert_pem.as_deref(), cfg.tls_key_pem.as_deref())?;

    let mut rustls = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    rustls.alpn_protocols = vec![cfg.alpn.as_bytes().to_vec()];

    let mut server_config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(rustls)?
    ));

    let mut transport = TransportConfig::default();
    transport.max_concurrent_bidi_streams(64u32.into());
    transport.max_concurrent_uni_streams(64u32.into());
    transport.datagram_receive_buffer_size(Some(1024 * 1024));
    transport.datagram_send_buffer_size(1024 * 1024);
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(10)));
    server_config.transport_config(Arc::new(transport));

    // ---- Control plane (Postgres) ----
    let pool = sqlx::PgPool::connect(&cfg.database_url).await?;
    let repo = vp_control::repo::PgControlRepo::new(pool);
    let control = vp_control::service::ControlService::new(repo);
    let state = Arc::new(crate::state::GatewayState::new(control));

    let endpoint = Endpoint::server(server_config, addr)?;
    info!("listening on {}", endpoint.local_addr()?);

    let auth_provider: Arc<dyn auth::AuthProvider> = if cfg.dev_mode {
        Arc::new(DevAuthProvider)
    } else {
        Arc::new(DevAuthProvider)
    };

    let gw = Gateway::new(auth_provider, cfg.alpn, state);
    tokio::select! {
        r = gw.serve(endpoint) => r?,
        _ = tokio::signal::ctrl_c() => info!("shutdown"),
    }

    Ok(())
}
