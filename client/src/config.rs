use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "vp-client", about = "TSOD voice platform client")]
pub struct Config {
    #[arg(long, default_value = "127.0.0.1:4433")]
    pub server: String,

    #[arg(long, default_value = "vp-control/1")]
    pub alpn: String,

    #[arg(long, default_value = "dev")]
    pub dev_token: String,

    /// Join this channel UUID on startup.
    #[arg(long)]
    pub channel_id: Option<String>,

    /// Enable push-to-talk (spacebar).
    #[arg(long, default_value_t = true)]
    pub push_to_talk: bool,

    /// TLS server name (SNI).
    #[arg(long, default_value = "localhost")]
    pub server_name: String,

    /// Path to CA certificate PEM for server validation.
    /// If unset, uses insecure dev mode (accept any cert).
    #[arg(long)]
    pub ca_cert_pem: Option<String>,

    /// Display name shown to other users.
    #[arg(long, default_value = "User")]
    pub display_name: String,

    /// Disable noise suppression (RNNoise).
    #[arg(long)]
    pub no_noise_suppression: bool,

    /// Disable automatic gain control.
    #[arg(long)]
    pub no_agc: bool,

    /// VAD threshold (0.0 = very sensitive, 1.0 = very strict).
    #[arg(long, default_value_t = 0.5)]
    pub vad_threshold: f32,
}
