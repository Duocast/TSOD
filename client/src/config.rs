use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(name = "vp-client", about = "TSOD voice platform client")]
pub struct Config {
    #[arg(long, env = "VP_SERVER", default_value = "127.0.0.1:4433")]
    pub server: String,

    #[arg(long, env = "VP_ALPN", default_value = "vp-control/1")]
    pub alpn: String,

    #[arg(long, env = "VP_DEV_TOKEN", default_value = "dev")]
    pub dev_token: String,

    /// Join this channel UUID on startup.
    #[arg(long)]
    pub channel_id: Option<String>,

    /// Enable push-to-talk (spacebar).
    #[arg(long, env = "VP_PUSH_TO_TALK", default_value_t = true)]
    pub push_to_talk: bool,

    /// TLS server name (SNI).
    #[arg(long, env = "VP_SERVER_NAME", default_value = "localhost")]
    pub server_name: String,

    /// Path to CA certificate PEM for server validation.
    /// If unset, uses insecure dev mode (accept any cert).
    #[arg(long, env = "VP_CA_CERT_PEM")]
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

impl Config {
    pub fn load() -> Self {
        let mut cfg = Self::parse();
        if cfg.ca_cert_pem.is_none() {
            cfg.ca_cert_pem = find_local_ca_cert();
        }
        cfg
    }
}

fn find_local_ca_cert() -> Option<String> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let mut candidates: Vec<PathBuf> = Vec::with_capacity(4);
    candidates.push(exe_dir.join("ca.crt"));
    candidates.push(exe_dir.join("ca.pem"));
    candidates.push(exe_dir.join("ca-cert.pem"));
    candidates.push(exe_dir.join("certs").join("ca.crt"));

    candidates
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().to_string())
}
