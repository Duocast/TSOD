//! vp-client main — egui/eframe GUI + QUIC voice
//!
//! Architecture:
//! - eframe runs the GUI event loop on the main thread
//! - A tokio runtime runs in a background thread for networking + audio
//! - crossbeam channels bridge the GUI ↔ backend boundary
//! - DSP pipeline (RNNoise, AGC, VAD) processes audio before encoding

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
use crossbeam_channel::{bounded, Receiver, Sender};
use net::dispatcher::{ControlDispatcher, PushEvent};
use net::voice_datagram::{make_voice_datagram, VOICE_HDR_LEN, VOICE_VERSION};
use proto::voiceplatform::v1 as pb;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::{watch, Mutex};
use tokio::time::{sleep, Duration};
use tracing::{info, warn, Level};
use tracing_subscriber::EnvFilter;
use ui::{UiEvent, UiIntent, VpApp};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let cfg = Config::parse();

    // Channels between GUI and backend (crossbeam for sync/async bridging)
    let (tx_intent, rx_intent) = bounded::<UiIntent>(256);
    let (tx_event, rx_event) = bounded::<UiEvent>(1024);

    // Shared shutdown signal
    let running = Arc::new(AtomicBool::new(true));
    let (shutdown_tx, shutdown_rx) = watch::channel::<bool>(false);

    // PTT state
    let ptt_active = Arc::new(AtomicBool::new(!cfg.push_to_talk));

    // Start the tokio backend in a background thread
    let backend_cfg = cfg.clone();
    let backend_running = running.clone();
    let backend_tx_event = tx_event.clone();
    let backend_ptt = ptt_active.clone();

    let backend_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        rt.block_on(async {
            if let Err(e) = app_task(
                backend_cfg,
                backend_tx_event,
                rx_intent,
                backend_running,
                shutdown_rx,
                backend_ptt,
            )
            .await
            {
                warn!("backend error: {e:#}");
            }
        });
    });

    // Run the eframe GUI on the main thread
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TSOD")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };

    let gui_result = eframe::run_native(
        "TSOD",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(VpApp::new(cc, tx_intent, rx_event)))
        }),
    );

    // GUI exited — signal backend to shut down
    running.store(false, Ordering::Relaxed);
    let _ = shutdown_tx.send(true);

    // Wait for backend to finish
    let _ = backend_thread.join();

    gui_result.map_err(|e| anyhow!("eframe error: {e}"))
}

// ── Backend task ───────────────────────────────────────────────────────

async fn app_task(
    cfg: Config,
    tx_event: Sender<UiEvent>,
    rx_intent: Receiver<UiIntent>,
    running: Arc<AtomicBool>,
    mut shutdown_rx: watch::Receiver<bool>,
    ptt_active: Arc<AtomicBool>,
) -> Result<()> {
    let _ = tx_event.send(UiEvent::AppendLog(format!(
        "[sys] starting, server={}",
        cfg.server
    )));
    let _ = tx_event.send(UiEvent::SetNick(cfg.display_name.clone()));

    // Audio constants
    let sample_rate = 48_000u32;
    let channels = 1u16;
    let frame_ms = 20u32;

    // Audio pipeline
    let codec = Arc::new(Mutex::new(audio::opus::OpusCodec::new(
        sample_rate,
        channels as u8,
    )?));
    let capture = Arc::new(audio::capture::Capture::start(
        sample_rate, channels, frame_ms,
    )?);
    let playout = Arc::new(audio::playout::Playout::start(sample_rate, channels)?);
    let jitter = Arc::new(Mutex::new(audio::jitter::JitterBuffer::new(64)));

    // DSP pipeline
    let dsp_enabled = !cfg.no_noise_suppression;
    let capture_dsp = if dsp_enabled {
        Some(Arc::new(Mutex::new(
            audio::dsp::CaptureDsp::new(sample_rate)?,
        )))
    } else {
        None
    };

    if let Some(ref dsp) = capture_dsp {
        let mut d = dsp.lock().await;
        d.set_vad_threshold(cfg.vad_threshold);
    }

    let channel_id_str = cfg.channel_id.clone().unwrap_or_default();
    let channel_route_hash = if !channel_id_str.is_empty() {
        stable_route_hash_u32(channel_id_str.as_bytes())
    } else {
        0
    };

    let mut backoff = Backoff::new(Duration::from_millis(250), Duration::from_secs(10));

    while running.load(Ordering::Relaxed) && !*shutdown_rx.borrow() {
        match connect_and_run_session(
            &cfg,
            &tx_event,
            &rx_intent,
            codec.clone(),
            capture.clone(),
            playout.clone(),
            jitter.clone(),
            capture_dsp.clone(),
            channel_route_hash,
            ptt_active.clone(),
            &mut shutdown_rx,
        )
        .await
        {
            Ok(()) => {
                backoff.reset();
            }
            Err(e) => {
                let _ = tx_event.send(UiEvent::AppendLog(format!("[net] disconnected: {e:#}")));
                backoff.sleep().await;
            }
        }
    }

    let _ = tx_event.send(UiEvent::AppendLog("[sys] shutting down".into()));
    Ok(())
}

async fn connect_and_run_session(
    cfg: &Config,
    tx_event: &Sender<UiEvent>,
    rx_intent: &Receiver<UiIntent>,
    codec: Arc<Mutex<audio::opus::OpusCodec>>,
    capture: Arc<audio::capture::Capture>,
    playout: Arc<audio::playout::Playout>,
    jitter: Arc<Mutex<audio::jitter::JitterBuffer>>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    channel_route_hash: u32,
    ptt_active: Arc<AtomicBool>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let _ = tx_event.send(UiEvent::SetConnected(false));
    let _ = tx_event.send(UiEvent::SetAuthed(false));

    let endpoint = make_endpoint_with_optional_pinning(cfg)?;
    let addr = cfg.server.parse().context("parse server addr")?;

    let conn = endpoint
        .connect(addr, &cfg.server_name)
        .context("connect start")?
        .await
        .context("connect await")?;

    let _ = tx_event.send(UiEvent::SetConnected(true));
    let _ = tx_event.send(UiEvent::AppendLog("[net] connected".into()));

    let (send, recv) = conn.open_bi().await.context("open control stream")?;
    let dispatcher = ControlDispatcher::start(send, recv, shutdown_rx.clone());

    dispatcher
        .hello_auth(&cfg.alpn, &cfg.dev_token)
        .await
        .context("hello/auth")?;

    // Server push consumer
    let mut push_rx = dispatcher.take_push_receiver().await;
    {
        let tx_event = tx_event.clone();
        tokio::spawn(async move {
            while let Some(ev) = push_rx.recv().await {
                match ev {
                    PushEvent::Chat(c) => {
                        if let Some(kind) = c.kind {
                            match kind {
                                pb::chat_event::Kind::MessagePosted(mp) => {
                                    let author = mp
                                        .author_user_id
                                        .as_ref()
                                        .map(|u| u.value.as_str())
                                        .unwrap_or("unknown");
                                    let _ = tx_event.send(UiEvent::MessageReceived(
                                        ui::model::ChatMessage {
                                            message_id: mp
                                                .message_id
                                                .map(|m| m.value)
                                                .unwrap_or_default(),
                                            channel_id: mp
                                                .channel_id
                                                .map(|c| c.value)
                                                .unwrap_or_default(),
                                            author_id: author.to_string(),
                                            author_name: author.to_string(),
                                            text: mp.text.clone(),
                                            timestamp: mp
                                                .edited_at
                                                .as_ref()
                                                .map(|t| t.unix_millis)
                                                .unwrap_or(0),
                                            attachments: Vec::new(),
                                            reply_to: mp
                                                .reply_to_message_id
                                                .map(|r| r.value),
                                            reactions: Vec::new(),
                                            pinned: mp.pinned,
                                            edited: mp.edited_at.is_some(),
                                        },
                                    ));
                                }
                                pb::chat_event::Kind::MessageEdited(me) => {
                                    let _ = tx_event.send(UiEvent::MessageEdited {
                                        channel_id: me
                                            .channel_id
                                            .map(|c| c.value)
                                            .unwrap_or_default(),
                                        message_id: me
                                            .message_id
                                            .map(|m| m.value)
                                            .unwrap_or_default(),
                                        new_text: me.new_text,
                                    });
                                }
                                pb::chat_event::Kind::MessageDeleted(md) => {
                                    let _ = tx_event.send(UiEvent::MessageDeleted {
                                        channel_id: md
                                            .channel_id
                                            .map(|c| c.value)
                                            .unwrap_or_default(),
                                        message_id: md
                                            .message_id
                                            .map(|m| m.value)
                                            .unwrap_or_default(),
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                    PushEvent::Presence(p) => {
                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                            "[presence] {:?}",
                            p.kind
                        )));
                    }
                    PushEvent::Moderation(m) => {
                        let _ = tx_event
                            .send(UiEvent::AppendLog(format!("[moderation] {:?}", m)));
                    }
                    PushEvent::ServerHint(h) => {
                        let mut parts = vec![];
                        if h.receiver_report_interval_ms != 0 {
                            parts.push(format!("rr={}ms", h.receiver_report_interval_ms));
                        }
                        if h.max_stream_bitrate_bps != 0 {
                            parts.push(format!(
                                "stream_cap={}bps",
                                h.max_stream_bitrate_bps
                            ));
                        }
                        if h.max_voice_bitrate_bps != 0 {
                            parts.push(format!(
                                "voice_cap={}bps",
                                h.max_voice_bitrate_bps
                            ));
                        }
                        let msg = if parts.is_empty() {
                            "server_hint".into()
                        } else {
                            format!("server_hint {}", parts.join(" "))
                        };
                        let _ = tx_event.send(UiEvent::AppendLog(format!("[hint] {msg}")));
                    }
                    PushEvent::Unknown(_) => {}
                }
            }
        });
    }

    let _ = tx_event.send(UiEvent::SetAuthed(true));
    let _ = tx_event.send(UiEvent::AppendLog("[net] authed".into()));

    if let Some(ch) = cfg.channel_id.as_deref() {
        match dispatcher.join_channel(ch).await {
            Ok(()) => {
                let _ = tx_event.send(UiEvent::SetChannelName(ch.to_string()));
                let _ = tx_event
                    .send(UiEvent::AppendLog(format!("[ctl] joined channel {ch}")));
            }
            Err(e) => {
                let _ = tx_event
                    .send(UiEvent::AppendLog(format!("[ctl] join failed: {e:#}")));
            }
        }
    }

    let (voice_die_tx, mut voice_die_rx) = watch::channel::<bool>(false);

    let voice_send = tokio::spawn(voice_send_loop(
        conn.clone(),
        codec.clone(),
        capture.clone(),
        capture_dsp.clone(),
        tx_event.clone(),
        channel_route_hash,
        ptt_active.clone(),
        cfg.push_to_talk,
        voice_die_tx.clone(),
    ));

    let _voice_recv = tokio::spawn(voice_recv_loop(
        conn.clone(),
        codec.clone(),
        playout.clone(),
        jitter.clone(),
        voice_die_tx.clone(),
    ));

    let disp_keepalive = dispatcher.clone();
    let ctl_keepalive = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            if let Err(e) = disp_keepalive.ping().await {
                return Err::<(), anyhow::Error>(e);
            }
        }
    });

    tokio::pin!(ctl_keepalive);
    loop {
        tokio::select! {
            // Check for UI intents (non-blocking poll from crossbeam)
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                while let Ok(intent) = rx_intent.try_recv() {
                    match intent {
                        UiIntent::Quit => return Ok(()),
                        UiIntent::TogglePtt => {
                            let new = !ptt_active.load(Ordering::Relaxed);
                            ptt_active.store(new, Ordering::Relaxed);
                        }
                        UiIntent::PttDown => {
                            ptt_active.store(true, Ordering::Relaxed);
                        }
                        UiIntent::PttUp => {
                            ptt_active.store(false, Ordering::Relaxed);
                        }
                        UiIntent::SendChat { text } => {
                            let _ = tx_event.send(UiEvent::AppendLog(format!("[me] {text}")));
                            if let Some(ch) = cfg.channel_id.as_deref() {
                                if let Err(e) = dispatcher.send_chat(ch, &text).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] send_chat failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::JoinChannel { channel_id } => {
                            match dispatcher.join_channel(&channel_id).await {
                                Ok(()) => {
                                    let _ = tx_event.send(UiEvent::SetChannelName(channel_id));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] join failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::Help => {
                            let _ = tx_event.send(UiEvent::AppendLog(
                                "[help] Space=PTT | Enter=Send | Settings for audio config".into(),
                            ));
                        }
                        _ => {
                            // Other intents will be handled as features are wired up
                        }
                    }
                }
            }

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { return Ok(()); }
            }

            _ = voice_die_rx.changed() => {
                if *voice_die_rx.borrow() {
                    return Err(anyhow!("voice loop terminated"));
                }
            }

            r = &mut ctl_keepalive => {
                return Err(anyhow!("control keepalive ended: {:?}", r));
            }
        }
    }
}

async fn voice_send_loop(
    conn: quinn::Connection,
    codec: Arc<Mutex<audio::opus::OpusCodec>>,
    capture: Arc<audio::capture::Capture>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    tx_event: Sender<UiEvent>,
    channel_route_hash: u32,
    ptt_active: Arc<AtomicBool>,
    push_to_talk: bool,
    voice_die_tx: watch::Sender<bool>,
) {
    let mut seq: u32 = 0;
    let ssrc: u32 = rand::random();

    let sample_rate = 48_000u32;
    let channels = 1usize;
    let frame_ms = 20u32;
    let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels;

    let mut pcm = vec![0i16; frame_samples];
    let mut enc_out = vec![0u8; 4000];

    let mut tick = tokio::time::interval(Duration::from_millis(5));
    let mut vad_report_counter = 0u32;

    loop {
        tick.tick().await;

        if push_to_talk && !ptt_active.load(Ordering::Relaxed) {
            continue;
        }

        if !capture.read_frame(&mut pcm) {
            continue;
        }

        // Apply DSP pipeline (noise suppression + AGC + VAD)
        let mut is_voice = true;
        if let Some(ref dsp) = capture_dsp {
            let mut d = dsp.lock().await;
            d.process_frame(&mut pcm);
            is_voice = d.is_voice_active();

            // Report VAD level to GUI periodically
            vad_report_counter += 1;
            if vad_report_counter % 10 == 0 {
                let _ = tx_event.send(UiEvent::VadLevel(d.last_vad_probability()));
            }
        }

        // Skip sending if VAD says no voice (and not PTT mode)
        if !push_to_talk && !is_voice {
            continue;
        }

        let n = match codec.lock().await.encode(&pcm, &mut enc_out) {
            Ok(n) => n,
            Err(_) => continue,
        };

        let ts_ms = (unix_ms() & 0xFFFF_FFFF) as u32;

        let d = make_voice_datagram(channel_route_hash, ssrc, seq, ts_ms, is_voice, &enc_out[..n]);
        seq = seq.wrapping_add(1);

        if conn.send_datagram(d).is_err() {
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

        let (seq, payload) = match parse_voice_payload(&d) {
            Some(v) => v,
            None => continue,
        };

        jitter.lock().await.push(seq, payload.to_vec());

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
    let seq = u32::from_be_bytes([d[12], d[13], d[14], d[15]]);
    Some((seq, &d[hdr_len..]))
}

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

fn make_endpoint_with_optional_pinning(cfg: &Config) -> Result<quinn::Endpoint> {
    if let Ok(pin_hex) = std::env::var("VP_TLS_PIN_SHA256_HEX") {
        let pin = hex_to_32(&pin_hex)?;
        return make_pinned_endpoint(pin);
    }

    if let Some(ref ca_path) = cfg.ca_cert_pem {
        return net::quic::make_ca_endpoint(ca_path);
    }

    net::quic::make_endpoint()
}

fn make_pinned_endpoint(pin_sha256: [u8; 32]) -> Result<quinn::Endpoint> {
    use quinn::{ClientConfig, Endpoint};
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};
    use std::{net::SocketAddr, sync::Arc};

    #[derive(Debug)]
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
        ) -> std::result::Result<ServerCertVerified, rustls::Error> {
            let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
            if digest.as_ref() == self.pin {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General("cert pin mismatch".into()))
            }
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    let crypto = rustls::ClientConfig::builder()
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
