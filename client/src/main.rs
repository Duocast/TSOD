//! vp-client main (production-shaped)
//!
//! What this does:
//! - Starts metrics/logging
//! - Starts TUI (blocking) and an async app loop
//! - Connects to server over QUIC, runs Hello->Auth, optional Join
//! - Runs voice send loop (capture->Opus->QUIC DATAGRAM) gated by PTT
//! - Runs voice receive loop (DATAGRAM->jitter->Opus decode->playout)
//! - Handles graceful shutdown + reconnect with backoff
//!
//! NOTE on TLS (important):
//! - A truly production-ready client MUST validate the server certificate.
//! - If your current net/quic.rs accepts any cert, that is NOT production.
//! - This main.rs supports optional certificate pinning via env var VP_TLS_PIN_SHA256_HEX.
//!   If unset, it falls back to your existing quic::make_endpoint() behavior.
//!   Replace that path with OS roots / CA chain validation for real production.

mod app;
mod audio;
mod config;
mod net;
mod proto;
mod ui;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use config::Config;
use net::control::ControlClient;
use net::voice_datagram::{make_voice_datagram, VOICE_HDR_LEN, VOICE_VERSION};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::{sleep, Duration, Instant};
use tracing::{info, warn, Level};
use tracing_subscriber::EnvFilter;
use ui::{Tui, UiEvent, UiIntent, UiModel};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .init();

    let cfg = Config::parse();

    // UI channels:
    // - UI -> app intents
    // - app -> UI events
    let (tx_intent, mut rx_intent) = mpsc::channel::<UiIntent>(256);
    let (tx_event, rx_event) = mpsc::channel::<UiEvent>(1024);

    // Shared shutdown signal
    let running = Arc::new(AtomicBool::new(true));
    let (shutdown_tx, shutdown_rx) = watch::channel::<bool>(false);

    // Shared PTT state (UI toggles)
    let ptt_active = Arc::new(AtomicBool::new(!cfg.push_to_talk));

    // Spawn UI (blocking thread)
    let ui_running = running.clone();
    let ui_shutdown_tx = shutdown_tx.clone();
    let ui_handle = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut model = UiModel::default();
        model.title = "vp-client".into();
        model.ptt_enabled = cfg.push_to_talk;
        model.ptt_active = !cfg.push_to_talk;

        let tui = Tui::new(tx_intent, rx_event);
        let r = tui.run_blocking(model);

        // Ensure app stops when UI exits
        ui_running.store(false, Ordering::Relaxed);
        let _ = ui_shutdown_tx.send(true);
        r
    });

    // App task: handles intents, networking lifecycle, audio loops
    let app_handle = tokio::spawn(app_task(
        cfg.clone(),
        tx_event.clone(),
        &mut rx_intent,
        running.clone(),
        shutdown_rx,
        ptt_active.clone(),
    ));

    // Also exit on Ctrl-C
    tokio::select! {
        r = ui_handle => {
            // UI exited; propagate any errors
            if let Err(e) = r {
                warn!("ui task join error: {}", e);
            }
        }
        r = app_handle => {
            if let Err(e) = r {
                warn!("app task join error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c");
            running.store(false, Ordering::Relaxed);
            let _ = shutdown_tx.send(true);
        }
    }

    // Wait for tasks to finish cleanly
    let _ = ui_handle.await;
    let _ = app_handle.await;

    Ok(())
}

async fn app_task(
    cfg: Config,
    tx_event: mpsc::Sender<UiEvent>,
    rx_intent: &mut mpsc::Receiver<UiIntent>,
    running: Arc<AtomicBool>,
    mut shutdown_rx: watch::Receiver<bool>,
    ptt_active: Arc<AtomicBool>,
) -> Result<()> {
    let _ = tx_event.send(UiEvent::AppendLog(format!("[sys] starting, server={}", cfg.server))).await;

    // Audio constants (match server expectations)
    let sample_rate = 48_000u32;
    let channels = 1u16;
    let frame_ms = 20u32;

    // Audio pipeline shared state
    let codec = Arc::new(Mutex::new(audio::opus::OpusCodec::new(sample_rate, channels as u8)?));
    let capture = Arc::new(audio::capture::Capture::start(sample_rate, channels, frame_ms)?);
    let playout = Arc::new(audio::playout::Playout::start(sample_rate, channels)?);

    // Voice rx jitter buffer (single stream for now)
    let jitter = Arc::new(Mutex::new(audio::jitter::JitterBuffer::new(64)));

    // Determine channel route hash (must match server membership resolver strategy)
    // If channel_id isn't set, we still can connect and auth; voice won't route meaningfully.
    let channel_id_str = cfg.channel_id.clone().unwrap_or_default();
    let channel_route_hash = if !channel_id_str.is_empty() {
        stable_route_hash_u32(channel_id_str.as_bytes())
    } else {
        0
    };

    // Reconnect loop state
    let mut backoff = Backoff::new(Duration::from_millis(250), Duration::from_secs(10));

    // Main lifecycle loop: connect -> run until disconnect/shutdown -> reconnect
    while running.load(Ordering::Relaxed) && !*shutdown_rx.borrow() {
        match connect_and_run_session(
            &cfg,
            &tx_event,
            codec.clone(),
            capture.clone(),
            playout.clone(),
            jitter.clone(),
            channel_route_hash,
            ptt_active.clone(),
            &mut shutdown_rx,
            rx_intent,
        )
        .await
        {
            Ok(()) => {
                // clean disconnect
                backoff.reset();
            }
            Err(e) => {
                let _ = tx_event.send(UiEvent::AppendLog(format!("[net] disconnected: {e:#}"))).await;
                backoff.sleep().await;
            }
        }
    }

    let _ = tx_event.send(UiEvent::AppendLog("[sys] shutting down".into())).await;
    Ok(())
}

async fn connect_and_run_session(
    cfg: &Config,
    tx_event: &mpsc::Sender<UiEvent>,
    codec: Arc<Mutex<audio::opus::OpusCodec>>,
    capture: Arc<audio::capture::Capture>,
    playout: Arc<audio::playout::Playout>,
    jitter: Arc<Mutex<audio::jitter::JitterBuffer>>,
    channel_route_hash: u32,
    ptt_active: Arc<AtomicBool>,
    shutdown_rx: &mut watch::Receiver<bool>,
    rx_intent: &mut mpsc::Receiver<UiIntent>,
) -> Result<()> {
    let _ = tx_event.send(UiEvent::SetConnected(false)).await;
    let _ = tx_event.send(UiEvent::SetAuthed(false)).await;

    let endpoint = make_endpoint_with_optional_pinning()?;
    let addr = cfg.server.parse().context("parse server addr")?;

    let conn = endpoint
        .connect(addr, net::quic::server_name())
        .context("connect start")?
        .await
        .context("connect await")?;

    let _ = tx_event.send(UiEvent::SetConnected(true)).await;
    let _ = tx_event.send(UiEvent::AppendLog("[net] connected".into())).await;

    // Control stream
    let (send, recv) = conn.open_bi().await.context("open control stream")?;
    let mut ctl = ControlClient::new(send, recv);

    ctl.hello_and_auth(&cfg.alpn, &cfg.dev_token)
        .await
        .context("hello/auth")?;

    let _ = tx_event.send(UiEvent::SetAuthed(true)).await;
    let _ = tx_event.send(UiEvent::AppendLog("[net] authed".into())).await;

    if let Some(ch) = cfg.channel_id.as_deref() {
        // If server hasn't implemented JoinChannel yet, this may fail; treat as non-fatal for now
        match ctl.join_channel(ch).await {
            Ok(()) => {
                let _ = tx_event.send(UiEvent::SetChannelName(ch.to_string())).await;
                let _ = tx_event.send(UiEvent::AppendLog(format!("[ctl] joined channel {ch}"))).await;
            }
            Err(e) => {
                let _ = tx_event.send(UiEvent::AppendLog(format!("[ctl] join failed: {e:#}"))).await;
            }
        }
    }

    // Spawn voice send + receive tasks. If they die, session should be considered dead.
    let (voice_die_tx, mut voice_die_rx) = watch::channel::<bool>(false);

    let voice_send = tokio::spawn(voice_send_loop(
        conn.clone(),
        codec.clone(),
        capture.clone(),
        channel_route_hash,
        ptt_active.clone(),
        cfg.push_to_talk,
        voice_die_tx.clone(),
    ));

    let voice_recv = tokio::spawn(voice_recv_loop(
        conn.clone(),
        codec.clone(),
        playout.clone(),
        jitter.clone(),
        voice_die_tx.clone(),
    ));

    // Control keepalive task
    let ctl_keepalive = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            if let Err(e) = ctl.ping().await {
                return Err::<(), anyhow::Error>(e);
            }
        }
    });

    // Intent handling loop; for now we just handle Quit / PTT toggle / log chat.
    loop {
        tokio::select! {
            // UI intents
            maybe_intent = rx_intent.recv() => {
                match maybe_intent {
                    None => return Ok(()), // UI ended
                    Some(intent) => {
                        match intent {
                            UiIntent::Quit => return Ok(()),
                            UiIntent::TogglePtt => {
                                // UI already toggled model; we just reflect in capture gating
                                let cur = ptt_active.load(Ordering::Relaxed);
                                let _ = tx_event.send(UiEvent::AppendLog(format!("[ptt] {}", if cur { "ON" } else { "OFF" }))).await;
                            }
                            UiIntent::SendChat { text } => {
                                // If server chat isn't wired yet, just local log.
                                let _ = tx_event.send(UiEvent::AppendLog(format!("[me] {text}"))).await;
                                // TODO: send pb::SendMessageRequest via ControlClient once server implements.
                            }
                            UiIntent::SelectNextChannel | UiIntent::SelectPrevChannel => {
                                // TODO: implement channel list and JoinChannel intent.
                            }
                            UiIntent::Help => {
                                let _ = tx_event.send(UiEvent::AppendLog("[help] q quit | Enter send | Space PTT | Up/Down select".into())).await;
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Shutdown
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { return Ok(()); }
            }

            // Voice tasks died
            _ = voice_die_rx.changed() => {
                if *voice_die_rx.borrow() {
                    return Err(anyhow!("voice loop terminated"));
                }
            }

            // Control keepalive died
            r = &mut tokio::pin!(ctl_keepalive) => {
                return Err(anyhow!("control keepalive ended: {:?}", r));
            }
        }
    }

    // Cleanup is best-effort (tasks are tied to conn lifetime)
    // We don't reach here.
    #[allow(unreachable_code)]
    {
        let _ = voice_send.await;
        let _ = voice_recv.await;
        Ok(())
    }
}

async fn voice_send_loop(
    conn: quinn::Connection,
    codec: Arc<Mutex<audio::opus::OpusCodec>>,
    capture: Arc<audio::capture::Capture>,
    channel_route_hash: u32,
    ptt_active: Arc<AtomicBool>,
    push_to_talk: bool,
    voice_die_tx: watch::Sender<bool>,
) {
    let mut seq: u32 = 0;
    let ssrc: u32 = rand::random();

    // 20ms @ 48k mono = 960 samples
    let sample_rate = 48_000u32;
    let channels = 1usize;
    let frame_ms = 20u32;
    let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels;

    let mut pcm = vec![0i16; frame_samples];
    let mut enc_out = vec![0u8; 4000];

    let mut tick = tokio::time::interval(Duration::from_millis(5));

    loop {
        tick.tick().await;

        // Gate capture by PTT if enabled
        if push_to_talk && !ptt_active.load(Ordering::Relaxed) {
            continue;
        }

        if !capture.read_frame(&mut pcm) {
            continue;
        }

        // Encode
        let n = match codec.lock().await.encode(&pcm, &mut enc_out) {
            Ok(n) => n,
            Err(_) => continue,
        };

        let ts_ms = (unix_ms() & 0xFFFF_FFFF) as u32;

        let d = make_voice_datagram(
            channel_route_hash,
            ssrc,
            seq,
            ts_ms,
            true,
            &enc_out[..n],
        );
        seq = seq.wrapping_add(1);

        if let Err(_e) = conn.send_datagram(d) {
            let _ = voice_die_tx.send(true);
            return;
        }
    }
}

async fn voice_recv_loop(
    conn: quinn::Connection,
    codec: Arc<Mutex<audio::opus::OpusCodec>>,
    playout: Arc<audio::playout::Playout>,
    jitter: Arc<Mutex<audio::jitter::JitterBuffer>>,
    voice_die_tx: watch::Sender<bool>,
) {
    // Decode buffer: 20ms @ 48k mono
    let sample_rate = 48_000u32;
    let channels = 1usize;
    let frame_ms = 20u32;
    let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels;

    let mut pcm_out = vec![0i16; frame_samples];

    loop {
        let d = match conn.read_datagram().await {
            Ok(d) => d,
            Err(_e) => {
                let _ = voice_die_tx.send(true);
                return;
            }
        };

        // Parse header and extract payload
        let (seq, payload) = match parse_voice_payload(&d) {
            Some(v) => v,
            None => continue,
        };

        // Push into jitter buffer
        jitter.lock().await.push(seq, payload.to_vec());

        // Pop any ready frames and decode
        while let Some(frame) = jitter.lock().await.pop_ready() {
            let n = match codec.lock().await.decode(&frame, &mut pcm_out) {
                Ok(n) => n,
                Err(_) => continue,
            };

            if n > 0 {
                playout.push_pcm(&pcm_out[..n]);
            }
        }
    }
}

/// Parse voice datagram used by server forwarder:
/// returns (seq, payload_slice)
fn parse_voice_payload(d: &Bytes) -> Option<(u32, &[u8])> {
    if d.len() < VOICE_HDR_LEN {
        return None;
    }
    if d[0] != VOICE_VERSION {
        return None;
    }
    let hdr_len = u16::from_be_bytes([d[2], d[3]]) as usize;
    if hdr_len != VOICE_HDR_LEN || d.len() <= hdr_len {
        return None;
    }
    // seq at bytes 12..16
    let seq = u32::from_be_bytes([d[12], d[13], d[14], d[15]]);
    Some((seq, &d[hdr_len..]))
}

/// Stable 32-bit route hash (FNV-1a) for channel routing key.
fn stable_route_hash_u32(bytes: &[u8]) -> u32 {
    const FNV_OFFSET: u32 = 0x811C9DC5;
    const FNV_PRIME: u32 = 0x01000193;

    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

fn unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Basic exponential backoff with jitter.
struct Backoff {
    min: Duration,
    max: Duration,
    cur: Duration,
}

impl Backoff {
    fn new(min: Duration, max: Duration) -> Self {
        Self { min, max, cur: min }
    }
    fn reset(&mut self) {
        self.cur = self.min;
    }
    async fn sleep(&mut self) {
        let jitter = rand::random::<u64>() % 150;
        sleep(self.cur + Duration::from_millis(jitter)).await;
        self.cur = (self.cur * 2).min(self.max);
    }
}

/// If env var VP_TLS_PIN_SHA256_HEX is set, install a QUIC endpoint with cert pinning.
/// Otherwise fall back to your existing net::quic::make_endpoint() (which may be insecure in dev).
fn make_endpoint_with_optional_pinning() -> Result<quinn::Endpoint> {
    if let Ok(pin_hex) = std::env::var("VP_TLS_PIN_SHA256_HEX") {
        let pin = hex_to_32(&pin_hex).context("bad VP_TLS_PIN_SHA256_HEX (need 64 hex chars)")?;
        return make_pinned_endpoint(pin);
    }

    // Fallback: use existing helper (may accept any cert depending on your net/quic.rs).
    net::quic::make_endpoint()
}

/// Pinned cert endpoint using rustls "dangerous" verifier.
/// This is production-acceptable if you pin the server leaf cert hash out-of-band.
fn make_pinned_endpoint(pin_sha256: [u8; 32]) -> Result<quinn::Endpoint> {
    use quinn::{ClientConfig, Endpoint};
    use rustls::{client::danger::ServerCertVerifier, RootCertStore};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use std::{net::SocketAddr, sync::Arc};

    struct Pinner {
        pin: [u8; 32],
    }

    impl ServerCertVerifier for Pinner {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            // SHA-256 of DER
            let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
            if digest.as_ref() == self.pin {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General("cert pin mismatch".into()))
            }
        }
    }

    // NOTE: We still use dangerous() because rustls doesn’t provide pinning via safe builder.
    // In pinning mode, we intentionally bypass normal chain validation.
    let crypto = rustls::ClientConfig::builder()
        .with_root_certificates(RootCertStore::empty())
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinner { pin: pin_sha256 }))
        .with_no_client_auth();

    let mut endpoint = Endpoint::client("[::]:0".parse::<SocketAddr>()?)?;
    endpoint.set_default_client_config(ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    )));
    Ok(endpoint)
}

fn hex_to_32(s: &str) -> Result<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return Err(anyhow!("expected 64 hex chars, got {}", s.len()));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| anyhow!("invalid hex at byte {}", i))?;
        out[i] = b;
    }
    Ok(out)
}
