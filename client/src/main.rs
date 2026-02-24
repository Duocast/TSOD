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
            self_muted.clone(),
            self_deafened.clone(),
            input_gain.clone(),
            output_gain.clone(),
            loopback_active.clone(),
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
    self_muted: Arc<AtomicBool>,
    self_deafened: Arc<AtomicBool>,
    input_gain: Arc<std::sync::atomic::AtomicU32>,
    output_gain: Arc<std::sync::atomic::AtomicU32>,
    loopback_active: Arc<AtomicBool>,
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
                        let _ =
                            tx_event.send(UiEvent::AppendLog(format!("[presence] {:?}", p.kind)));
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

    if let Some(ch) = cfg.channel_id.as_deref() {
        match dispatcher.join_channel(ch).await {
            Ok(()) => {
                let _ = tx_event.send(UiEvent::SetChannelName(ch.to_string()));
                let _ = tx_event.send(UiEvent::AppendLog(format!("[ctl] joined channel {ch}")));
            }
            Err(e) => {
                let _ = tx_event.send(UiEvent::AppendLog(format!("[ctl] join failed: {e:#}")));
            }
        }
    }

    let (voice_die_tx, mut voice_die_rx) = watch::channel::<bool>(false);

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
    let mut active_channel: Option<String> = cfg.channel_id.clone();

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
                                        author_id: cfg.display_name.clone(),
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
                                Ok(()) => {
                                    active_channel = Some(channel_id.clone());
                                    let _ = tx_event.send(UiEvent::SetChannelName(channel_id.clone()));
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
                        UiIntent::ApplySettings(ref settings) => {
                            // Apply all settings to the audio pipeline
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_noise_suppression(settings.noise_suppression);
                                d.set_agc(settings.agc_enabled);
                                d.set_vad_threshold(settings.vad_threshold);
                                d.set_agc_target(settings.agc_target_db);
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

        // Don't send voice when self-muted
        if self_muted.load(Ordering::Relaxed) {
            continue;
        }

        if push_to_talk && !ptt_active.load(Ordering::Relaxed) {
            continue;
        }

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
