use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::audio::opus::{OpusEncoder, OpusEncoderProfile};
use crate::media_codec::VideoSessionConfig;
use crate::net::egress::EgressScheduler;
use crate::net::overwrite_queue::OverwriteQueue;
use crate::net::video_encode::build_screen_encoder;
use crate::net::video_frame::{FramePlanes, VideoFrame};
use crate::net::video_transport::{VideoSender, VideoStreamProfile};
use crate::net::voice_datagram::make_voice_datagram;
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::config::SenderPolicy;
use crate::screen_share::policy::bitrate::bitrate_for_pressure;
use crate::screen_share::runtime_probe::MediaRuntimeCaps;
use crate::ui::model::UiEvent;

use crate::{video_codec_name, ShareSource, VideoRuntimeCounters};

const CAPTURE_QUEUE_DEPTH: usize = 3;
const ENCODE_QUEUE_DEPTH: usize = 2;
const PACKETIZATION_QUEUE_DEPTH: usize = 2;

/// Bitrate threshold above which sender-side pacing is enabled.
/// Layers below this threshold rely on the egress scheduler for pacing.
const PACING_BITRATE_THRESHOLD_BPS: u32 = 4_000_000;

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
    pub force_keyframe_generation: Arc<AtomicU64>,
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
    pub offered_layers: Vec<pb::SimulcastLayer>,
    pub accepted_layer_ids: Vec<u32>,
}

#[derive(Clone, Copy)]
struct LayerEncodingTarget {
    layer_id: u8,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
    profile: VideoStreamProfile,
}

#[derive(Clone, Copy, Debug)]
struct FramePacer {
    interval: Duration,
    next_deadline: Instant,
}

impl FramePacer {
    fn new(target_fps: u32) -> Self {
        let fps = target_fps.clamp(1, 60);
        let interval = Duration::from_secs_f64(1.0 / fps as f64);
        Self {
            interval,
            next_deadline: Instant::now() + interval,
        }
    }

    fn wait(&mut self) {
        let now = Instant::now();
        if self.next_deadline > now {
            std::thread::sleep(self.next_deadline - now);
            self.next_deadline += self.interval;
            return;
        }
        self.next_deadline = now + self.interval;
    }
}

#[derive(Debug, Clone, Copy)]
struct PressureSnapshot {
    encode_queue_len: usize,
    packet_queue_len: usize,
    dropped_video_fragments: u64,
}

#[derive(Debug, Clone)]
struct AdaptiveQualityController {
    base_bitrate_bps: u32,
    min_bitrate_bps: u32,
    base_fps: u32,
    min_fps: u32,
    max_encode_queue_len: usize,
    max_packet_queue_len: usize,
    last_drop_count: u64,
    current_bitrate_bps: u32,
    current_fps: u32,
}

impl AdaptiveQualityController {
    fn new(base_bitrate_bps: u32, base_fps: u32) -> Self {
        let base_fps = base_fps.clamp(5, 60);
        Self {
            base_bitrate_bps,
            min_bitrate_bps: (base_bitrate_bps / 3).max(600_000),
            base_fps,
            min_fps: (base_fps / 2).max(10),
            max_encode_queue_len: ENCODE_QUEUE_DEPTH,
            max_packet_queue_len: PACKETIZATION_QUEUE_DEPTH,
            last_drop_count: 0,
            current_bitrate_bps: base_bitrate_bps,
            current_fps: base_fps,
        }
    }

    fn evaluate(&mut self, snapshot: PressureSnapshot) -> (u32, u32) {
        let dropped_since_last = snapshot
            .dropped_video_fragments
            .saturating_sub(self.last_drop_count);
        self.last_drop_count = snapshot.dropped_video_fragments;

        let queue_pressure = snapshot.encode_queue_len >= self.max_encode_queue_len
            || snapshot.packet_queue_len >= self.max_packet_queue_len;
        let network_pressure = dropped_since_last > 0;

        let pressure_level = if network_pressure && queue_pressure {
            3
        } else if network_pressure || queue_pressure {
            2
        } else {
            0
        };

        let next_bitrate =
            bitrate_for_pressure(self.base_bitrate_bps, pressure_level, self.min_bitrate_bps);

        let next_fps = if pressure_level >= 3 {
            self.min_fps
        } else if pressure_level >= 2 {
            (self.base_fps * 2 / 3).max(self.min_fps)
        } else {
            self.base_fps
        };

        self.current_bitrate_bps = next_bitrate;
        self.current_fps = next_fps;
        (next_bitrate, next_fps)
    }
}

// ---------------------------------------------------------------------------
// Shared frame scaling
// ---------------------------------------------------------------------------

/// Resolution key for the scale cache: (width, height).
type ResolutionKey = (u32, u32);

/// A set of pre-scaled frames for all distinct target resolutions needed by
/// the current session.  Produced once per captured frame by the scale-fanout
/// stage and shared (via `Arc`) with every encoder that needs a given
/// resolution.
struct ScaledFrameSet {
    /// Scaled frames keyed by target resolution.
    frames: HashMap<ResolutionKey, VideoFrame>,
    /// The original captured frame (used when no downscale is required).
    original: VideoFrame,
}

impl ScaledFrameSet {
    /// Get the frame for the given layer target.  Returns the pre-scaled
    /// version if one exists, otherwise the original.
    fn frame_for(&self, target: &LayerEncodingTarget) -> VideoFrame {
        let key = compute_output_resolution(
            self.original.width,
            self.original.height,
            target.width,
            target.height,
        );
        if let Some(frame) = self.frames.get(&key) {
            frame.clone()
        } else {
            self.original.clone()
        }
    }
}

/// Compute the actual output dimensions for a given input and target, matching
/// the logic used during scaling (aspect-preserving fit, even dimensions).
fn compute_output_resolution(
    src_w: u32,
    src_h: u32,
    target_w: u32,
    target_h: u32,
) -> ResolutionKey {
    if src_w <= target_w && src_h <= target_h {
        return (src_w, src_h);
    }
    let scale = (target_w as f32 / src_w as f32).min(target_h as f32 / src_h as f32);
    if scale >= 1.0 {
        return (src_w, src_h);
    }
    let out_w = ((src_w as f32 * scale).round() as u32).max(2) & !1;
    let out_h = ((src_h as f32 * scale).round() as u32).max(2) & !1;
    (out_w, out_h)
}

/// Collect the set of distinct output resolutions needed, given the captured
/// frame dimensions and the layer targets.
fn distinct_resolutions(
    src_w: u32,
    src_h: u32,
    targets: &[LayerEncodingTarget],
) -> Vec<ResolutionKey> {
    let mut seen = Vec::new();
    for t in targets {
        let key = compute_output_resolution(src_w, src_h, t.width, t.height);
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    seen
}

/// Bilinear downscale for BGRA pixel data.
///
/// Uses fixed-point (8.8) arithmetic to interpolate a 2x2 neighbourhood for
/// each output pixel.  Roughly 2x the quality of nearest-neighbour for text
/// and diagonal edges with minimal extra cost.
fn bilinear_downscale_bgra(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    src_stride: u32,
    dst_w: u32,
    dst_h: u32,
) -> Vec<u8> {
    let src_stride = src_stride as usize;
    let mut out = vec![0u8; (dst_w * dst_h * 4) as usize];
    let dst_stride = dst_w as usize * 4;

    // Pre-compute horizontal mapping (fixed-point 8.8).
    let x_ratio = ((src_w as u64) << 8) / dst_w.max(1) as u64;
    let y_ratio = ((src_h as u64) << 8) / dst_h.max(1) as u64;

    let max_sx = (src_w as usize).saturating_sub(1);
    let max_sy = (src_h as usize).saturating_sub(1);

    for dy in 0..dst_h as usize {
        let sy_fp = (dy as u64 * y_ratio) as usize;
        let sy0 = (sy_fp >> 8).min(max_sy);
        let sy1 = (sy0 + 1).min(max_sy);
        let fy = (sy_fp & 0xFF) as u16; // fractional part [0..255]
        let fy_inv = 256 - fy;

        let src_row0 = sy0 * src_stride;
        let src_row1 = sy1 * src_stride;
        let dst_row = dy * dst_stride;

        for dx in 0..dst_w as usize {
            let sx_fp = (dx as u64 * x_ratio) as usize;
            let sx0 = (sx_fp >> 8).min(max_sx);
            let sx1 = (sx0 + 1).min(max_sx);
            let fx = (sx_fp & 0xFF) as u16;
            let fx_inv = 256 - fx;

            let i00 = src_row0 + sx0 * 4;
            let i10 = src_row0 + sx1 * 4;
            let i01 = src_row1 + sx0 * 4;
            let i11 = src_row1 + sx1 * 4;

            let dst_idx = dst_row + dx * 4;

            // Bounds check: if any source index is out of range, fall back to
            // nearest-neighbour (copy top-left sample).
            if i00 + 4 > src.len()
                || i10 + 4 > src.len()
                || i01 + 4 > src.len()
                || i11 + 4 > src.len()
            {
                if i00 + 4 <= src.len() && dst_idx + 4 <= out.len() {
                    out[dst_idx..dst_idx + 4].copy_from_slice(&src[i00..i00 + 4]);
                }
                continue;
            }

            for c in 0..4_usize {
                let p00 = src[i00 + c] as u16;
                let p10 = src[i10 + c] as u16;
                let p01 = src[i01 + c] as u16;
                let p11 = src[i11 + c] as u16;

                let top = p00 * fx_inv + p10 * fx;
                let bot = p01 * fx_inv + p11 * fx;
                let val = (top * fy_inv + bot * fy + (1 << 15)) >> 16;
                out[dst_idx + c] = val.min(255) as u8;
            }
        }
    }

    out
}

/// Downscale a single frame to fit within `target_width x target_height`,
/// preserving aspect ratio.  Uses bilinear filtering for BGRA frames.
fn downscale_frame_to_fit(frame: &VideoFrame, target_width: u32, target_height: u32) -> VideoFrame {
    if frame.width <= target_width && frame.height <= target_height {
        return frame.clone();
    }
    let scale =
        (target_width as f32 / frame.width as f32).min(target_height as f32 / frame.height as f32);
    if scale >= 1.0 {
        return frame.clone();
    }
    let out_width = ((frame.width as f32 * scale).round() as u32).max(2) & !1;
    let out_height = ((frame.height as f32 * scale).round() as u32).max(2) & !1;

    match &frame.planes {
        FramePlanes::Bgra { bytes, stride } => {
            let scaled = bilinear_downscale_bgra(
                bytes,
                frame.width,
                frame.height,
                *stride,
                out_width,
                out_height,
            );
            VideoFrame {
                width: out_width,
                height: out_height,
                ts_ms: frame.ts_ms,
                format: frame.format,
                planes: FramePlanes::Bgra {
                    bytes: bytes::Bytes::from(scaled),
                    stride: out_width * 4,
                },
            }
        }
        _ => frame.clone(),
    }
}

/// Build the shared `ScaledFrameSet` for a captured frame, producing each
/// distinct resolution exactly once.
fn build_scaled_frame_set(frame: VideoFrame, targets: &[LayerEncodingTarget]) -> ScaledFrameSet {
    let resolutions = distinct_resolutions(frame.width, frame.height, targets);
    let mut frames = HashMap::with_capacity(resolutions.len());
    for &(rw, rh) in &resolutions {
        // Skip if the resolution matches the original (no scaling needed).
        if rw == frame.width && rh == frame.height {
            continue;
        }
        let scaled = downscale_frame_to_fit(&frame, rw, rh);
        frames.insert((rw, rh), scaled);
    }
    ScaledFrameSet {
        frames,
        original: frame,
    }
}

// ---------------------------------------------------------------------------
// Session config helper
// ---------------------------------------------------------------------------

fn build_session_config(
    frame: &VideoFrame,
    target_fps: u32,
    target_bitrate_bps: u32,
) -> VideoSessionConfig {
    VideoSessionConfig {
        width: frame.width,
        height: frame.height,
        fps: target_fps,
        target_bitrate_bps,
        low_latency: true,
        allow_frame_drop: true,
    }
}

// ---------------------------------------------------------------------------
// Main pipeline
// ---------------------------------------------------------------------------

pub async fn start_local_share(params: StartLocalShareParams) {
    let layer_targets = build_layer_targets(
        &params.offered_layers,
        &params.accepted_layer_ids,
        params.negotiated_profile,
    );
    if layer_targets.is_empty() {
        warn!("[video] no accepted screen-share layers to encode");
        return;
    }
    let capture_fps = layer_targets.iter().map(|l| l.fps).max().unwrap_or(30);

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

    // ---- Audio (unchanged) ----
    let mut audio_worker: Option<tokio::task::JoinHandle<()>> = None;
    if params.include_audio {
        match crate::screen_share::audio::build_system_audio_backend(
            params.runtime_caps.as_ref(),
            &params.source,
        ) {
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
                            let channels = backend.channels();
                            let mut encoder = match OpusEncoder::new(
                                48_000,
                                channels as u8,
                                OpusEncoderProfile::Music,
                            ) {
                                Ok(enc) => enc,
                                Err(e) => {
                                    warn!(error=?e, channels, "[audio-share] failed to initialize opus encoder");
                                    backend.stop();
                                    return;
                                }
                            };
                            if let Err(e) =
                                encoder.set_bitrate(if channels > 1 { 96_000 } else { 64_000 })
                            {
                                warn!(error=?e, "[audio-share] failed to set opus bitrate");
                            }
                            let mut pcm = vec![0i16; 960 * channels];
                            let mut out = vec![0u8; 4000];
                            let ssrc: u32 = rand::random();
                            let mut seq = 0u32;
                            let session_zero = Instant::now();
                            let mut stall_spins = 0u64;
                            let mut underflows = 0u64;
                            while !stop.load(Ordering::Relaxed) {
                                if !backend.read_frame(&mut pcm) {
                                    stall_spins = stall_spins.wrapping_add(1);
                                    underflows = underflows.wrapping_add(1);
                                    if stall_spins % 200 == 0 {
                                        warn!(
                                            stall_spins,
                                            underflows,
                                            "[audio-share] system audio stalled waiting for frame"
                                        );
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                                    continue;
                                }
                                stall_spins = 0;
                                let Ok(n) = encoder.encode(&pcm, &mut out) else {
                                    warn!(
                                        "[audio-share] opus encode failed for system audio frame"
                                    );
                                    continue;
                                };
                                let route = active_route.load(Ordering::Relaxed);
                                if route == 0 {
                                    continue;
                                }
                                let ts_ms = session_zero.elapsed().as_millis() as u32;
                                let d =
                                    make_voice_datagram(route, ssrc, seq, ts_ms, true, &out[..n]);
                                if let Err(reason) = egress.enqueue_voice(d) {
                                    warn!(
                                        ?reason,
                                        "[audio-share] failed to enqueue system audio datagram"
                                    );
                                }
                                seq = seq.wrapping_add(1);
                            }
                            info!(underflows, "[audio-share] system audio worker stopped");
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

    // ---- Capture task (unchanged) ----
    let capture_stop = stop_flag.clone();
    let capture_source = params.source.clone();
    let capture_caps = params.runtime_caps.clone();
    let capture_q = capture_queue.clone();
    let capture_len = params.capture_queue_len_gauge.clone();
    let capture_ovf = params.capture_queue_overflow_total.clone();
    let counters = params.counters.clone();
    let capture_task = tokio::task::spawn_blocking(move || {
        let mut pacer = FramePacer::new(capture_fps);
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
                    pacer.wait();
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

    // ---- Build per-(stream, layer) encoder pipelines ----
    //
    // New architecture:
    //   capture_queue
    //     -> scale_fanout task (one)
    //          produces Arc<ScaledFrameSet> per frame, pushes into each
    //          encoder's OverwriteQueue
    //     -> per-(stream, layer) encode task
    //          picks its resolution from the ScaledFrameSet
    //     -> per-(stream, layer) send task
    //
    // This ensures each target resolution is computed at most once per frame.

    // Collect all encode queues so the single scale-fanout task can push into
    // them.
    struct EncoderSlot {
        encode_queue: Arc<OverwriteQueue<Arc<ScaledFrameSet>>>,
        target: LayerEncodingTarget,
        stream_tag: u64,
        codec: pb::VideoCodec,
    }

    let mut encoder_slots: Vec<EncoderSlot> = Vec::new();
    let mut stream_workers = Vec::new();

    for stream in &params.streams {
        for &target in &layer_targets {
            let mut sender = VideoSender::new(
                stream.stream_tag,
                target.layer_id,
                target.profile,
                params.mtu,
            );
            // Enable pacing for high-bitrate layers to smooth burst transmission;
            // low-bitrate layers rely on the egress scheduler.
            sender.set_pacing_enabled(target.bitrate_bps >= PACING_BITRATE_THRESHOLD_BPS);

            let mut encoder = match build_screen_encoder(
                stream.codec,
                params.sender_policy,
                params.runtime_caps.as_ref(),
            ) {
                Ok(enc) => enc,
                Err(e) => {
                    warn!(error=?e, stream_tag=stream.stream_tag, codec=%video_codec_name(stream.codec), layer_id=target.layer_id, "[video] failed to build screen encoder");
                    continue;
                }
            };
            if let Ok(mut label) = params.backend_label.lock() {
                *label = encoder.backend_name().to_string();
            }

            let encode_queue =
                Arc::new(OverwriteQueue::<Arc<ScaledFrameSet>>::new(ENCODE_QUEUE_DEPTH));
            let packet_queue = Arc::new(OverwriteQueue::<EncodedFrame>::new(
                PACKETIZATION_QUEUE_DEPTH,
            ));

            encoder_slots.push(EncoderSlot {
                encode_queue: encode_queue.clone(),
                target,
                stream_tag: stream.stream_tag,
                codec: stream.codec,
            });

            // ---- Encode task ----
            let stream_encode_q = encode_queue.clone();
            let stream_packet_q = packet_queue.clone();
            let keyframe_generation = params.force_keyframe_generation.clone();
            let stream_tag = stream.stream_tag;
            let encode_depth = params.encode_queue_len_gauge.clone();
            let packet_depth = params.packetize_queue_len_gauge.clone();
            let counters = params.counters.clone();
            let egress_stats = params.egress.stats();
            let layer_id = target.layer_id;
            let encode_task = tokio::spawn(async move {
                let mut configured = false;
                let mut quality = AdaptiveQualityController::new(target.bitrate_bps, target.fps);
                let mut last_report = std::time::Instant::now();
                let mut encoded_frames = 0_u32;
                let mut last_encoded_at = std::time::Instant::now();
                let mut last_force_keyframe_generation =
                    keyframe_generation.load(Ordering::Relaxed);
                while let Some(scaled_set) = stream_encode_q.pop_latest_or_wait().await {
                    encode_depth.store(stream_encode_q.len() as u64, Ordering::Relaxed);
                    let frame = scaled_set.frame_for(&target);
                    if !configured {
                        if let Err(e) = encoder.configure_session(build_session_config(
                            &frame,
                            target.fps,
                            target.bitrate_bps,
                        )) {
                            warn!(error=?e, stream_tag, layer_id, "[video] failed to configure encoder session");
                            continue;
                        }
                        configured = true;
                    }

                    let snapshot = PressureSnapshot {
                        encode_queue_len: stream_encode_q.len(),
                        packet_queue_len: stream_packet_q.len(),
                        dropped_video_fragments: egress_stats
                            .drop_queue_full_video
                            .load(Ordering::Relaxed),
                    };
                    let (next_bitrate, next_fps) = quality.evaluate(snapshot);
                    let _ = encoder.update_bitrate(next_bitrate);

                    let min_interval = Duration::from_secs_f64(1.0 / next_fps.max(1) as f64);
                    if last_encoded_at.elapsed() < min_interval {
                        continue;
                    }
                    last_encoded_at = std::time::Instant::now();

                    let generation = keyframe_generation.load(Ordering::Relaxed);
                    if generation != last_force_keyframe_generation {
                        last_force_keyframe_generation = generation;
                        let _ = encoder.request_keyframe();
                    }

                    match encoder.encode(frame) {
                        Ok(encoded) => {
                            encoded_frames = encoded_frames.saturating_add(1);
                            let elapsed = last_report.elapsed();
                            if elapsed >= Duration::from_secs(1) {
                                let runtime_fps = encoded_frames as f32 / elapsed.as_secs_f32();
                                crate::net::dispatcher::report_runtime_encode_fps(runtime_fps);
                                last_report = std::time::Instant::now();
                                encoded_frames = 0;
                            }
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
                        Err(e) => {
                            counters.encode_errors.fetch_add(1, Ordering::Relaxed);
                            warn!(error=?e, stream_tag, layer_id, "[video] encode failed")
                        }
                    }
                }
                stream_packet_q.close();
            });

            // ---- Send task ----
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
                                    warn!(
                                        ?reason,
                                        stream_tag, frame_idx, "[video] enqueue rejected"
                                    );
                                }
                            }
                        })
                        .await
                    {
                        counters.sender_frame_errors.fetch_add(1, Ordering::Relaxed);
                        warn!(error=?e, stream_tag, layer_id, frame_size=encoded.data.len(), "[video] send_frame failed");
                        break;
                    }
                    frame_idx = frame_idx.wrapping_add(1);
                }
            });

            stream_workers.push(encode_task);
            stream_workers.push(send_task);
        }
    }

    // ---- Scale-fanout task ----
    //
    // Single task that reads from the capture queue, builds a ScaledFrameSet
    // (scaling each distinct resolution once), and pushes Arc<ScaledFrameSet>
    // into every encoder's queue.
    let scale_capture_q = capture_queue.clone();
    let scale_stop = stop_flag.clone();
    let scale_counters = params.counters.clone();
    let scale_targets = layer_targets.clone();
    // Collect just the queues for the fanout push.
    let fanout_queues: Vec<Arc<OverwriteQueue<Arc<ScaledFrameSet>>>> = encoder_slots
        .iter()
        .map(|s| s.encode_queue.clone())
        .collect();
    let num_distinct = {
        // Log how many distinct resolutions we'll scale to.
        let sample_resolutions = distinct_resolutions(1920, 1080, &scale_targets);
        let n = sample_resolutions.len();
        info!(
            distinct_resolutions = n,
            total_encoder_slots = fanout_queues.len(),
            "[video] scale-fanout: sharing scaled frames across encoders"
        );
        n
    };
    let _ = num_distinct; // used for logging only

    let scale_fanout_task = tokio::spawn(async move {
        while !scale_stop.load(Ordering::Relaxed) {
            let Some(frame) = scale_capture_q.pop_latest_or_wait().await else {
                break;
            };
            let scaled_set = Arc::new(build_scaled_frame_set(frame, &scale_targets));
            scale_counters
                .queue_depth_encode
                .store(0, Ordering::Relaxed);
            for q in &fanout_queues {
                q.push(Arc::clone(&scaled_set));
            }
        }
        // Close all downstream encode queues.
        for q in &fanout_queues {
            q.close();
        }
    });
    stream_workers.push(scale_fanout_task);

    // ---- Join all workers ----
    for worker in stream_workers {
        let _ = worker.await;
    }
    if let Some(worker) = audio_worker {
        let _ = worker.await;
    }
    let _ = stop_watch_task.await;
    let _ = capture_task.await;
}

fn build_layer_targets(
    offered_layers: &[pb::SimulcastLayer],
    accepted_layer_ids: &[u32],
    fallback_profile: VideoStreamProfile,
) -> Vec<LayerEncodingTarget> {
    let mut out = offered_layers
        .iter()
        .filter(|layer| accepted_layer_ids.contains(&layer.layer_id))
        .map(|layer| LayerEncodingTarget {
            layer_id: layer.layer_id.clamp(0, u8::MAX as u32) as u8,
            width: layer.width.max(320),
            height: layer.height.max(180),
            fps: layer.max_fps.clamp(5, 60),
            bitrate_bps: layer.max_bitrate_bps.max(600_000),
            profile: if layer.width >= 2560 || layer.height >= 1440 {
                VideoStreamProfile::P1440p60
            } else {
                fallback_profile
            },
        })
        .collect::<Vec<_>>();
    out.sort_by_key(|target| target.layer_id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::video_frame::FramePlanes;

    fn make_frame(width: u32, height: u32) -> VideoFrame {
        VideoFrame {
            width,
            height,
            ts_ms: 0,
            format: crate::net::video_frame::PixelFormat::Bgra,
            planes: FramePlanes::Bgra {
                bytes: bytes::Bytes::from(vec![0_u8; (width * height * 4) as usize]),
                stride: width * 4,
            },
        }
    }

    fn make_gradient_frame(width: u32, height: u32) -> VideoFrame {
        let mut data = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                data[idx] = (x % 256) as u8; // B
                data[idx + 1] = (y % 256) as u8; // G
                data[idx + 2] = ((x + y) % 256) as u8; // R
                data[idx + 3] = 255; // A
            }
        }
        VideoFrame {
            width,
            height,
            ts_ms: 42,
            format: crate::net::video_frame::PixelFormat::Bgra,
            planes: FramePlanes::Bgra {
                bytes: bytes::Bytes::from(data),
                stride: width * 4,
            },
        }
    }

    #[test]
    fn settings_drive_encoder_session_config() {
        let frame = make_frame(1280, 720);
        let config = build_session_config(&frame, 24, 2_400_000);
        assert_eq!(config.width, 1280);
        assert_eq!(config.height, 720);
        assert_eq!(config.fps, 24);
        assert_eq!(config.target_bitrate_bps, 2_400_000);
    }

    #[test]
    fn frame_pacer_honors_target_fps() {
        let mut pacer = FramePacer::new(20);
        let start = std::time::Instant::now();
        for _ in 0..3 {
            pacer.wait();
        }
        assert!(start.elapsed() >= Duration::from_millis(120));
    }

    #[test]
    fn pressure_reduces_bitrate_and_fps() {
        let mut controller = AdaptiveQualityController::new(3_000_000, 30);
        let (bitrate_a, fps_a) = controller.evaluate(PressureSnapshot {
            encode_queue_len: 0,
            packet_queue_len: 0,
            dropped_video_fragments: 0,
        });
        assert_eq!(bitrate_a, 3_000_000);
        assert_eq!(fps_a, 30);

        let (bitrate_b, fps_b) = controller.evaluate(PressureSnapshot {
            encode_queue_len: ENCODE_QUEUE_DEPTH,
            packet_queue_len: PACKETIZATION_QUEUE_DEPTH,
            dropped_video_fragments: 10,
        });
        assert!(bitrate_b < bitrate_a);
        assert!(fps_b < fps_a);
    }

    #[test]
    fn build_layer_targets_keeps_only_accepted_layers() {
        let offered = vec![
            pb::SimulcastLayer {
                layer_id: 0,
                width: 1280,
                height: 720,
                max_fps: 30,
                max_bitrate_bps: 2_000_000,
            },
            pb::SimulcastLayer {
                layer_id: 1,
                width: 1920,
                height: 1080,
                max_fps: 60,
                max_bitrate_bps: 6_000_000,
            },
            pb::SimulcastLayer {
                layer_id: 2,
                width: 2560,
                height: 1440,
                max_fps: 60,
                max_bitrate_bps: 12_000_000,
            },
        ];
        let targets = build_layer_targets(&offered, &[0, 2], VideoStreamProfile::P1080p60);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].layer_id, 0);
        assert_eq!(targets[1].layer_id, 2);
    }

    // --- New tests for shared scaling pipeline ---

    #[test]
    fn compute_output_resolution_no_downscale_needed() {
        // Source fits within target: output = source.
        assert_eq!(compute_output_resolution(1280, 720, 1920, 1080), (1280, 720));
    }

    #[test]
    fn compute_output_resolution_downscale() {
        // 1920x1080 -> fit within 1280x720.
        let (w, h) = compute_output_resolution(1920, 1080, 1280, 720);
        assert!(w <= 1280);
        assert!(h <= 720);
        // Must be even.
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
        // Aspect ratio approximately preserved.
        let src_ar = 1920.0 / 1080.0_f32;
        let dst_ar = w as f32 / h as f32;
        assert!((src_ar - dst_ar).abs() < 0.05);
    }

    #[test]
    fn compute_output_resolution_extreme_aspect() {
        // Very wide source: 3840x100, target 1920x1080.
        let (w, h) = compute_output_resolution(3840, 100, 1920, 1080);
        assert!(w <= 1920);
        assert!(h <= 1080);
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }

    #[test]
    fn distinct_resolutions_deduplicates() {
        let targets = vec![
            LayerEncodingTarget {
                layer_id: 0,
                width: 1280,
                height: 720,
                fps: 30,
                bitrate_bps: 2_000_000,
                profile: VideoStreamProfile::P1080p60,
            },
            LayerEncodingTarget {
                layer_id: 1,
                width: 1280,
                height: 720,
                fps: 60,
                bitrate_bps: 4_000_000,
                profile: VideoStreamProfile::P1080p60,
            },
            LayerEncodingTarget {
                layer_id: 2,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 8_000_000,
                profile: VideoStreamProfile::P1080p60,
            },
        ];
        // Source: 1920x1080. Layers 0 and 1 both want 1280x720, layer 2 wants
        // 1920x1080 (no downscale). Should deduplicate to 2 resolutions.
        let resolutions = distinct_resolutions(1920, 1080, &targets);
        assert_eq!(resolutions.len(), 2);
    }

    #[test]
    fn build_scaled_frame_set_produces_correct_resolutions() {
        let frame = make_gradient_frame(1920, 1080);
        let targets = vec![
            LayerEncodingTarget {
                layer_id: 0,
                width: 1280,
                height: 720,
                fps: 30,
                bitrate_bps: 2_000_000,
                profile: VideoStreamProfile::P1080p60,
            },
            LayerEncodingTarget {
                layer_id: 1,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 6_000_000,
                profile: VideoStreamProfile::P1080p60,
            },
        ];
        let set = build_scaled_frame_set(frame, &targets);

        // Layer 0 should get a 1280x720 frame.
        let f0 = set.frame_for(&targets[0]);
        assert_eq!(f0.width, 1280);
        assert_eq!(f0.height, 720);

        // Layer 1 should get the original (no downscale needed).
        let f1 = set.frame_for(&targets[1]);
        assert_eq!(f1.width, 1920);
        assert_eq!(f1.height, 1080);

        // Only 1 entry in the frames map (the downscaled one); original is
        // stored separately.
        assert_eq!(set.frames.len(), 1);
    }

    #[test]
    fn bilinear_downscale_preserves_dimensions() {
        let src_w = 1920_u32;
        let src_h = 1080_u32;
        let dst_w = 960_u32;
        let dst_h = 540_u32;
        let src = vec![128u8; (src_w * src_h * 4) as usize];
        let out = bilinear_downscale_bgra(&src, src_w, src_h, src_w * 4, dst_w, dst_h);
        assert_eq!(out.len(), (dst_w * dst_h * 4) as usize);
        // Uniform input -> uniform output.
        for &b in &out {
            assert_eq!(b, 128);
        }
    }

    #[test]
    fn bilinear_downscale_gradient_smoothness() {
        // Downscale a horizontal gradient; output should still be monotonic.
        let src_w = 256_u32;
        let src_h = 4_u32;
        let mut src = vec![0u8; (src_w * src_h * 4) as usize];
        for x in 0..src_w {
            for y in 0..src_h {
                let idx = ((y * src_w + x) * 4) as usize;
                src[idx] = x as u8; // B channel = horizontal gradient
                src[idx + 1] = 0;
                src[idx + 2] = 0;
                src[idx + 3] = 255;
            }
        }
        let dst_w = 64_u32;
        let dst_h = 2_u32;
        let out = bilinear_downscale_bgra(&src, src_w, src_h, src_w * 4, dst_w, dst_h);

        // Check monotonicity of B channel along the first row.
        let mut prev = 0u8;
        for x in 0..dst_w {
            let idx = (x * 4) as usize;
            let b = out[idx];
            assert!(b >= prev, "gradient not monotonic at x={x}: {b} < {prev}");
            prev = b;
        }
    }

    #[test]
    fn downscale_frame_noop_when_smaller_than_target() {
        let frame = make_frame(640, 480);
        let result = downscale_frame_to_fit(&frame, 1920, 1080);
        assert_eq!(result.width, 640);
        assert_eq!(result.height, 480);
    }

    #[test]
    fn downscale_frame_reduces_resolution() {
        let frame = make_gradient_frame(1920, 1080);
        let result = downscale_frame_to_fit(&frame, 1280, 720);
        assert!(result.width <= 1280);
        assert!(result.height <= 720);
        assert_eq!(result.ts_ms, 42);
    }

    #[test]
    fn scaled_frame_set_shared_across_same_resolution() {
        let frame = make_gradient_frame(2560, 1440);
        let targets = vec![
            // Two layers wanting the same 1280x720 resolution.
            LayerEncodingTarget {
                layer_id: 0,
                width: 1280,
                height: 720,
                fps: 30,
                bitrate_bps: 2_000_000,
                profile: VideoStreamProfile::P1080p60,
            },
            LayerEncodingTarget {
                layer_id: 1,
                width: 1280,
                height: 720,
                fps: 60,
                bitrate_bps: 3_000_000,
                profile: VideoStreamProfile::P1080p60,
            },
        ];
        let set = build_scaled_frame_set(frame, &targets);

        let f0 = set.frame_for(&targets[0]);
        let f1 = set.frame_for(&targets[1]);
        assert_eq!(f0.width, f1.width);
        assert_eq!(f0.height, f1.height);
        // Only one scaled variant (not two).
        assert_eq!(set.frames.len(), 1);
    }

    #[test]
    fn pacing_threshold_logic() {
        // Below threshold -> pacing disabled.
        assert!(3_000_000 < PACING_BITRATE_THRESHOLD_BPS);
        // At or above threshold -> pacing enabled.
        assert!(4_000_000 >= PACING_BITRATE_THRESHOLD_BPS);
        assert!(12_000_000 >= PACING_BITRATE_THRESHOLD_BPS);
    }
}
