use clap::Parser;

use crate::bootstrap::OwnerBootstrapPolicy;

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

    /// Optional user id (UUID) to force-assign the owner role on this server.
    #[arg(long)]
    pub bootstrap_owner_user_id: Option<String>,

    /// Owner bootstrap policy.
    #[arg(long, value_enum, default_value_t = default_owner_bootstrap_policy())]
    pub owner_bootstrap_policy: OwnerBootstrapPolicy,

    /// In dev only: delete orphaned user_roles rows that reference missing roles.
    #[arg(long, default_value_t = false)]
    pub dev_repair_orphan_user_roles: bool,

    /// Metrics scrape listen address.
    ///
    /// Defaults to loopback-only for safety. Set explicitly (e.g. 0.0.0.0:9100)
    /// to opt-in to remote scraping.
    #[arg(long, default_value = "127.0.0.1:9100")]
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

    /// Dev mode: accept dev token "dev" (NEVER enable in production)
    #[arg(long, default_value_t = default_dev_mode())]
    pub dev_mode: bool,

    /// Max concurrent connections (soft limit)
    #[arg(long, default_value_t = 10_000)]
    pub max_connections: usize,

    /// Interval in seconds between orphan upload file scans (0 = disabled)
    #[arg(long, default_value_t = 3600)]
    pub orphan_scan_interval_secs: u64,

    /// Quinn per-connection total bytes buffered for received-but-not-yet-consumed datagrams.
    ///
    /// In quinn 0.11 this also influences the peer-advertised max datagram frame size.
    /// We still enforce vp_voice::APP_MEDIA_MTU as the real accepted media MTU at the app
    /// layer. Larger values absorb microbursts but can mask stalls; start at 32KiB.
    #[arg(
        long = "quic-datagram-recv-buffer-bytes",
        env = "VP_QUIC_DG_RECV_BUF_BYTES",
        default_value_t = 32 * 1024
    )]
    pub quic_datagram_recv_buffer_bytes: usize,
}

fn default_dev_mode() -> bool {
    cfg!(debug_assertions)
}

fn default_owner_bootstrap_policy() -> OwnerBootstrapPolicy {
    if cfg!(debug_assertions) {
        OwnerBootstrapPolicy::FirstLoginWins
    } else {
        OwnerBootstrapPolicy::ConfigOnly
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use clap::Parser;

    #[test]
    fn quic_datagram_recv_buffer_default_is_32kib() {
        let cfg = Config::parse_from(["vp-gateway", "--database-url", "postgres://dummy"]);
        assert_eq!(cfg.quic_datagram_recv_buffer_bytes, 32 * 1024);
    }

    #[test]
    fn metrics_listen_default_is_loopback() {
        let cfg = Config::parse_from(["vp-gateway", "--database-url", "postgres://dummy"]);
        assert_eq!(cfg.metrics_listen, "127.0.0.1:9100");
    }
}
