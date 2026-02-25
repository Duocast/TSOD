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

    /// Extra Subject Alternative Names (SANs) for generated self-signed certs.
    /// Repeat the flag for multiple values (IP or DNS), e.g.:
    /// --tls-self-signed-san 192.168.1.220 --tls-self-signed-san 136.38.142.63
    #[arg(long = "tls-self-signed-san")]
    pub tls_self_signed_sans: Vec<String>,

    /// Postgres connection URL (required for control-plane operations)
    #[arg(long, env = "VP_DATABASE_URL")]
    pub database_url: String,

    /// Default server id (UUID string) for this gateway instance
    #[arg(long, default_value = "00000000-0000-0000-0000-0000000000aa")]
    pub default_server_id: String,

    /// HTTP listen address for uploads/downloads
    #[arg(long, env = "VP_UPLOAD_LISTEN", default_value = "0.0.0.0:8081")]
    pub upload_listen: String,

    /// Public base URL for uploaded file download links
    #[arg(
        long,
        env = "VP_UPLOAD_PUBLIC_BASE",
        default_value = "http://127.0.0.1:8081"
    )]
    pub upload_public_base: String,

    /// Disk directory for upload storage
    #[arg(long, env = "VP_UPLOAD_DIR", default_value = "./data/uploads")]
    pub upload_dir: String,

    /// Max allowed upload size in MB
    #[arg(long, env = "VP_UPLOAD_MAX_MB", default_value_t = 25)]
    pub upload_max_mb: u64,

    /// Allowed MIME types (comma-separated)
    #[arg(
        long,
        env = "VP_UPLOAD_ALLOWED_MIME",
        default_value = "image/png,image/jpeg,image/webp,image/gif,video/mp4,video/webm"
    )]
    pub upload_allowed_mime: String,

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
