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
    fn main() -> Result<()> {
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
        );
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
async fn app_task(
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

async fn connect_and_run_session(
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
async fn voice_send_loop(
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
