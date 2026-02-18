use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "vp-gateway", about = "Voice platform QUIC gateway")]
pub struct Config {
    #[arg(long, default_value = "0.0.0.0:4433")]
    pub listen: String,

    #[arg(long, default_value = "vp-control/1")]
    pub alpn: String,

    #[arg(long)]
    pub tls_cert_pem: Option<String>,

    #[arg(long)]
    pub tls_key_pem: Option<String>,

    #[arg(long, default_value_t = true)]
    pub dev_mode: bool,

    #[arg(long, default_value_t = 10_000)]
    pub max_connections: usize,

    /// Postgres connection string used by vp-control
    #[arg(long, env = "VP_DATABASE_URL")]
    pub database_url: String,

    /// Logical server UUID (stable per deployment)
    #[arg(long, env = "VP_SERVER_ID")]
    pub server_id: String,
}
