use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
    Arc,
};

use crossbeam_channel::Sender;
use tokio::sync::watch;
use tracing::warn;

use crate::audio::opus::{OpusEncoder, OpusEncoderProfile};
use crate::media_codec::VideoSessionConfig;
use crate::net::egress::EgressScheduler;
use crate::net::overwrite_queue::OverwriteQueue;
use crate::net::video_convert::convert_frame;
use crate::net::video_encode::build_screen_encoder;
use crate::net::video_frame::{PixelFormat, VideoFrame};
use crate::net::video_transport::{VideoSender, VideoStreamProfile};
use crate::net::voice_datagram::make_voice_datagram;
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::config::SenderPolicy;
use crate::screen_share::policy::bitrate::BitrateController;
use crate::screen_share::runtime_probe::MediaRuntimeCaps;
use crate::ui::model::UiEvent;

use crate::{video_codec_name, ShareSource, VideoRuntimeCounters};

const CAPTURE_QUEUE_DEPTH: usize = 3;
const ENCODE_QUEUE_DEPTH: usize = 2;
const PACKETIZATION_QUEUE_DEPTH: usize = 2;

struct EncodedFrame {
    ts_ms: u32,
    is_keyframe: bool,
    data: bytes::Bytes,
}

#[derive(Clone)]
pub struct SessionStream {
    pub stream_tag: u64,
    pub codec: pb::VideoCodec,
}

pub struct StartLocalShareParams {
    pub source: ShareSource,
    pub include_audio: bool,
    pub streams: Vec<SessionStream>,
    pub negotiated_profile: VideoStreamProfile,
    pub mtu: usize,
    pub active_layer_id: Arc<AtomicU8>,
    pub force_next_keyframe: Arc<AtomicBool>,
    pub counters: Arc<VideoRuntimeCounters>,
    pub egress: Arc<EgressScheduler>,
    pub runtime_caps: Arc<MediaRuntimeCaps>,
    pub sender_policy: SenderPolicy,
    pub stop_rx: watch::Receiver<bool>,
    pub capture_queue_len_gauge: Arc<AtomicU64>,
    pub capture_queue_overflow_total: Arc<AtomicU64>,
    pub encode_queue_len_gauge: Arc<AtomicU64>,
    pub packetize_queue_len_gauge: Arc<AtomicU64>,
    pub backend_label: Arc<std::sync::Mutex<String>>,
    pub active_voice_channel_route: Arc<AtomicU32>,
    pub tx_event: Sender<UiEvent>,
}

fn profile_fps(profile: VideoStreamProfile) -> u32 {
    match profile {
        VideoStreamProfile::P1080p60 | VideoStreamProfile::P1440p60 => 60,
    }
}

pub async fn start_local_share(params: StartLocalShareParams) {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let capture_queue = Arc::new(OverwriteQueue::<VideoFrame>::new(CAPTURE_QUEUE_DEPTH));

    let mut stop_watch = params.stop_rx.clone();
    let stop_for_watch = stop_flag.clone();
    let capture_for_watch = capture_queue.clone();
    let stop_watch_task = tokio::spawn(async move {
        while stop_watch.changed().await.is_ok() {
            if *stop_watch.borrow() {
                stop_for_watch.store(true, Ordering::Relaxed);
                capture_for_watch.close();
                break;
            }
        }
    });

    let mut audio_worker: Option<tokio::task::JoinHandle<()>> = None;
    if params.include_audio {
        match crate::screen_share::audio::build_system_audio_backend(params.runtime_caps.as_ref()) {
            Ok(Some(mut backend)) => {
                let backend_name = backend.backend_name().to_string();
                match backend.start() {
                    Ok(()) => {
                        let _ = params
                            .tx_event
                            .send(UiEvent::SetScreenShareSystemAudioStatus {
                                available: true,
                                detail: format!("System audio: enabled ({backend_name})"),
                            });
                        let stop = stop_flag.clone();
                        let egress = params.egress.clone();
                        let active_route = params.active_voice_channel_route.clone();
                        audio_worker = Some(tokio::spawn(async move {
                            let mut encoder = match OpusEncoder::new(
                                48_000,
                                1,
                                OpusEncoderProfile::Music,
                            ) {
                                Ok(enc) => enc,
                                Err(e) => {
                                    warn!(error=?e, "[audio-share] failed to initialize opus encoder");
                                    backend.stop();
                                    return;
                                }
                            };
                            if let Err(e) = encoder.set_bitrate(64_000) {
                                warn!(error=?e, "[audio-share] failed to set opus bitrate");
                            }
                            let mut pcm = vec![0i16; 960];
                            let mut out = vec![0u8; 4000];
                            let ssrc: u32 = rand::random();
                            let mut seq = 0u32;
                            let mut ts_ms = 0u32;
                            while !stop.load(Ordering::Relaxed) {
                                if !backend.read_frame(&mut pcm) {
                                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                                    continue;
                                }
                                let Ok(n) = encoder.encode(&pcm, &mut out) else {
                                    continue;
                                };
                                let route = active_route.load(Ordering::Relaxed);
                                if route == 0 {
                                    continue;
                                }
                                let d =
                                    make_voice_datagram(route, ssrc, seq, ts_ms, true, &out[..n]);
                                if let Err(reason) = egress.enqueue_voice(d) {
                                    warn!(
                                        ?reason,
                                        "[audio-share] failed to enqueue system audio datagram"
                                    );
                                }
                                seq = seq.wrapping_add(1);
                                ts_ms = ts_ms.wrapping_add(20);
                            }
                            backend.stop();
                        }));
                    }
                    Err(e) => {
                        warn!(error=?e, "[audio-share] failed to start system audio backend; continuing video-only");
                        let _ = params
                            .tx_event
                            .send(UiEvent::SetScreenShareSystemAudioStatus {
                                available: false,
                                detail: format!("System audio failed: {e:#}. Sharing video-only."),
                            });
                    }
                }
            }
            Ok(None) => {
                let _ = params
                    .tx_event
                    .send(UiEvent::SetScreenShareSystemAudioStatus {
                        available: false,
                        detail: "System audio unavailable on this runtime. Sharing video-only."
                            .into(),
                    });
            }
            Err(e) => {
                warn!(error=?e, "[audio-share] system audio init failed; continuing video-only");
                let _ = params
                    .tx_event
                    .send(UiEvent::SetScreenShareSystemAudioStatus {
                        available: false,
                        detail: format!(
                            "System audio failed to initialize: {e:#}. Sharing video-only."
                        ),
                    });
            }
        }
    } else {
        let _ = params
            .tx_event
            .send(UiEvent::SetScreenShareSystemAudioStatus {
                available: true,
                detail: "System audio: disabled".into(),
            });
    }

    let capture_stop = stop_flag.clone();
    let capture_source = params.source.clone();
    let capture_caps = params.runtime_caps.clone();
    let capture_q = capture_queue.clone();
    let capture_len = params.capture_queue_len_gauge.clone();
    let capture_ovf = params.capture_queue_overflow_total.clone();
    let counters = params.counters.clone();
    let capture_task = tokio::task::spawn_blocking(move || {
        let mut cap = match crate::screen_share::capture::build_capture_backend(
            &capture_source,
            capture_caps.as_ref(),
        ) {
            Ok(cap) => cap,
            Err(e) => {
                warn!(error=?e, "[video] failed to build screen capture backend");
                return;
            }
        };

        while !capture_stop.load(Ordering::Relaxed) {
            match cap.next_frame() {
                Ok(frame) => {
                    capture_q.push(frame);
                    counters.capture_frames.fetch_add(1, Ordering::Relaxed);
                    capture_len.store(capture_q.len() as u64, Ordering::Relaxed);
                    counters
                        .queue_depth_capture
                        .store(capture_q.len() as u64, Ordering::Relaxed);
                    let ovf = capture_q.overflow_evictions_swap();
                    if ovf > 0 {
                        capture_ovf.fetch_add(ovf, Ordering::Relaxed);
                        counters
                            .capture_queue_overflows
                            .fetch_add(ovf, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    warn!(error=?e, "[video] capture frame failed");
                    break;
                }
            }
        }
        capture_q.close();
    });

    let mut stream_workers = Vec::new();
    for stream in params.streams {
        let mut sender = VideoSender::new(
            stream.stream_tag,
            params.active_layer_id.load(Ordering::Relaxed),
            params.negotiated_profile,
            params.mtu,
        );
        sender.set_pacing_enabled(false);

        let mut encoder = match build_screen_encoder(
            stream.codec,
            params.sender_policy,
            params.runtime_caps.as_ref(),
        ) {
            Ok(enc) => enc,
            Err(e) => {
                warn!(error=?e, stream_tag=stream.stream_tag, codec=%video_codec_name(stream.codec), "[video] failed to build screen encoder");
                continue;
            }
        };
        if let Ok(mut label) = params.backend_label.lock() {
            *label = encoder.backend_name().to_string();
        }

        let encode_queue = Arc::new(OverwriteQueue::<VideoFrame>::new(ENCODE_QUEUE_DEPTH));
        let packet_queue = Arc::new(OverwriteQueue::<EncodedFrame>::new(
            PACKETIZATION_QUEUE_DEPTH,
        ));

        let stream_capture_q = capture_queue.clone();
        let stream_encode_q = encode_queue.clone();
        let stop_for_fanout = stop_flag.clone();
        let counters = params.counters.clone();
        let fanout_task = tokio::spawn(async move {
            while !stop_for_fanout.load(Ordering::Relaxed) {
                let Some(frame) = stream_capture_q.pop_latest_or_wait().await else {
                    break;
                };
                stream_encode_q.push(frame);
                counters
                    .queue_depth_encode
                    .store(stream_encode_q.len() as u64, Ordering::Relaxed);
            }
            stream_encode_q.close();
        });

        let stream_encode_q = encode_queue.clone();
        let stream_packet_q = packet_queue.clone();
        let force_keyframe = params.force_next_keyframe.clone();
        let stream_tag = stream.stream_tag;
        let encode_depth = params.encode_queue_len_gauge.clone();
        let packet_depth = params.packetize_queue_len_gauge.clone();
        let counters = params.counters.clone();
        let negotiated_profile = params.negotiated_profile;
        let active_layer_id = params.active_layer_id.clone();
        let bitrate_controller =
            BitrateController::new(negotiated_profile, active_layer_id.load(Ordering::Relaxed));
        let target_pixel_format = if encoder.backend_name().contains("libvpx")
            || encoder.backend_name().contains("svt")
        {
            PixelFormat::I420
        } else {
            PixelFormat::Nv12
        };
        let encode_task = tokio::spawn(async move {
            let mut configured = false;
            let mut produced = 0u32;
            let mut last_fps_emit = std::time::Instant::now();
            while let Some(mut frame) = stream_encode_q.pop_latest_or_wait().await {
                encode_depth.store(stream_encode_q.len() as u64, Ordering::Relaxed);
                if frame.format != target_pixel_format {
                    match convert_frame(frame, target_pixel_format) {
                        Ok(converted) => frame = converted,
                        Err(e) => {
                            counters.encode_errors.fetch_add(1, Ordering::Relaxed);
                            warn!(error=?e, stream_tag, "[video] conversion failed");
                            continue;
                        }
                    }
                }
                if !configured {
                    if let Err(e) = encoder.configure_session(VideoSessionConfig {
                        width: frame.width,
                        height: frame.height,
                        fps: profile_fps(negotiated_profile),
                        target_bitrate_bps: bitrate_controller.current_target_bps(),
                        low_latency: true,
                        allow_frame_drop: true,
                    }) {
                        warn!(error=?e, stream_tag, "[video] failed to configure encoder session");
                        continue;
                    }
                    configured = true;
                }

                let layer_id = active_layer_id.load(Ordering::Relaxed);
                bitrate_controller.set_layer(layer_id);
                let target_bps = bitrate_controller
                    .apply_network_feedback(bitrate_controller.current_target_bps());
                let _ = encoder.update_bitrate(target_bps);

                if force_keyframe.swap(false, Ordering::Relaxed) {
                    let _ = encoder.request_keyframe();
                }

                match encoder.encode(frame) {
                    Ok(Some(encoded)) => {
                        produced = produced.saturating_add(1);
                        stream_packet_q.push(EncodedFrame {
                            ts_ms: encoded.ts_ms,
                            is_keyframe: encoded.is_keyframe,
                            data: encoded.data,
                        });
                        counters.encode_frames.fetch_add(1, Ordering::Relaxed);
                        packet_depth.store(stream_packet_q.len() as u64, Ordering::Relaxed);
                        counters
                            .queue_depth_packetize
                            .store(stream_packet_q.len() as u64, Ordering::Relaxed);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        counters.encode_errors.fetch_add(1, Ordering::Relaxed);
                        warn!(error=?e, stream_tag, "[video] encode failed")
                    }
                }

                let elapsed = last_fps_emit.elapsed().as_secs_f32();
                if elapsed >= 1.0 {
                    crate::net::dispatcher::report_runtime_encode_fps(produced as f32 / elapsed);
                    produced = 0;
                    last_fps_emit = std::time::Instant::now();
                }
            }
            if let Ok(flushed) = encoder.flush() {
                for encoded in flushed {
                    stream_packet_q.push(EncodedFrame {
                        ts_ms: encoded.ts_ms,
                        is_keyframe: encoded.is_keyframe,
                        data: encoded.data,
                    });
                }
            }
            stream_packet_q.close();
        });

        let stream_packet_q = packet_queue.clone();
        let egress = params.egress.clone();
        let counters = params.counters.clone();
        let send_task = tokio::spawn(async move {
            let mut frame_idx = 0_u32;
            while let Some(encoded) = stream_packet_q.pop_latest_or_wait().await {
                counters
                    .queue_depth_packetize
                    .store(stream_packet_q.len() as u64, Ordering::Relaxed);
                if let Err(e) = sender
                    .send_frame_async(encoded.ts_ms, encoded.is_keyframe, &encoded.data, |dg| {
                        match egress.enqueue_video_fragment(
                            stream_tag,
                            frame_idx,
                            encoded.is_keyframe,
                            std::time::Instant::now(),
                            dg,
                        ) {
                            Ok(report) => {
                                counters.video_tx_datagrams.fetch_add(1, Ordering::Relaxed);
                                if let Some(dropped) = report.dropped {
                                    counters
                                        .video_tx_drop_queue_full
                                        .fetch_add(dropped.count as u64, Ordering::Relaxed);
                                }
                            }
                            Err(reason) => {
                                counters
                                    .video_tx_drop_deadline
                                    .fetch_add(1, Ordering::Relaxed);
                                warn!(?reason, stream_tag, frame_idx, "[video] enqueue rejected");
                            }
                        }
                    })
                    .await
                {
                    counters.sender_frame_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(error=?e, stream_tag, frame_size=encoded.data.len(), "[video] send_frame failed");
                    break;
                }
                frame_idx = frame_idx.wrapping_add(1);
            }
        });

        stream_workers.push(fanout_task);
        stream_workers.push(encode_task);
        stream_workers.push(send_task);
    }

    for worker in stream_workers {
        let _ = worker.await;
    }
    if let Some(worker) = audio_worker {
        let _ = worker.await;
    }
    let _ = stop_watch_task.await;
    let _ = capture_task.await;
}
