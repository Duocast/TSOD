use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "vp-gateway", about = "Voice platform QUIC gateway")]
pub struct Config {
    /// Address to bind, e.g. 0.0.0.0:4433
    #[arg(long, default_value = "0.0.0.0:4433")]
    pub listen: String,

    /// ALPN protocol to require
    #[arg(long, default_value = "vp-control/1")]
    pub alpn: String,

    /// Path to TLS cert PEM (optional; if unset, uses ephemeral self-signed cert)
    #[arg(long)]
    pub tls_cert_pem: Option<String>,

    /// Path to TLS key PEM (optional; required if tls_cert_pem is set)
    #[arg(long)]
    pub tls_key_pem: Option<String>,

    /// Postgres connection URL (required for control-plane operations)
    #[arg(long, env = "VP_DATABASE_URL")]
    pub database_url: String,

    /// Default server id (UUID string) for this gateway instance
    #[arg(long, default_value = "00000000-0000-0000-0000-0000000000aa")]
    pub default_server_id: String,

    /// Metrics scrape listen address
    #[arg(long, default_value = "0.0.0.0:9100")]
    pub metrics_listen: String,

    /// Outbox poll interval in milliseconds
    #[arg(long, default_value_t = 200)]
    pub outbox_poll_ms: u64,

    /// Maximum outbox records to claim per poll
    #[arg(long, default_value_t = 256)]
    pub outbox_batch: i64,

    /// Claim TTL seconds for outbox records (allows recovery if a gateway crashes)
    #[arg(long, default_value_t = 30)]
    pub outbox_claim_ttl_s: i64,

    /// Dev mode: accept dev token "dev"
    #[arg(long, default_value_t = true)]
    pub dev_mode: bool,

    /// Max concurrent connections (soft limit)
    #[arg(long, default_value_t = 10_000)]
    pub max_connections: usize,
}
