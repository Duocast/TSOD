use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "vp-client", about = "Voice platform client")]
pub struct Config {
    #[arg(long, default_value = "127.0.0.1:4433")]
    pub server: String,

    #[arg(long, default_value = "vp-control/1")]
    pub alpn: String,

    #[arg(long, default_value = "dev")]
    pub dev_token: String,

    /// Join this channel UUID (or you can create later)
    #[arg(long)]
    pub channel_id: Option<String>,

    /// Push-to-talk (spacebar in TUI later). For now toggles capture on/off.
    #[arg(long, default_value_t = true)]
    pub push_to_talk: bool,

    /// TLS server name (SNI). Defaults to "localhost".
    #[arg(long, default_value = "localhost")]
    pub server_name: String,

    /// Path to CA certificate PEM for server validation.
    /// If unset, uses insecure dev mode (accept any cert).
    
    /// Path to CA certificate PEM for server validation (optional; if unset, uses insecure dev mode)
    #[arg(long)]
    pub ca_cert_pem: Option<String>,
}
