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

    /// Dev mode: accept dev token "dev"
    #[arg(long, default_value_t = true)]
    pub dev_mode: bool,

    /// Max concurrent connections (soft limit)
    #[arg(long, default_value_t = 10_000)]
    pub max_connections: usize,
}
