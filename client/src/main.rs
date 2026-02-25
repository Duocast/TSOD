#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

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
mod settings_io;
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

    let cfg = Config::load();

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
        Box::new(move |cc| Ok(Box::new(VpApp::new(cc, tx_intent, rx_event)))),
    );

    // GUI exited — signal backend to shut down
    running.store(false, Ordering::Relaxed);
    let _ = shutdown_tx.send(true);

    // Do not block UI shutdown waiting for backend/network teardown.
    // Once the main thread returns, the process exits immediately.
    let _ = backend_thread;

    gui_result.map_err(|e| anyhow!("eframe error: {e}"))
}

// ── Backend task ───────────────────────────────────────────────────────

async fn app_task(
    mut cfg: Config,
    tx_event: Sender<UiEvent>,
    rx_intent: Receiver<UiIntent>,
    running: Arc<AtomicBool>,
    mut shutdown_rx: watch::Receiver<bool>,
    ptt_active: Arc<AtomicBool>,
) -> Result<()> {
    let _ = tx_event.send(UiEvent::AppendLog(format!(
        "[sys] starting, server={}, sni={}, ca_cert={}",
        cfg.server,
        cfg.server_name,
        cfg.ca_cert_pem.as_deref().unwrap_or("(insecure dev mode)")
    )));
    let _ = tx_event.send(UiEvent::SetNick(cfg.display_name.clone()));
    let (initial_host, initial_port) = split_server_host_port(&cfg.server);
    let _ = tx_event.send(UiEvent::SetServerAddress {
        host: initial_host,
        port: initial_port,
    });

    if cfg.server == "127.0.0.1:4433" || cfg.server == "localhost:4433" {
        let _ = tx_event.send(UiEvent::AppendLog(
            "[net] warning: using default server 127.0.0.1:4433; set --server or VP_SERVER for remote gateway".into(),
        ));
    }

    // Enumerate and report audio devices to the UI
    let input_devices = audio::capture::enumerate_input_devices();
    let output_devices = audio::playout::enumerate_output_devices();
    let _ = tx_event.send(UiEvent::SetAudioDevices {
        input_devices,
        output_devices,
    });

    // Load persisted settings and send to UI
    let saved_settings = settings_io::load_settings();
    let _ = tx_event.send(UiEvent::SettingsLoaded(Box::new(saved_settings.clone())));

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
        sample_rate,
        channels,
        frame_ms,
    )?);
    let playout = Arc::new(audio::playout::Playout::start(sample_rate, channels)?);
    let jitter = Arc::new(Mutex::new(audio::jitter::JitterBuffer::new(64)));

    // DSP pipeline
    let dsp_enabled = !cfg.no_noise_suppression;
    let capture_dsp = if dsp_enabled {
        Some(Arc::new(Mutex::new(audio::dsp::CaptureDsp::new(
            sample_rate,
        )?)))
    } else {
        None
    };

    if let Some(ref dsp) = capture_dsp {
        let mut d = dsp.lock().await;
        d.set_vad_threshold(cfg.vad_threshold);
        d.set_noise_suppression(saved_settings.noise_suppression);
        d.set_agc(saved_settings.agc_enabled);
        d.set_agc_target(saved_settings.agc_target_db);
        d.set_echo_cancellation(saved_settings.echo_cancellation);
    }

    let channel_id_str = cfg.channel_id.clone().unwrap_or_default();
    let channel_route_hash = if !channel_id_str.is_empty() {
        stable_route_hash_u32(channel_id_str.as_bytes())
    } else {
        0
    };

    // Shared self-mute/deafen state for the audio pipeline
    let self_muted = Arc::new(AtomicBool::new(false));
    let self_deafened = Arc::new(AtomicBool::new(false));

    // Shared gain values (stored as u32 bits of f32, default 1.0)
    let input_gain = Arc::new(std::sync::atomic::AtomicU32::new(f32_to_u32(1.0)));
    let output_gain = Arc::new(std::sync::atomic::AtomicU32::new(f32_to_u32(1.0)));
    let loopback_active = Arc::new(AtomicBool::new(false));
    let session_voice_active = Arc::new(AtomicBool::new(false));

    let _mic_test = tokio::spawn(mic_test_loop(
        capture.clone(),
        playout.clone(),
        tx_event.clone(),
        input_gain.clone(),
        loopback_active.clone(),
        session_voice_active.clone(),
        running.clone(),
        shutdown_rx.clone(),
    ));

    let mut backoff = Backoff::new(Duration::from_millis(250), Duration::from_secs(10));

    while running.load(Ordering::Relaxed) && !*shutdown_rx.borrow() {
        match connect_and_run_session(
            &mut cfg,
            &tx_event,
            &rx_intent,
            codec.clone(),
            capture.clone(),
            playout.clone(),
            jitter.clone(),
            capture_dsp.clone(),
            channel_route_hash,
            ptt_active.clone(),
            self_muted.clone(),
            self_deafened.clone(),
            input_gain.clone(),
            output_gain.clone(),
            loopback_active.clone(),
            session_voice_active.clone(),
            &mut shutdown_rx,
        )
        .await
        {
            Ok(()) => {
                backoff.reset();
            }
            Err(e) => {
                let _ = tx_event.send(UiEvent::AppendLog(format!("[net] disconnected: {e:#}")));

                let jitter = rand::random::<u64>() % 150;
                let wait_for = backoff.cur + Duration::from_millis(jitter);
                backoff.cur = (backoff.cur * 2).min(backoff.max);

                let deadline = tokio::time::Instant::now() + wait_for;
                'retry_wait: while tokio::time::Instant::now() < deadline {
                    while let Ok(intent) = rx_intent.try_recv() {
                        match intent {
                            UiIntent::Quit => return Ok(()),
                            UiIntent::ToggleLoopback => {
                                let new = !loopback_active.load(Ordering::Relaxed);
                                loopback_active.store(new, Ordering::Relaxed);
                                let _ = tx_event.send(UiEvent::SetLoopbackActive(new));
                                let _ = tx_event
                                    .send(UiEvent::AppendLog(format!("[audio] loopback: {new}")));
                            }
                            UiIntent::SetInputGain(gain) => {
                                input_gain.store(f32_to_u32(gain), Ordering::Relaxed);
                            }
                            UiIntent::SaveSettings(ref settings) => {
                                let _ = settings_io::save_settings(settings);
                            }
                            UiIntent::ConnectToServer { host, port } => {
                                cfg.server = format!("{host}:{port}");
                                cfg.server_name = host.clone();
                                let _ = tx_event.send(UiEvent::SetServerAddress { host, port });
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[net] target server updated: {}",
                                    cfg.server
                                )));
                                break 'retry_wait;
                            }
                            UiIntent::SetAwayMessage { message } => {
                                let _ = tx_event.send(UiEvent::SetAwayMessage(message.clone()));
                                let text = if message.trim().is_empty() {
                                    "[presence] away message cleared".to_string()
                                } else {
                                    format!("[presence] away message set: {message}")
                                };
                                let _ = tx_event.send(UiEvent::AppendLog(text));
                            }
                            _ => {}
                        }
                    }

                    if *shutdown_rx.borrow() {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
    }

    let _ = tx_event.send(UiEvent::AppendLog("[sys] shutting down".into()));
    Ok(())
}

fn split_server_host_port(server: &str) -> (String, u16) {
    if let Some((host, port_text)) = server.rsplit_once(':') {
        if let Ok(port) = port_text.parse::<u16>() {
            return (host.to_string(), port);
        }
    }
    (server.to_string(), 4433)
}

async fn connect_and_run_session(
    cfg: &mut Config,
    tx_event: &Sender<UiEvent>,
    rx_intent: &Receiver<UiIntent>,
    codec: Arc<Mutex<audio::opus::OpusCodec>>,
    capture: Arc<audio::capture::Capture>,
    playout: Arc<audio::playout::Playout>,
    jitter: Arc<Mutex<audio::jitter::JitterBuffer>>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    channel_route_hash: u32,
    ptt_active: Arc<AtomicBool>,
    self_muted: Arc<AtomicBool>,
    self_deafened: Arc<AtomicBool>,
    input_gain: Arc<std::sync::atomic::AtomicU32>,
    output_gain: Arc<std::sync::atomic::AtomicU32>,
    loopback_active: Arc<AtomicBool>,
    session_voice_active: Arc<AtomicBool>,
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

    let auth_info = dispatcher
        .hello_auth(&cfg.alpn, &cfg.dev_token)
        .await
        .context("hello/auth")?;

    if !auth_info.user_id.is_empty() {
        let _ = tx_event.send(UiEvent::SetUserId(auth_info.user_id.clone()));
    }

    let local_user_id = if auth_info.user_id.trim().is_empty() {
        cfg.display_name.clone()
    } else {
        auth_info.user_id.clone()
    };

    // Server push consumer
    let mut push_rx = dispatcher.take_push_receiver().await;
    {
        let tx_event = tx_event.clone();
        tokio::spawn(async move {
            while let Some(ev) = push_rx.recv().await {
                match ev {
                    PushEvent::Chat(c) => {
                        let event_at_millis = c.at.as_ref().map(|t| t.unix_millis);
                        if let Some(kind) = c.kind {
                            match kind {
                                pb::chat_event::Kind::MessagePosted(mp) => {
                                    let author_id = mp
                                        .author_user_id
                                        .as_ref()
                                        .map(|u| u.value.clone())
                                        .unwrap_or_default();
                                    let channel_id = mp
                                        .channel_id
                                        .as_ref()
                                        .map(|c| c.value.clone())
                                        .unwrap_or_default();
                                    let timestamp = event_at_millis.unwrap_or_else(|| {
                                        let missing = [
                                            ("message.author_user_id", author_id.is_empty()),
                                            ("chat_event.at", event_at_millis.is_none()),
                                        ]
                                        .into_iter()
                                        .filter_map(|(name, miss)| miss.then_some(name))
                                        .collect::<Vec<_>>();
                                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                                            "[chat] missing metadata for message_posted fields={}",
                                            missing.join(", ")
                                        )));
                                        unix_ms() as i64
                                    });

                                    let _ = tx_event.send(UiEvent::MessageReceived(
                                        ui::model::ChatMessage {
                                            message_id: mp
                                                .message_id
                                                .map(|m| m.value)
                                                .unwrap_or_default(),
                                            channel_id,
                                            author_name: String::new(),
                                            author_id,
                                            text: mp.text.clone(),
                                            timestamp,
                                            attachments: Vec::new(),
                                            reply_to: mp.reply_to_message_id.map(|r| r.value),
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
                        if let Some(kind) = p.kind {
                            match kind {
                                pb::presence_event::Kind::MemberJoined(mj) => {
                                    if let (Some(channel_id), Some(member)) =
                                        (mj.channel_id, mj.member)
                                    {
                                        let user_id = member
                                            .user_id
                                            .as_ref()
                                            .map(|u| u.value.clone())
                                            .unwrap_or_default();
                                        let _ = tx_event.send(UiEvent::MemberJoined {
                                            channel_id: channel_id.value,
                                            member: ui::model::MemberEntry {
                                                user_id,
                                                display_name: member.display_name,
                                                muted: member.muted,
                                                deafened: member.deafened,
                                                self_muted: member.self_muted,
                                                self_deafened: member.self_deafened,
                                                streaming: member.streaming,
                                                speaking: false,
                                                avatar_url: None,
                                            },
                                        });
                                    }
                                }
                                pb::presence_event::Kind::MemberLeft(ml) => {
                                    let _ = tx_event.send(UiEvent::MemberLeft {
                                        channel_id: ml
                                            .channel_id
                                            .map(|c| c.value)
                                            .unwrap_or_default(),
                                        user_id: ml.user_id.map(|u| u.value).unwrap_or_default(),
                                    });
                                }
                                other => {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[presence] {:?}",
                                        other
                                    )));
                                }
                            }
                        }
                    }
                    PushEvent::Moderation(m) => {
                        let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] {:?}", m)));
                    }
                    PushEvent::ServerHint(h) => {
                        let mut parts = vec![];
                        if h.receiver_report_interval_ms != 0 {
                            parts.push(format!("rr={}ms", h.receiver_report_interval_ms));
                        }
                        if h.max_stream_bitrate_bps != 0 {
                            parts.push(format!("stream_cap={}bps", h.max_stream_bitrate_bps));
                        }
                        if h.max_voice_bitrate_bps != 0 {
                            parts.push(format!("voice_cap={}bps", h.max_voice_bitrate_bps));
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

    let mut initial_active_channel: Option<String> = cfg.channel_id.clone();

    if let Some(ch) = cfg.channel_id.as_deref() {
        match dispatcher.join_channel(ch).await {
            Ok(state) => {
                let resolved_channel_id = state.channel_id.clone();
                let _ = tx_event.send(UiEvent::SetChannelName(resolved_channel_id.clone()));
                let _ = tx_event.send(UiEvent::UpdateChannelMembers {
                    channel_id: resolved_channel_id,
                    members: state
                        .members
                        .into_iter()
                        .map(|m| ui::model::MemberEntry {
                            user_id: m.user_id.map(|u| u.value).unwrap_or_default(),
                            display_name: m.display_name,
                            muted: m.muted,
                            deafened: m.deafened,
                            self_muted: m.self_muted,
                            self_deafened: m.self_deafened,
                            streaming: m.streaming,
                            speaking: false,
                            avatar_url: None,
                        })
                        .collect(),
                });
                initial_active_channel = Some(state.channel_id);
                let _ = tx_event.send(UiEvent::AppendLog(format!("[ctl] joined channel {ch}")));
            }
            Err(e) => {
                let _ = tx_event.send(UiEvent::AppendLog(format!("[ctl] join failed: {e:#}")));
            }
        }
    }

    let (voice_die_tx, mut voice_die_rx) = watch::channel::<bool>(false);
    let _session_voice_flag = SessionVoiceFlag::new(session_voice_active.clone());

    let _voice_send = tokio::spawn(voice_send_loop(
        conn.clone(),
        codec.clone(),
        capture.clone(),
        playout.clone(),
        capture_dsp.clone(),
        tx_event.clone(),
        channel_route_hash,
        ptt_active.clone(),
        self_muted.clone(),
        input_gain.clone(),
        loopback_active.clone(),
        cfg.push_to_talk,
        voice_die_tx.clone(),
    ));

    let _voice_recv = tokio::spawn(voice_recv_loop(
        conn.clone(),
        codec.clone(),
        playout.clone(),
        jitter.clone(),
        capture_dsp.clone(),
        self_deafened.clone(),
        output_gain.clone(),
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

    // Track the active channel (for SendChat and other channel-scoped operations)
    let mut active_channel: Option<String> = initial_active_channel;

    tokio::pin!(ctl_keepalive);
    loop {
        tokio::select! {
            // Check for UI intents (non-blocking poll from crossbeam)
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                while let Ok(intent) = rx_intent.try_recv() {
                    match intent {
                        UiIntent::Quit => return Ok(()),
                        UiIntent::ConnectToServer { host, port } => {
                            let new_server = format!("{host}:{port}");
                            if cfg.server != new_server {
                                cfg.server = new_server.clone();
                                cfg.server_name = host.clone();
                                let _ = tx_event.send(UiEvent::SetServerAddress { host, port });
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[net] reconnect requested: {new_server}"
                                )));
                                return Err(anyhow!("reconnect requested"));
                            }
                        }
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
                        UiIntent::ToggleSelfMute => {
                            let new = !self_muted.load(Ordering::Relaxed);
                            self_muted.store(new, Ordering::Relaxed);
                            let _ = tx_event.send(UiEvent::SetSelfMuted(new));
                        }
                        UiIntent::ToggleSelfDeafen => {
                            let new = !self_deafened.load(Ordering::Relaxed);
                            self_deafened.store(new, Ordering::Relaxed);
                            let _ = tx_event.send(UiEvent::SetSelfDeafened(new));
                        }
                        UiIntent::SendChat { text } => {
                            if let Some(ref ch) = active_channel {
                                // Optimistic local echo
                                let now_ms = unix_ms() as i64;
                                let _ = tx_event.send(UiEvent::MessageReceived(
                                    ui::model::ChatMessage {
                                        message_id: format!("local-{now_ms}"),
                                        channel_id: ch.clone(),
                                        author_id: local_user_id.clone(),
                                        author_name: cfg.display_name.clone(),
                                        text: text.clone(),
                                        timestamp: now_ms,
                                        attachments: Vec::new(),
                                        reply_to: None,
                                        reactions: Vec::new(),
                                        pinned: false,
                                        edited: false,
                                    },
                                ));
                                if let Err(e) = dispatcher.send_chat(ch, &text).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] send_chat failed: {e:#}"),
                                    ));
                                }
                            } else {
                                let _ = tx_event.send(UiEvent::AppendLog(
                                    "[ctl] no channel selected, cannot send message".into(),
                                ));
                            }
                        }
                        UiIntent::JoinChannel { channel_id } => {
                            match dispatcher.join_channel(&channel_id).await {
                                Ok(state) => {
                                    active_channel = Some(state.channel_id.clone());
                                    let _ = tx_event.send(UiEvent::SetChannelName(state.channel_id.clone()));
                                    let _ = tx_event.send(UiEvent::UpdateChannelMembers {
                                        channel_id: state.channel_id,
                                        members: state
                                            .members
                                            .into_iter()
                                            .map(|m| ui::model::MemberEntry {
                                                user_id: m.user_id.map(|u| u.value).unwrap_or_default(),
                                                display_name: m.display_name,
                                                muted: m.muted,
                                                deafened: m.deafened,
                                                self_muted: m.self_muted,
                                                self_deafened: m.self_deafened,
                                                streaming: m.streaming,
                                                speaking: false,
                                                avatar_url: None,
                                            })
                                            .collect(),
                                    });
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] joined channel {channel_id}"),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] join failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::LeaveChannel => {
                            if let Some(ref ch) = active_channel {
                                if let Err(e) = dispatcher.leave_channel(ch).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] leave failed: {e:#}"),
                                    ));
                                }
                            }
                            active_channel = None;
                        }
                        UiIntent::CreateChannel { name, description, channel_type, codec: _, quality, user_limit } => {
                            match dispatcher.create_channel(&name, &description, channel_type, quality * 1000, user_limit).await {
                                Ok(ch_id) => {
                                    let ch_type = if channel_type == 1 {
                                        ui::model::ChannelType::Text
                                    } else {
                                        ui::model::ChannelType::Voice
                                    };
                                    let _ = tx_event.send(UiEvent::ChannelCreated(
                                        ui::model::ChannelEntry {
                                            id: ch_id.clone(),
                                            name: name.clone(),
                                            channel_type: ch_type,
                                            parent_id: None,
                                            position: 0,
                                            member_count: 0,
                                            user_limit,
                                        },
                                    ));
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] created channel '{name}' ({ch_id})"),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] create_channel failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::Help => {
                            let _ = tx_event.send(UiEvent::AppendLog(
                                "[help] Space=PTT | Enter=Send | Settings for audio config".into(),
                            ));
                        }
                        UiIntent::SetNoiseSuppression(enabled) => {
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_noise_suppression(enabled);
                            }
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[dsp] noise suppression: {enabled}"),
                            ));
                        }
                        UiIntent::SetAgcEnabled(enabled) => {
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_agc(enabled);
                            }
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[dsp] AGC: {enabled}"),
                            ));
                        }
                        UiIntent::SetVadThreshold(threshold) => {
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_vad_threshold(threshold);
                            }
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[dsp] VAD threshold: {threshold:.2}"),
                            ));
                        }
                        UiIntent::SetInputGain(gain) => {
                            input_gain.store(f32_to_u32(gain), Ordering::Relaxed);
                        }
                        UiIntent::SetOutputGain(gain) => {
                            output_gain.store(f32_to_u32(gain), Ordering::Relaxed);
                        }
                        UiIntent::ToggleLoopback => {
                            let new = !loopback_active.load(Ordering::Relaxed);
                            loopback_active.store(new, Ordering::Relaxed);
                            let _ = tx_event.send(UiEvent::SetLoopbackActive(new));
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[audio] loopback: {new}"),
                            ));
                        }
                        UiIntent::SetInputDevice(dev) => {
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[audio] input device: {dev}"),
                            ));
                        }
                        UiIntent::SetOutputDevice(dev) => {
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[audio] output device: {dev}"),
                            ));
                        }
                        UiIntent::SetAwayMessage { message } => {
                            let _ = tx_event.send(UiEvent::SetAwayMessage(message.clone()));
                            match dispatcher.set_away_message(&message).await {
                                Ok(()) => {
                                    let text = if message.trim().is_empty() {
                                        "[presence] away message cleared".to_string()
                                    } else {
                                        format!("[presence] away message set: {message}")
                                    };
                                    let _ = tx_event.send(UiEvent::AppendLog(text));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[presence] set away message failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::SetAvatar { path } => {
                            match dispatcher.set_avatar(&path).await {
                                Ok(()) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        "[profile] avatar updated".to_string(),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[profile] set avatar failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::ApplySettings(ref settings) => {
                            // Apply all settings to the audio pipeline
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_noise_suppression(settings.noise_suppression);
                                d.set_agc(settings.agc_enabled);
                                d.set_vad_threshold(settings.vad_threshold);
                                d.set_agc_target(settings.agc_target_db);
                                d.set_echo_cancellation(settings.echo_cancellation);
                            }
                            input_gain.store(f32_to_u32(settings.input_gain), Ordering::Relaxed);
                            output_gain.store(f32_to_u32(settings.output_gain), Ordering::Relaxed);

                            // Update PTT mode
                            let is_ptt = settings.capture_mode == ui::model::CaptureMode::PushToTalk;
                            let is_continuous = settings.capture_mode == ui::model::CaptureMode::Continuous;
                            ptt_active.store(!is_ptt || is_continuous, Ordering::Relaxed);

                            let _ = tx_event.send(UiEvent::AppendLog(
                                "[settings] applied".into(),
                            ));
                        }
                        UiIntent::SaveSettings(ref settings) => {
                            if let Err(e) = settings_io::save_settings(settings) {
                                let _ = tx_event.send(UiEvent::AppendLog(
                                    format!("[settings] save failed: {e:#}"),
                                ));
                            }
                        }
                        _ => {
                            // Remaining intents (moderation, file upload, etc.)
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

async fn mic_test_loop(
    capture: Arc<audio::capture::Capture>,
    playout: Arc<audio::playout::Playout>,
    tx_event: Sender<UiEvent>,
    input_gain: Arc<std::sync::atomic::AtomicU32>,
    loopback_active: Arc<AtomicBool>,
    session_voice_active: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    shutdown_rx: watch::Receiver<bool>,
) {
    let sample_rate = 48_000u32;
    let channels = 1usize;
    let frame_ms = 20u32;
    let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels;

    let mut pcm = vec![0i16; frame_samples];
    let mut tick = tokio::time::interval(Duration::from_millis(10));

    loop {
        if !running.load(Ordering::Relaxed) || *shutdown_rx.borrow() {
            return;
        }
        tick.tick().await;

        if !loopback_active.load(Ordering::Relaxed) || session_voice_active.load(Ordering::Relaxed)
        {
            continue;
        }

        if !capture.read_frame(&mut pcm) {
            continue;
        }

        let gain = u32_to_f32(input_gain.load(Ordering::Relaxed));
        if (gain - 1.0).abs() > 0.001 {
            for s in pcm.iter_mut() {
                *s = (*s as f32 * gain).clamp(-32768.0, 32767.0) as i16;
            }
        }

        playout.push_pcm(&pcm);
        let waveform = build_mic_test_waveform(&pcm, 96);
        let _ = tx_event.send(UiEvent::MicTestWaveform(waveform));
    }
}

async fn voice_send_loop(
    conn: quinn::Connection,
    codec: Arc<Mutex<audio::opus::OpusCodec>>,
    capture: Arc<audio::capture::Capture>,
    playout: Arc<audio::playout::Playout>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    tx_event: Sender<UiEvent>,
    channel_route_hash: u32,
    ptt_active: Arc<AtomicBool>,
    self_muted: Arc<AtomicBool>,
    input_gain: Arc<std::sync::atomic::AtomicU32>,
    loopback_active: Arc<AtomicBool>,
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

        if !capture.read_frame(&mut pcm) {
            continue;
        }

        // Apply input gain
        let gain = u32_to_f32(input_gain.load(Ordering::Relaxed));
        if (gain - 1.0).abs() > 0.001 {
            for s in pcm.iter_mut() {
                *s = (*s as f32 * gain).clamp(-32768.0, 32767.0) as i16;
            }
        }

        // Loopback: feed capture directly to playout for mic testing
        if loopback_active.load(Ordering::Relaxed) {
            playout.push_pcm(&pcm);
            let waveform = build_mic_test_waveform(&pcm, 96);
            let _ = tx_event.send(UiEvent::MicTestWaveform(waveform));
        }

        let can_send = !self_muted.load(Ordering::Relaxed)
            && (!push_to_talk || ptt_active.load(Ordering::Relaxed));
        if !can_send {
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

        let d = make_voice_datagram(
            channel_route_hash,
            ssrc,
            seq,
            ts_ms,
            is_voice,
            &enc_out[..n],
        );
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
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    self_deafened: Arc<AtomicBool>,
    output_gain: Arc<std::sync::atomic::AtomicU32>,
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

        // Skip playout when self-deafened (still receive to keep connection alive)
        if self_deafened.load(Ordering::Relaxed) {
            continue;
        }

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
                // Apply output gain
                let gain = u32_to_f32(output_gain.load(Ordering::Relaxed));
                if (gain - 1.0).abs() > 0.001 {
                    for s in pcm_out[..n].iter_mut() {
                        *s = (*s as f32 * gain).clamp(-32768.0, 32767.0) as i16;
                    }
                }

                if let Some(ref dsp) = capture_dsp {
                    let mut d = dsp.lock().await;
                    d.feed_echo_reference(&pcm_out[..n]);
                }

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

fn build_mic_test_waveform(pcm: &[i16], points: usize) -> Vec<f32> {
    if pcm.is_empty() || points == 0 {
        return Vec::new();
    }

    let chunk = (pcm.len() / points.max(1)).max(1);
    let mut out = Vec::with_capacity(points);

    for i in 0..points {
        let start = i * chunk;
        if start >= pcm.len() {
            break;
        }

        let end = ((i + 1) * chunk).min(pcm.len());
        let peak = pcm[start..end]
            .iter()
            .map(|s| (*s as f32).abs() / 32768.0)
            .fold(0.0_f32, f32::max);
        out.push(peak);
    }

    out
}

fn f32_to_u32(f: f32) -> u32 {
    f.to_bits()
}

fn u32_to_f32(u: u32) -> f32 {
    f32::from_bits(u)
}

fn unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct SessionVoiceFlag(Arc<AtomicBool>);

impl SessionVoiceFlag {
    fn new(flag: Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::Relaxed);
        Self(flag)
    }
}

impl Drop for SessionVoiceFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
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
        return make_pinned_endpoint(pin, &cfg.alpn);
    }

    if let Some(ref ca_path) = cfg.ca_cert_pem {
        return net::quic::make_ca_endpoint(ca_path, &cfg.alpn);
    }

    net::quic::make_endpoint(&cfg.alpn)
}

fn make_pinned_endpoint(pin_sha256: [u8; 32], alpn: &str) -> Result<quinn::Endpoint> {
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

    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinner { pin: pin_sha256 }))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![alpn.as_bytes().to_vec()];

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
