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
mod identity;
mod net;
mod proto;
mod settings_io;
mod ui;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use config::Config;
use crossbeam_channel::{bounded, Receiver, Sender};
use identity::DeviceIdentity;
use net::dispatcher::{ControlDispatcher, PushEvent};
use net::egress::EgressScheduler;
use net::overwrite_queue::{pop_voice_realtime, OverwriteQueue, StampedBytes};
use net::video_datagram::VideoHeader;
use net::video_transport::{VideoReceiver, VideoSender, VideoStreamProfile};
use net::voice_datagram::{
    make_voice_datagram, VOICE_FORWARDED_HDR_LEN, VOICE_HDR_LEN, VOICE_VERSION,
};
use proto::voiceplatform::v1 as pb;
use std::collections::HashMap;
#[cfg(debug_assertions)]
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
    Arc,
};
#[cfg(debug_assertions)]
use std::sync::{Mutex as StdMutex, OnceLock};
use tokio::sync::{mpsc, watch, Mutex, RwLock};
use tokio::time::{sleep, Duration, Instant, MissedTickBehavior};
use tracing::{debug, info, warn, Level};
use tracing_subscriber::EnvFilter;
use ui::model::AudioDeviceId;
use ui::model::{AttachmentAsset, DspMethod, FecMode, PerUserAudioSettings, ShareSourceSelection};
use ui::{UiEvent, UiIntent, VpApp};

#[cfg(debug_assertions)]
static DEBUG_SEEN_AUTH_USER_IDS: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();

pub const BUILD_VERSION: &str = env!("VP_CLIENT_BUILD_VERSION");

const VOICE_INGRESS_CAP: usize = 16; // Do not increase without justification; latency risk.
const VOICE_MAX_AGE: Duration = Duration::from_millis(250);
const VOICE_DRAIN_KEEP_LATEST: usize = 4;

#[derive(Debug, Clone)]
struct PttState {
    pressed: bool,
    release_deadline: Option<Instant>,
}

#[derive(Debug, Clone)]
pub enum ShareSource {
    WindowsDisplay(String),
    WindowsWindow(String),
    LinuxPortal(String),
    X11Window(u64),
}

#[derive(Debug, Clone, Copy)]
pub enum PixelFormat {
    Bgra,
    Nv12,
}

#[derive(Debug, Clone)]
pub enum FrameData {
    Cpu(Bytes),
}

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub ts_ms: u32,
    pub format: PixelFormat,
    pub data: FrameData,
}

pub trait ScreenCapture {
    fn next_frame(&mut self) -> anyhow::Result<CapturedFrame>;
}

#[derive(Debug)]
pub struct EncodedFrame {
    pub ts_ms: u32,
    pub is_keyframe: bool,
    pub data: bytes::Bytes,
}

trait ScreenEncoder: Send {
    fn encode(&mut self, frame: CapturedFrame) -> anyhow::Result<EncodedFrame>;
}

struct Av1AvifEncoder {
    frame_seq: u32,
}

impl Av1AvifEncoder {
    fn new() -> Self {
        Self { frame_seq: 0 }
    }
}

impl ScreenEncoder for Av1AvifEncoder {
    fn encode(&mut self, frame: CapturedFrame) -> anyhow::Result<EncodedFrame> {
        use image::codecs::avif::AvifEncoder;
        use image::{ColorType, ImageEncoder};

        let FrameData::Cpu(src) = frame.data;
        let width = frame.width as usize;
        let height = frame.height as usize;
        let stride = frame.stride as usize;
        let mut rgba = vec![0_u8; width * height * 4];

        match frame.format {
            PixelFormat::Bgra => {
                for y in 0..height {
                    let src_row = &src[y * stride..(y * stride) + width * 4];
                    let dst_row = &mut rgba[y * width * 4..(y + 1) * width * 4];
                    for x in 0..width {
                        let si = x * 4;
                        let di = x * 4;
                        dst_row[di] = src_row[si + 2];
                        dst_row[di + 1] = src_row[si + 1];
                        dst_row[di + 2] = src_row[si];
                        dst_row[di + 3] = 255;
                    }
                }
            }
            PixelFormat::Nv12 => return Err(anyhow!("NV12 screen encoding is not implemented")),
        }

        let mut encoded = Vec::new();
        AvifEncoder::new(&mut encoded)
            .write_image(&rgba, frame.width, frame.height, ColorType::Rgba8.into())
            .context("encode AV1/AVIF frame")?;

        let encoded = EncodedFrame {
            ts_ms: frame.ts_ms,
            is_keyframe: self.frame_seq % 60 == 0,
            data: Bytes::from(encoded),
        };
        self.frame_seq = self.frame_seq.wrapping_add(1);
        Ok(encoded)
    }
}

#[cfg(feature = "dev-synthetic-stream")]
struct SyntheticCapture {
    width: u32,
    height: u32,
    frame_idx: u32,
}

#[cfg(feature = "dev-synthetic-stream")]
impl SyntheticCapture {
    fn new() -> Self {
        Self {
            width: 1280,
            height: 720,
            frame_idx: 0,
        }
    }
}

#[cfg(feature = "dev-synthetic-stream")]
impl ScreenCapture for SyntheticCapture {
    fn next_frame(&mut self) -> anyhow::Result<CapturedFrame> {
        let mut rgba = vec![0_u8; (self.width * self.height * 4) as usize];
        for y in 0..self.height as usize {
            for x in 0..self.width as usize {
                let idx = (y * self.width as usize + x) * 4;
                rgba[idx] = ((x as u32 + self.frame_idx) & 0xff) as u8;
                rgba[idx + 1] = ((y as u32 + self.frame_idx * 2) & 0xff) as u8;
                rgba[idx + 2] = (self.frame_idx & 0xff) as u8;
                rgba[idx + 3] = 255;
            }
        }
        self.frame_idx = self.frame_idx.wrapping_add(1);
        Ok(CapturedFrame {
            width: self.width,
            height: self.height,
            stride: self.width * 4,
            ts_ms: unix_ms() as u32,
            format: PixelFormat::Bgra,
            data: FrameData::Cpu(Bytes::from(rgba)),
        })
    }
}

struct ScrapCapture {
    capturer: scrap::Capturer,
    width: u32,
    height: u32,
}

#[cfg(target_os = "windows")]
struct WindowsWindowCapture {
    hwnd: windows::Win32::Foundation::HWND,
}

#[cfg(target_os = "windows")]
impl WindowsWindowCapture {
    fn from_source(source: &ShareSource) -> anyhow::Result<Self> {
        use windows::Win32::Foundation::HWND;

        let ShareSource::WindowsWindow(id) = source else {
            return Err(anyhow!("invalid capture source for Windows window backend"));
        };

        let hwnd_raw = id
            .strip_prefix("window-hwnd-")
            .and_then(|value| value.parse::<isize>().ok())
            .ok_or_else(|| anyhow!("invalid window id: {id}"))?;
        let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
        if hwnd.0.is_null() {
            return Err(anyhow!("invalid window handle: {id}"));
        }

        Ok(Self { hwnd })
    }

    fn window_dimensions(&self) -> anyhow::Result<(u32, u32)> {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

        let mut rect = RECT::default();
        if unsafe { GetWindowRect(self.hwnd, &mut rect) }.is_err() {
            return Err(anyhow!(
                "failed to query window bounds for hwnd={:?}",
                self.hwnd.0
            ));
        }
        let width = (rect.right - rect.left).max(1) as u32;
        let height = (rect.bottom - rect.top).max(1) as u32;
        Ok((width, height))
    }
}

#[cfg(target_os = "windows")]
impl ScreenCapture for WindowsWindowCapture {
    fn next_frame(&mut self) -> anyhow::Result<CapturedFrame> {
        use windows::Win32::Graphics::Gdi::{
            BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits,
            GetWindowDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
            DIB_RGB_COLORS, HGDIOBJ, SRCCOPY,
        };
        use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, PW_RENDERFULLCONTENT};

        if !unsafe { IsWindow(Some(self.hwnd)) }.as_bool() {
            return Err(anyhow!("window is no longer valid: hwnd={:?}", self.hwnd.0));
        }

        let (width, height) = self.window_dimensions()?;
        let window_dc = unsafe { GetWindowDC(Some(self.hwnd)) };
        if window_dc.0.is_null() {
            return Err(anyhow!("failed to get window device context"));
        }

        let mem_dc = unsafe { CreateCompatibleDC(Some(window_dc)) };
        if mem_dc.0.is_null() {
            unsafe {
                ReleaseDC(Some(self.hwnd), window_dc);
            }
            return Err(anyhow!("failed to create memory device context"));
        }

        let bitmap = unsafe { CreateCompatibleBitmap(window_dc, width as i32, height as i32) };
        if bitmap.0.is_null() {
            unsafe {
                DeleteDC(mem_dc);
                ReleaseDC(Some(self.hwnd), window_dc);
            }
            return Err(anyhow!("failed to create compatible bitmap"));
        }

        let previous = unsafe { SelectObject(mem_dc, HGDIOBJ(bitmap.0)) };
        if previous.0.is_null() {
            unsafe {
                DeleteObject(HGDIOBJ(bitmap.0));
                DeleteDC(mem_dc);
                ReleaseDC(Some(self.hwnd), window_dc);
            }
            return Err(anyhow!("failed to select bitmap into device context"));
        }

        let printed =
            unsafe { PrintWindow(self.hwnd, mem_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)) }
                .as_bool();
        if !printed {
            let _ = unsafe {
                BitBlt(
                    mem_dc,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    Some(window_dc),
                    0,
                    0,
                    SRCCOPY,
                )
            };
        }

        let mut pixels = vec![0_u8; (width * height * 4) as usize];
        let mut bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let rows = unsafe {
            GetDIBits(
                mem_dc,
                bitmap,
                0,
                height,
                Some(pixels.as_mut_ptr() as *mut _),
                &mut bitmap_info,
                DIB_RGB_COLORS,
            )
        };

        unsafe {
            SelectObject(mem_dc, previous);
            DeleteObject(HGDIOBJ(bitmap.0));
            DeleteDC(mem_dc);
            ReleaseDC(Some(self.hwnd), window_dc);
        }

        if rows == 0 {
            return Err(anyhow!("failed to read window pixels from DIB"));
        }

        Ok(CapturedFrame {
            width,
            height,
            stride: width * 4,
            ts_ms: unix_ms() as u32,
            format: PixelFormat::Bgra,
            data: FrameData::Cpu(Bytes::from(pixels)),
        })
    }
}

impl ScrapCapture {
    fn from_source(source: &ShareSource) -> anyhow::Result<Self> {
        let displays = scrap::Display::all().context("enumerate displays")?;
        let display = match source {
            ShareSource::WindowsDisplay(id) => {
                let n = id
                    .strip_prefix("screen-")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(1)
                    .max(1);
                displays
                    .into_iter()
                    .nth(n - 1)
                    .ok_or_else(|| anyhow!("display source not found: {id}"))?
            }
            ShareSource::WindowsWindow(id) => {
                let _ = id;
                scrap::Display::primary().context("resolve primary display")?
            }
            ShareSource::LinuxPortal(id) => {
                let n = id
                    .strip_prefix("screen-")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(1)
                    .max(1);
                displays
                    .into_iter()
                    .nth(n - 1)
                    .or_else(|| scrap::Display::primary().ok())
                    .ok_or_else(|| anyhow!("display source not found: {id}"))?
            }
            ShareSource::X11Window(id) => {
                let _ = id;
                scrap::Display::primary().context("resolve primary display")?
            }
        };

        let width = display.width() as u32;
        let height = display.height() as u32;
        let capturer = scrap::Capturer::new(display).context("create screen capturer")?;
        Ok(Self {
            capturer,
            width,
            height,
        })
    }
}

impl ScreenCapture for ScrapCapture {
    fn next_frame(&mut self) -> anyhow::Result<CapturedFrame> {
        loop {
            match self.capturer.frame() {
                Ok(frame) => {
                    let stride = (frame.len() / self.height as usize) as u32;
                    return Ok(CapturedFrame {
                        width: self.width,
                        height: self.height,
                        stride,
                        ts_ms: unix_ms() as u32,
                        format: PixelFormat::Bgra,
                        data: FrameData::Cpu(Bytes::copy_from_slice(&frame)),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e).context("read captured screen frame"),
            }
        }
    }
}

fn map_share_selection(selection: ShareSourceSelection) -> ShareSource {
    match selection {
        ShareSourceSelection::WindowsDisplay(id) => ShareSource::WindowsDisplay(id),
        ShareSourceSelection::WindowsWindow(id) => ShareSource::WindowsWindow(id),
        ShareSourceSelection::LinuxPortal(token) => ShareSource::LinuxPortal(token),
        ShareSourceSelection::X11Window(window_id) => ShareSource::X11Window(window_id),
    }
}

fn platform_supports_system_audio() -> bool {
    false
}

fn build_screen_capture(source: &ShareSource) -> anyhow::Result<Box<dyn ScreenCapture>> {
    #[cfg(feature = "dev-synthetic-stream")]
    if std::env::var("VP_USE_SYNTHETIC_SCREEN_CAPTURE")
        .ok()
        .as_deref()
        == Some("1")
    {
        return Ok(Box::new(SyntheticCapture::new()));
    }

    #[cfg(target_os = "windows")]
    if matches!(source, ShareSource::WindowsWindow(_)) {
        return Ok(Box::new(WindowsWindowCapture::from_source(source)?));
    }

    #[cfg(target_os = "linux")]
    if matches!(source, ShareSource::LinuxPortal(_)) {
        if std::env::var("WAYLAND_DISPLAY").is_ok()
            || std::env::var("XDG_SESSION_TYPE")
                .map(|v| v.eq_ignore_ascii_case("wayland"))
                .unwrap_or(false)
        {
            warn!(
                "[video] Wayland screen share selected; attempting PipeWire/portal-compatible capture path"
            );
        }
        return Ok(Box::new(ScrapCapture::from_source(source)?));
    }

    Ok(Box::new(ScrapCapture::from_source(source)?))
}

fn build_screen_encoder(codec: &str, _profile: &str) -> anyhow::Result<Box<dyn ScreenEncoder>> {
    match codec {
        "AV1" if cfg!(feature = "video-av1") => Ok(Box::new(Av1AvifEncoder::new())),
        // TODO(video-vp9): replace AVIF-frame fallback with a realtime VP9 encoder.
        "VP9" if cfg!(feature = "video-vp9") => Ok(Box::new(Av1AvifEncoder::new())),
        _ => Err(anyhow!("no screen encoder available for codec {codec}")),
    }
}

#[derive(Clone)]
struct AudioRuntimeSettings {
    output_auto_level: Arc<AtomicBool>,
    mono_expansion: Arc<AtomicBool>,
    comfort_noise: Arc<AtomicBool>,
    comfort_noise_level: Arc<AtomicU32>,
    ducking_enabled: Arc<AtomicBool>,
    ducking_attenuation_db: Arc<AtomicU32>,
    typing_attenuation: Arc<AtomicBool>,
    denoise_attenuation_db: Arc<AtomicU32>,
    fec_mode: Arc<AtomicU32>,
    fec_strength: Arc<AtomicU32>,
}

impl AudioRuntimeSettings {
    fn from_app_settings(settings: &ui::model::AppSettings) -> Self {
        Self {
            output_auto_level: Arc::new(AtomicBool::new(settings.output_auto_level)),
            mono_expansion: Arc::new(AtomicBool::new(settings.mono_expansion)),
            comfort_noise: Arc::new(AtomicBool::new(settings.comfort_noise)),
            comfort_noise_level: Arc::new(AtomicU32::new(f32_to_u32(settings.comfort_noise_level))),
            ducking_enabled: Arc::new(AtomicBool::new(settings.ducking_enabled)),
            ducking_attenuation_db: Arc::new(AtomicU32::new(f32_to_u32(
                settings.ducking_attenuation_db as f32,
            ))),
            typing_attenuation: Arc::new(AtomicBool::new(settings.typing_attenuation)),
            denoise_attenuation_db: Arc::new(AtomicU32::new(f32_to_u32(
                settings.denoise_attenuation_db as f32,
            ))),
            fec_mode: Arc::new(AtomicU32::new(settings.fec_mode as u32)),
            fec_strength: Arc::new(AtomicU32::new(settings.fec_strength as u32)),
        }
    }

    fn apply(&self, settings: &ui::model::AppSettings) {
        self.output_auto_level
            .store(settings.output_auto_level, Ordering::Relaxed);
        self.mono_expansion
            .store(settings.mono_expansion, Ordering::Relaxed);
        self.comfort_noise
            .store(settings.comfort_noise, Ordering::Relaxed);
        self.comfort_noise_level
            .store(f32_to_u32(settings.comfort_noise_level), Ordering::Relaxed);
        self.ducking_enabled
            .store(settings.ducking_enabled, Ordering::Relaxed);
        self.ducking_attenuation_db.store(
            f32_to_u32(settings.ducking_attenuation_db as f32),
            Ordering::Relaxed,
        );
        self.typing_attenuation
            .store(settings.typing_attenuation, Ordering::Relaxed);
        self.denoise_attenuation_db.store(
            f32_to_u32(settings.denoise_attenuation_db as f32),
            Ordering::Relaxed,
        );
        self.fec_mode
            .store(settings.fec_mode as u32, Ordering::Relaxed);
        self.fec_strength
            .store(settings.fec_strength as u32, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct VoiceTelemetryCounters {
    tx_packets: AtomicU64,
    tx_bytes: AtomicU64,
    rx_packets: AtomicU64,
    rx_bytes: AtomicU64,
    late_packets: AtomicU64,
    lost_packets: AtomicU64,
    concealment_frames: AtomicU64,
    tx_oversized_payload_drops: AtomicU64,
    jitter_buffer_depth: AtomicU64,
    peak_stream_level_bits: AtomicU32,
    playout_delay_ms: AtomicU32,
}

#[derive(Default)]
struct SharedNetworkTelemetry {
    rtt_ms: AtomicU32,
    loss_ppm: AtomicU32,
    jitter_ms: AtomicU32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetworkClass {
    Good,
    Moderate,
    Poor,
}

impl NetworkClass {
    /// Returns the target bitrate scaled relative to the channel's configured bitrate.
    /// For high-bitrate music channels this preserves quality instead of collapsing
    /// to the old hardcoded VoIP values (36 / 28 / 20 kbps).
    fn opus_target_bitrate_bps(self, channel_bitrate_bps: u32) -> i32 {
        // Use the channel bitrate (or a sane floor) as the "Good" reference.
        let base = (channel_bitrate_bps as i32).max(32_000);
        match self {
            Self::Good => base,
            Self::Moderate => (base as f32 * 0.75) as i32,
            Self::Poor => (base as f32 * 0.50).max(16_000.0) as i32,
        }
    }

    fn encoder_fec_params(self) -> (bool, i32) {
        match self {
            Self::Good => (false, 8),
            Self::Moderate => (true, 10),
            Self::Poor => (true, 18),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct NetworkSample {
    rtt_ms: u32,
    loss_rate: f32,
    jitter_ms: u32,
    jitter_buffer_depth: u32,
}

#[derive(Debug)]
struct OpusAdaptationController {
    class: NetworkClass,
    pending_class: Option<NetworkClass>,
    pending_samples: u32,
}

impl Default for OpusAdaptationController {
    fn default() -> Self {
        Self {
            class: NetworkClass::Good,
            pending_class: None,
            pending_samples: 0,
        }
    }
}

impl OpusAdaptationController {
    /// Classify network quality from a telemetry sample.
    ///
    /// The classifier uses a weighted-score approach instead of OR-ing individual
    /// thresholds.  Each metric contributes a score of 0–2 and the total is
    /// compared against tier boundaries.  This prevents a single borderline
    /// metric (e.g. 25 ms jitter on a healthy link) from immediately tanking
    /// the bitrate for the entire channel.
    fn classify(&self, sample: NetworkSample) -> NetworkClass {
        let mut score: u32 = 0;

        // Loss: strongest signal of real congestion.
        if sample.loss_rate >= 0.12 {
            score += 4;
        } else if sample.loss_rate >= 0.06 {
            score += 2;
        } else if sample.loss_rate >= 0.03 {
            score += 1;
        }

        // RTT
        if sample.rtt_ms >= 350 {
            score += 2;
        } else if sample.rtt_ms >= 200 {
            score += 1;
        }

        // Jitter
        if sample.jitter_ms >= 80 {
            score += 2;
        } else if sample.jitter_ms >= 40 {
            score += 1;
        }

        // Jitter-buffer depth
        if sample.jitter_buffer_depth >= 12 {
            score += 2;
        } else if sample.jitter_buffer_depth >= 8 {
            score += 1;
        }

        if score >= 5 {
            NetworkClass::Poor
        } else if score >= 3 {
            NetworkClass::Moderate
        } else {
            NetworkClass::Good
        }
    }

    fn promote_threshold(target: NetworkClass) -> u32 {
        match target {
            NetworkClass::Poor => 3,
            NetworkClass::Moderate => 3,
            NetworkClass::Good => 3,
        }
    }

    fn update(&mut self, sample: NetworkSample) -> Option<NetworkClass> {
        let candidate = self.classify(sample);
        if candidate == self.class {
            self.pending_class = None;
            self.pending_samples = 0;
            return None;
        }

        if self.pending_class != Some(candidate) {
            self.pending_class = Some(candidate);
            self.pending_samples = 1;
            return None;
        }

        self.pending_samples = self.pending_samples.saturating_add(1);
        if self.pending_samples >= Self::promote_threshold(candidate) {
            self.class = candidate;
            self.pending_class = None;
            self.pending_samples = 0;
            return Some(candidate);
        }

        None
    }
}

#[derive(Default)]
struct VideoRuntimeCounters {
    video_datagrams: AtomicU64,
    video_tx_datagrams: AtomicU64,
    video_tx_bytes: AtomicU64,
    video_tx_blocked: AtomicU64,
    video_tx_drop_queue_full: AtomicU64,
    video_tx_drop_deadline: AtomicU64,
    voice_tx_drop_queue_full: AtomicU64,
    rx_oversized_datagram_drops: AtomicU64,
    voice_rx_stale_drops: AtomicU64,
    voice_rx_drain_drops: AtomicU64,

    completed_frames: AtomicU64,
    dropped_no_subscription: AtomicU64,
    dropped_channel_full: AtomicU64,
    sender_frame_errors: AtomicU64,
    last_frame_size_bytes: AtomicU64,
    last_frame_seq: AtomicU32,
    last_frame_ts_ms: AtomicU32,
}

#[derive(Clone)]
struct SharedStreamState {
    active_streams: Arc<RwLock<HashMap<u64, Arc<Mutex<VideoReceiver>>>>>,
    stream_codecs: Arc<RwLock<HashMap<u64, pb::VideoCodec>>>,
    counters: Arc<VideoRuntimeCounters>,
    latest_frame: Arc<std::sync::RwLock<Option<ui::model::StreamFrameView>>>,
}

impl SharedStreamState {
    fn new() -> Self {
        Self {
            active_streams: Arc::new(RwLock::new(HashMap::new())),
            stream_codecs: Arc::new(RwLock::new(HashMap::new())),
            counters: Arc::new(VideoRuntimeCounters::default()),
            latest_frame: Arc::new(std::sync::RwLock::new(None)),
        }
    }
}

fn video_codec_name(codec: pb::VideoCodec) -> &'static str {
    match codec {
        pb::VideoCodec::Av1 => "AV1",
        pb::VideoCodec::Vp9 => "VP9",
        pb::VideoCodec::Vp8 => "VP8",
        _ => "UNKNOWN",
    }
}

fn video_codec_encoder_name(codec: pb::VideoCodec) -> Option<&'static str> {
    match codec {
        pb::VideoCodec::Av1 => Some("AV1"),
        pb::VideoCodec::Vp9 => Some("VP9"),
        pb::VideoCodec::Vp8 => Some("VP8"),
        _ => None,
    }
}

fn is_video_datagram(datagram: &Bytes) -> bool {
    datagram.len() >= 2
        && datagram[0] == vp_voice::VIDEO_VERSION
        && datagram[1] == vp_voice::DATAGRAM_KIND_VIDEO
}

async fn datagram_demux_loop(
    conn: quinn::Connection,
    voice_ingress_q: Arc<OverwriteQueue<StampedBytes>>,
    video_tx: mpsc::Sender<Bytes>,
    counters: Arc<VideoRuntimeCounters>,
    voice_stale_drops_total: Arc<AtomicU64>,
    voice_drain_drops_total: Arc<AtomicU64>,
    voice_die_tx: watch::Sender<bool>,
) {
    let mut last_log = Instant::now();
    loop {
        let datagram = match conn.read_datagram().await {
            Ok(d) => d,
            Err(_) => {
                voice_ingress_q.close();
                let _ = voice_die_tx.send(true);
                return;
            }
        };

        if datagram.len() > vp_voice::APP_MEDIA_MTU {
            counters
                .rx_oversized_datagram_drops
                .fetch_add(1, Ordering::Relaxed);
            continue;
        }

        if is_video_datagram(&datagram) {
            if let Err(_e) = video_tx.try_send(datagram) {
                counters
                    .dropped_channel_full
                    .fetch_add(1, Ordering::Relaxed);
                warn!("[video] dropping datagram because video channel is full");
            }
        } else {
            voice_ingress_q.push((Instant::now(), datagram));
        }

        if last_log.elapsed() >= Duration::from_secs(1) {
            let overflow = voice_ingress_q.overflow_evictions_swap();
            let stale = voice_stale_drops_total.swap(0, Ordering::Relaxed);
            let drain = voice_drain_drops_total.swap(0, Ordering::Relaxed);
            if overflow > 0 || stale > 0 || drain > 0 {
                let queue_len = voice_ingress_q.len();
                info!(
                    "[voice] client ingress overflow_evictions/sec={} stale_drops/sec={} drain_drops/sec={} queue_len={}",
                    overflow, stale, drain, queue_len
                );
            }
            last_log = Instant::now();
        }
    }
}

async fn video_recv_loop(mut video_rx: mpsc::Receiver<Bytes>, state: SharedStreamState) {
    while let Some(datagram) = video_rx.recv().await {
        state
            .counters
            .video_datagrams
            .fetch_add(1, Ordering::Relaxed);
        let Some(hdr) = VideoHeader::parse(&datagram) else {
            continue;
        };

        let receiver = {
            let g = state.active_streams.read().await;
            g.get(&hdr.stream_tag).cloned()
        };

        let Some(receiver) = receiver else {
            state
                .counters
                .dropped_no_subscription
                .fetch_add(1, Ordering::Relaxed);
            debug!(
                stream_tag = hdr.stream_tag,
                "[video] drop datagram with no subscription"
            );
            continue;
        };

        let mut rx = receiver.lock().await;
        if let Some(frame) = rx.receive(&datagram) {
            let size = frame.payload.len();
            let codec = {
                state
                    .stream_codecs
                    .read()
                    .await
                    .get(&frame.stream_tag)
                    .copied()
                    .unwrap_or(pb::VideoCodec::Unspecified) as i32
            };
            state
                .counters
                .completed_frames
                .fetch_add(1, Ordering::Relaxed);
            state
                .counters
                .last_frame_size_bytes
                .store(size as u64, Ordering::Relaxed);
            state
                .counters
                .last_frame_seq
                .store(frame.frame_seq, Ordering::Relaxed);
            state
                .counters
                .last_frame_ts_ms
                .store(frame.ts_ms, Ordering::Relaxed);
            if let Ok(mut latest) = state.latest_frame.write() {
                *latest = Some(ui::model::StreamFrameView {
                    stream_tag: frame.stream_tag,
                    frame_seq: frame.frame_seq,
                    ts_ms: frame.ts_ms,
                    codec,
                    payload: frame.payload.to_vec(),
                });
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ChannelAudioMode {
    opus_profile: i32,
    bitrate_bps: u32,
}

impl Default for ChannelAudioMode {
    fn default() -> Self {
        Self {
            opus_profile: pb::OpusProfile::OpusVoice as i32,
            bitrate_bps: 64_000,
        }
    }
}

fn is_music_channel(mode: ChannelAudioMode) -> bool {
    matches!(
        pb::OpusProfile::try_from(mode.opus_profile).ok(),
        Some(pb::OpusProfile::OpusMusic)
    ) || mode.bitrate_bps >= 160_000
}

#[derive(Debug)]
struct MissingWaitController {
    ewma_late_ms: f32,
    ewma_jitter_ms: f32,
    missing_wait_ms: f32,
    last_adjust_log_ms: u64,
    last_logged_wait_ms: f32,
    last_arrival_ms: Option<u64>,
    last_packet_ts_ms: Option<u32>,
}

impl MissingWaitController {
    const MIN_WAIT_MS: f32 = 40.0;
    const MAX_WAIT_MS: f32 = 200.0;
    const ADJUST_ALPHA: f32 = 0.05;

    fn new() -> Self {
        Self {
            ewma_late_ms: 0.0,
            ewma_jitter_ms: 0.0,
            missing_wait_ms: Self::MIN_WAIT_MS,
            last_adjust_log_ms: 0,
            last_logged_wait_ms: Self::MIN_WAIT_MS,
            last_arrival_ms: None,
            last_packet_ts_ms: None,
        }
    }

    fn observe_packet(&mut self, now_ms: u64, packet_ts_ms: u32, frame_ms: u32) {
        if let (Some(last_arrival), Some(last_ts)) = (self.last_arrival_ms, self.last_packet_ts_ms)
        {
            let arrival_delta = now_ms.saturating_sub(last_arrival) as f32;
            let ts_delta = packet_ts_ms.wrapping_sub(last_ts);
            let expected_delta = if ts_delta == 0 {
                frame_ms as f32
            } else {
                ts_delta as f32
            };
            let jitter_ms = (arrival_delta - expected_delta).abs();
            self.ewma_jitter_ms = 0.9 * self.ewma_jitter_ms + 0.1 * jitter_ms;

            let expected_arrival_ms =
                last_arrival.saturating_add(expected_delta.max(frame_ms as f32) as u64);
            let late_ms = now_ms.saturating_sub(expected_arrival_ms) as f32;
            self.ewma_late_ms = 0.9 * self.ewma_late_ms + 0.1 * late_ms;
        }
        self.last_arrival_ms = Some(now_ms);
        self.last_packet_ts_ms = Some(packet_ts_ms);
        self.update_missing_wait(now_ms);
    }

    fn update_missing_wait(&mut self, now_ms: u64) {
        let target = (Self::MIN_WAIT_MS + 2.0 * self.ewma_jitter_ms + self.ewma_late_ms)
            .clamp(Self::MIN_WAIT_MS, Self::MAX_WAIT_MS);
        let prev = self.missing_wait_ms;
        self.missing_wait_ms = prev + (target - prev) * Self::ADJUST_ALPHA;
        if (self.missing_wait_ms - self.last_logged_wait_ms).abs() >= 20.0
            && now_ms.saturating_sub(self.last_adjust_log_ms) >= 1_000
        {
            self.last_adjust_log_ms = now_ms;
            self.last_logged_wait_ms = self.missing_wait_ms;
            info!(
                "[audio] jitter: missing_wait_ms adjusted to {}ms (ewma_jitter={:.1}ms ewma_late={:.1}ms)",
                self.missing_wait_ms.round() as u64,
                self.ewma_jitter_ms,
                self.ewma_late_ms
            );
        }
    }

    fn missing_wait_ms(&self) -> u64 {
        self.missing_wait_ms.round() as u64
    }
}
impl VoiceTelemetryCounters {
    fn observe_peak_stream_level(&self, level: f32) {
        let mut current = self.peak_stream_level_bits.load(Ordering::Relaxed);
        loop {
            let cur = f32::from_bits(current);
            if level <= cur {
                break;
            }
            match self.peak_stream_level_bits.compare_exchange_weak(
                current,
                level.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }
}

fn apply_fec_encoder_settings(
    encoder: &mut audio::opus::OpusEncoder,
    audio_runtime: &AudioRuntimeSettings,
) -> Result<()> {
    let fec_mode = match audio_runtime.fec_mode.load(Ordering::Relaxed) {
        0 => FecMode::Off,
        2 => FecMode::On,
        _ => FecMode::Auto,
    };
    let fec_strength = audio_runtime.fec_strength.load(Ordering::Relaxed).min(100) as i32;
    let enable_fec = fec_mode != FecMode::Off;
    let packet_loss = match fec_mode {
        FecMode::Off => 0,
        FecMode::Auto => fec_strength.clamp(10, 40),
        FecMode::On => fec_strength,
    };
    encoder.set_inband_fec(enable_fec)?;
    encoder.set_packet_loss_perc(packet_loss)?;
    info!(
        "[audio] set fec={:?} strength={} encoder_inband_fec={} packet_loss_perc={}",
        fec_mode, fec_strength, enable_fec, packet_loss
    );
    Ok(())
}

fn apply_network_class_encoder_settings(
    encoder: &mut audio::opus::OpusEncoder,
    class: NetworkClass,
    channel_bitrate_bps: u32,
) -> Result<()> {
    let bitrate = class.opus_target_bitrate_bps(channel_bitrate_bps);
    let (enable_fec, loss_perc) = class.encoder_fec_params();
    encoder.set_bitrate(bitrate)?;
    encoder.set_inband_fec(enable_fec)?;
    encoder.set_packet_loss_perc(loss_perc)?;
    info!(
        "[audio] network_class={class:?} channel_bitrate={} apply opus bitrate={} fec={} packet_loss_perc={}",
        channel_bitrate_bps, bitrate, enable_fec, loss_perc
    );
    Ok(())
}

fn persist_settings(tx_event: &Sender<UiEvent>, settings: &ui::model::AppSettings) {
    if let Err(e) = settings_io::save_settings(settings) {
        let _ = tx_event.send(UiEvent::AppendLog(format!("[settings] save failed: {e:#}")));
    }
}

fn capture_mode_to_u8(mode: ui::model::CaptureMode) -> u8 {
    match mode {
        ui::model::CaptureMode::PushToTalk => 0,
        ui::model::CaptureMode::VoiceActivation => 1,
        ui::model::CaptureMode::Continuous => 2,
    }
}

fn capture_mode_from_u8(mode: u8) -> ui::model::CaptureMode {
    match mode {
        0 => ui::model::CaptureMode::PushToTalk,
        2 => ui::model::CaptureMode::Continuous,
        _ => ui::model::CaptureMode::VoiceActivation,
    }
}

fn apply_resampler_mode(mode: DspMethod) {
    std::env::set_var("VP_AUDIO_RESAMPLER", mode.label());
}

fn select_gui_renderer() -> eframe::Renderer {
    let default_renderer = eframe::Renderer::default();
    match std::env::var("VP_GUI_RENDERER") {
        Ok(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            let alias = match normalized.as_str() {
                "gl" | "opengl" => "glow",
                "gpu" | "hardware" => "wgpu",
                "auto" | "default" => "wgpu",
                other => other,
            };

            match alias.parse::<eframe::Renderer>() {
                Ok(renderer) => {
                    info!("[gui] forcing {renderer} renderer via VP_GUI_RENDERER={value}");
                    renderer
                }
                Err(_) => {
                    warn!(
                        "[gui] unsupported VP_GUI_RENDERER value '{value}'; falling back to renderer ({default_renderer})"
                    );
                    default_renderer
                }
            }
        }
        Err(_) => default_renderer,
    }
}

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
        renderer: select_gui_renderer(),
        viewport: egui::ViewportBuilder::default()
            .with_title("TSOD")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 500.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    let max_upload_mb = cfg.max_upload_mb;

    let gui_result = eframe::run_native(
        "TSOD",
        native_options,
        Box::new(move |cc| Ok(Box::new(VpApp::new(cc, tx_intent, rx_event, max_upload_mb)))),
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

fn set_connection_stage(
    tx_event: &Sender<UiEvent>,
    stage: ui::model::ConnectionStage,
    detail: impl Into<String>,
) {
    let detail = detail.into();
    let _ = tx_event.send(UiEvent::SetConnectionStage {
        stage,
        detail: detail.clone(),
    });
    let _ = tx_event.send(UiEvent::AppendLog(format!("[conn] {detail}")));
}

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
        if cfg.ca_cert_pem.is_empty() {
            "(insecure dev mode)"
        } else {
            &cfg.ca_cert_pem
        }
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
    let capture_modes = audio::capture::enumerate_capture_modes();
    let playback_modes = audio::playout::enumerate_playback_modes();
    let _ = tx_event.send(UiEvent::SetAudioDevices {
        input_devices: input_devices.clone(),
        output_devices: output_devices.clone(),
        capture_modes,
        playback_modes,
    });

    // Load persisted settings and send to UI
    let mut saved_settings = settings_io::load_settings();
    settings_io::migrate_audio_device_ids(&mut saved_settings, &input_devices, &output_devices);
    if !saved_settings.identity_nickname.trim().is_empty() {
        cfg.display_name = saved_settings.identity_nickname.trim().to_string();
        let _ = tx_event.send(UiEvent::SetNick(cfg.display_name.clone()));
    }
    if !saved_settings.last_server_host.trim().is_empty() {
        cfg.server = format!(
            "{}:{}",
            saved_settings.last_server_host.trim(),
            saved_settings.last_server_port
        );
        cfg.server_name = saved_settings.last_server_host.trim().to_string();
        let _ = tx_event.send(UiEvent::SetServerAddress {
            host: saved_settings.last_server_host.trim().to_string(),
            port: saved_settings.last_server_port,
        });
    }
    let _ = tx_event.send(UiEvent::SettingsLoaded(Box::new(saved_settings.clone())));

    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            let _ = tx_event.send(UiEvent::AppendLog(
                "[hotkeys] Global PTT hotkeys are disabled on Wayland in this build (compositor integration is required)."
                    .to_string(),
            ));
        }
    }

    ptt_active.store(
        saved_settings.capture_mode != ui::model::CaptureMode::PushToTalk,
        Ordering::Relaxed,
    );
    let mut ptt_state = PttState {
        pressed: false,
        release_deadline: None,
    };
    let capture_mode = Arc::new(AtomicU8::new(capture_mode_to_u8(
        saved_settings.capture_mode,
    )));

    let audio_runtime = AudioRuntimeSettings::from_app_settings(&saved_settings);
    let dsp_enabled = Arc::new(AtomicBool::new(
        saved_settings.dsp_enabled && !cfg.no_noise_suppression,
    ));
    apply_resampler_mode(saved_settings.dsp_method);

    // Audio constants
    let sample_rate = 48_000u32;
    let channels = 1u16;
    let frame_ms = 20u32;

    let selected_audio = Arc::new(Mutex::new(AudioSelection {
        input_device: saved_settings.capture_device.clone(),
        output_device: saved_settings.playback_device.clone(),
        capture_mode: normalize_capture_mode(&saved_settings.capture_backend_mode),
        playback_mode: normalize_playback_mode(&saved_settings.playback_mode),
    }));

    // Audio pipeline
    let encoder = Arc::new(Mutex::new(audio::opus::OpusEncoder::new(
        sample_rate,
        channels as u8,
        audio::opus::OpusEncoderProfile::Voice,
    )?));
    {
        let mut enc = encoder.lock().await;
        let _ = apply_fec_encoder_settings(&mut enc, &audio_runtime);
    }

    let initial_selection = selected_audio.lock().await.clone();
    let capture = Arc::new(RwLock::new(Arc::new(start_capture_with_fallback(
        sample_rate,
        channels,
        frame_ms,
        preferred_device_id(&initial_selection.input_device),
        initial_selection.capture_mode.as_deref(),
        &tx_event,
    )?)));
    let playout = Arc::new(RwLock::new(Arc::new(start_playout_with_fallback(
        sample_rate,
        channels,
        preferred_device_id(&initial_selection.output_device),
        initial_selection.playback_mode.as_deref(),
        &tx_event,
    )?)));

    // DSP pipeline
    let capture_dsp = if !cfg.no_noise_suppression {
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
        d.set_agc_preset(saved_settings.agc_preset);
        d.set_agc_target(saved_settings.agc_target_db);
        d.set_echo_cancellation(saved_settings.echo_cancellation);
        d.set_echo_reference_enabled(should_enable_aec_reference(&saved_settings.playback_device));
    }

    // Shared self-mute/deafen state for the audio pipeline
    let self_muted = Arc::new(AtomicBool::new(false));
    let self_deafened = Arc::new(AtomicBool::new(false));
    let server_deafened = Arc::new(AtomicBool::new(false));

    // Shared gain values (stored as u32 bits of f32)
    let input_gain = Arc::new(std::sync::atomic::AtomicU32::new(f32_to_u32(1.0)));
    let output_gain = Arc::new(std::sync::atomic::AtomicU32::new(f32_to_u32(1.0)));
    input_gain.store(f32_to_u32(saved_settings.input_gain), Ordering::Relaxed);
    output_gain.store(f32_to_u32(saved_settings.output_gain), Ordering::Relaxed);
    let per_user_audio = Arc::new(std::sync::RwLock::new(
        saved_settings.per_user_audio.clone(),
    ));
    let loopback_active = Arc::new(AtomicBool::new(false));
    let session_voice_active = Arc::new(AtomicBool::new(false));
    let active_voice_channel_route = Arc::new(AtomicU32::new(0));
    let active_channel_audio_mode = Arc::new(std::sync::RwLock::new(ChannelAudioMode::default()));
    let voice_counters = Arc::new(VoiceTelemetryCounters::default());
    let send_queue_drop_count = Arc::new(AtomicU32::new(0));
    let network_telemetry = Arc::new(SharedNetworkTelemetry::default());

    let _telemetry = tokio::spawn(emit_telemetry_loop(
        tx_event.clone(),
        capture_dsp.clone(),
        dsp_enabled.clone(),
        voice_counters.clone(),
        network_telemetry.clone(),
        send_queue_drop_count.clone(),
        running.clone(),
        shutdown_rx.clone(),
    ));

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
            encoder.clone(),
            capture.clone(),
            playout.clone(),
            capture_dsp.clone(),
            dsp_enabled.clone(),
            active_voice_channel_route.clone(),
            active_channel_audio_mode.clone(),
            selected_audio.clone(),
            ptt_active.clone(),
            &mut ptt_state,
            capture_mode.clone(),
            self_muted.clone(),
            self_deafened.clone(),
            server_deafened.clone(),
            input_gain.clone(),
            output_gain.clone(),
            per_user_audio.clone(),
            loopback_active.clone(),
            session_voice_active.clone(),
            voice_counters.clone(),
            network_telemetry.clone(),
            send_queue_drop_count.clone(),
            audio_runtime.clone(),
            sample_rate,
            channels,
            frame_ms,
            &mut shutdown_rx,
            &mut saved_settings,
        )
        .await
        {
            Ok(()) => {
                backoff.reset();
            }
            Err(e) => {
                set_connection_stage(
                    &tx_event,
                    ui::model::ConnectionStage::Failed,
                    format!("Connection failed: {e:#}"),
                );
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
                                saved_settings.input_gain = gain;
                                input_gain.store(f32_to_u32(gain), Ordering::Relaxed);
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetEchoCancellation(enabled) => {
                                saved_settings.echo_cancellation = enabled;
                                if let Some(ref dsp) = capture_dsp {
                                    let mut d = dsp.lock().await;
                                    d.set_echo_cancellation(enabled);
                                }
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetVoiceProcessingMode(mode) => {
                                saved_settings.voice_processing_mode = mode;
                                mode.apply_to_settings(&mut saved_settings);
                                dsp_enabled.store(
                                    saved_settings.dsp_enabled && !cfg.no_noise_suppression,
                                    Ordering::Relaxed,
                                );
                                if let Some(ref dsp) = capture_dsp {
                                    let mut d = dsp.lock().await;
                                    d.set_noise_suppression(saved_settings.noise_suppression);
                                    d.set_agc(saved_settings.agc_enabled);
                                    d.set_agc_preset(saved_settings.agc_preset);
                                    d.set_agc_target(saved_settings.agc_target_db);
                                }
                                audio_runtime
                                    .fec_mode
                                    .store(saved_settings.fec_mode as u32, Ordering::Relaxed);
                                audio_runtime
                                    .fec_strength
                                    .store(saved_settings.fec_strength as u32, Ordering::Relaxed);
                                let bitrate = active_channel_audio_mode
                                    .read()
                                    .map(|mode| mode.bitrate_bps)
                                    .unwrap_or(64_000);
                                let mut enc = encoder.lock().await;
                                match audio::opus::OpusEncoder::new(
                                    sample_rate,
                                    channels as u8,
                                    encoder_profile_for_mode(saved_settings.voice_processing_mode),
                                ) {
                                    Ok(mut new_encoder) => {
                                        let _ = new_encoder.set_bitrate(bitrate as i32);
                                        let _ = apply_fec_encoder_settings(
                                            &mut new_encoder,
                                            &audio_runtime,
                                        );
                                        *enc = new_encoder;
                                    }
                                    Err(e) => {
                                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                                            "[audio] failed to reconfigure encoder profile: {e:#}"
                                        )));
                                    }
                                }
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetDspEnabled(enabled) => {
                                saved_settings.dsp_enabled = enabled;
                                dsp_enabled
                                    .store(enabled && !cfg.no_noise_suppression, Ordering::Relaxed);
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetDspMethod(method) => {
                                saved_settings.dsp_method = method;
                                apply_resampler_mode(method);
                                if let Err(e) = restart_audio_streams(
                                    &capture,
                                    &playout,
                                    &selected_audio,
                                    &tx_event,
                                    sample_rate,
                                    channels,
                                    frame_ms,
                                )
                                .await
                                {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[audio] failed to switch DSP method: {e:#}"
                                    )));
                                }
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetNoiseSuppression(enabled) => {
                                saved_settings.noise_suppression = enabled;
                                if let Some(ref dsp) = capture_dsp {
                                    let mut d = dsp.lock().await;
                                    d.set_noise_suppression(enabled);
                                }
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetAgcEnabled(enabled) => {
                                saved_settings.agc_enabled = enabled;
                                if let Some(ref dsp) = capture_dsp {
                                    let mut d = dsp.lock().await;
                                    d.set_agc(enabled);
                                }
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetAgcPreset(preset) => {
                                saved_settings.agc_preset = preset;
                                saved_settings.agc_target_db = preset.target_db();
                                if let Some(ref dsp) = capture_dsp {
                                    let mut d = dsp.lock().await;
                                    d.set_agc_preset(preset);
                                }
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetAgcTargetDb(target_db) => {
                                saved_settings.agc_target_db = target_db;
                                if let Some(ref dsp) = capture_dsp {
                                    let mut d = dsp.lock().await;
                                    d.set_agc_target(target_db);
                                }
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetTypingAttenuation(enabled) => {
                                saved_settings.typing_attenuation = enabled;
                                audio_runtime
                                    .typing_attenuation
                                    .store(enabled, Ordering::Relaxed);
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetFecMode(mode) => {
                                saved_settings.fec_mode = mode;
                                audio_runtime.fec_mode.store(mode as u32, Ordering::Relaxed);
                                let mut enc = encoder.lock().await;
                                let _ = apply_fec_encoder_settings(&mut enc, &audio_runtime);
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetFecStrength(strength) => {
                                saved_settings.fec_strength = strength.min(100);
                                audio_runtime
                                    .fec_strength
                                    .store(saved_settings.fec_strength as u32, Ordering::Relaxed);
                                let mut enc = encoder.lock().await;
                                let _ = apply_fec_encoder_settings(&mut enc, &audio_runtime);
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetVadThreshold(threshold) => {
                                saved_settings.vad_threshold = threshold;
                                if let Some(ref dsp) = capture_dsp {
                                    let mut d = dsp.lock().await;
                                    d.set_vad_threshold(threshold);
                                }
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetOutputGain(gain) => {
                                saved_settings.output_gain = gain;
                                output_gain.store(f32_to_u32(gain), Ordering::Relaxed);
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetOutputAutoLevel(enabled) => {
                                saved_settings.output_auto_level = enabled;
                                audio_runtime
                                    .output_auto_level
                                    .store(enabled, Ordering::Relaxed);
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetMonoExpansion(enabled) => {
                                saved_settings.mono_expansion = enabled;
                                audio_runtime
                                    .mono_expansion
                                    .store(enabled, Ordering::Relaxed);
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetComfortNoise(enabled) => {
                                saved_settings.comfort_noise = enabled;
                                audio_runtime
                                    .comfort_noise
                                    .store(enabled, Ordering::Relaxed);
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetComfortNoiseLevel(level) => {
                                saved_settings.comfort_noise_level = level.clamp(0.0, 0.1);
                                audio_runtime.comfort_noise_level.store(
                                    f32_to_u32(saved_settings.comfort_noise_level),
                                    Ordering::Relaxed,
                                );
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetDuckingEnabled(enabled) => {
                                saved_settings.ducking_enabled = enabled;
                                audio_runtime
                                    .ducking_enabled
                                    .store(enabled, Ordering::Relaxed);
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetDuckingAttenuationDb(db) => {
                                saved_settings.ducking_attenuation_db = db.clamp(-40, 0);
                                audio_runtime.ducking_attenuation_db.store(
                                    f32_to_u32(saved_settings.ducking_attenuation_db as f32),
                                    Ordering::Relaxed,
                                );
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetInputDevice(dev) => {
                                {
                                    let mut state = selected_audio.lock().await;
                                    state.input_device = dev;
                                }
                                if let Err(e) = restart_audio_streams(
                                    &capture,
                                    &playout,
                                    &selected_audio,
                                    &tx_event,
                                    sample_rate,
                                    channels,
                                    frame_ms,
                                )
                                .await
                                {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[audio] failed to switch input device: {e:#}"
                                    )));
                                }
                            }
                            UiIntent::SetOutputDevice(dev) => {
                                {
                                    let mut state = selected_audio.lock().await;
                                    state.output_device = dev;
                                    if let Some(ref dsp) = capture_dsp {
                                        let mut d = dsp.lock().await;
                                        d.set_echo_reference_enabled(should_enable_aec_reference(
                                            &state.output_device,
                                        ));
                                    }
                                }
                                if let Err(e) = restart_audio_streams(
                                    &capture,
                                    &playout,
                                    &selected_audio,
                                    &tx_event,
                                    sample_rate,
                                    channels,
                                    frame_ms,
                                )
                                .await
                                {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[audio] failed to switch output device: {e:#}"
                                    )));
                                }
                            }
                            UiIntent::SetCaptureMode(mode) => {
                                {
                                    let mut state = selected_audio.lock().await;
                                    state.capture_mode = normalize_capture_mode(&mode);
                                }

                                if let Err(e) = restart_audio_streams(
                                    &capture,
                                    &playout,
                                    &selected_audio,
                                    &tx_event,
                                    sample_rate,
                                    channels,
                                    frame_ms,
                                )
                                .await
                                {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[audio] failed to switch capture mode: {e:#}"
                                    )));
                                }
                            }
                            UiIntent::SaveSettings(ref settings) => {
                                saved_settings = (**settings).clone();
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::ConnectToServer {
                                host,
                                port,
                                nickname,
                            } => {
                                cfg.server = format!("{host}:{port}");
                                cfg.server_name = host.clone();
                                cfg.display_name = nickname.clone();
                                let _ = tx_event.send(UiEvent::SetNick(nickname.clone()));
                                let _ = tx_event.send(UiEvent::SetServerAddress { host, port });
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[net] target server updated: {}",
                                    cfg.server
                                )));
                                break 'retry_wait;
                            }
                            UiIntent::CancelConnect => {
                                set_connection_stage(
                                    &tx_event,
                                    ui::model::ConnectionStage::Idle,
                                    "Connection attempt cancelled",
                                );
                            }
                            UiIntent::SetPlaybackMode(mode) => {
                                {
                                    let mut state = selected_audio.lock().await;
                                    state.playback_mode = normalize_playback_mode(&mode);
                                }

                                if let Err(e) = restart_audio_streams(
                                    &capture,
                                    &playout,
                                    &selected_audio,
                                    &tx_event,
                                    sample_rate,
                                    channels,
                                    frame_ms,
                                )
                                .await
                                {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[audio] failed to switch playback mode: {e:#}"
                                    )));
                                }
                            }
                            UiIntent::StartScreenShare { .. } => {}
                            UiIntent::StopScreenShare => {}
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

fn maybe_note_event_gap(_tx_event: &Sender<UiEvent>, _event_seq: u64) {
    // event_seq == 0 means the server did not stamp this push with a sequence
    // number; it is treated as unordered and always applied. No user-visible
    // log entry is emitted here — gap detection for stamped events is handled
    // inside should_apply_event_seq.
}

fn should_apply_event_seq(
    tx_event: &Sender<UiEvent>,
    last_event_seq: &mut u64,
    event_seq: u64,
) -> bool {
    if event_seq == 0 {
        // Server did not stamp this event; apply unconditionally.
        return true;
    }
    if event_seq <= *last_event_seq {
        let _ = tx_event.send(UiEvent::AppendLog(format!(
            "[sync] ignoring stale push event_seq={} <= last_event_seq={}",
            event_seq, *last_event_seq
        )));
        return false;
    }
    if *last_event_seq != 0 && event_seq > *last_event_seq + 1 {
        let _ = tx_event.send(UiEvent::AppendLog(format!(
            "[sync] event sequence gap detected: expected {} got {} (missed {} events)",
            *last_event_seq + 1,
            event_seq,
            event_seq - *last_event_seq - 1,
        )));
    }
    *last_event_seq = event_seq;
    let _ = tx_event.send(UiEvent::SetLastEventSeq(event_seq));
    true
}

fn pb_channel_type_to_ui(channel_type: i32) -> ui::model::ChannelType {
    match pb::ChannelType::try_from(channel_type).ok() {
        Some(pb::ChannelType::Text) => ui::model::ChannelType::Text,
        Some(pb::ChannelType::Streaming) => ui::model::ChannelType::Streaming,
        Some(pb::ChannelType::Category) => ui::model::ChannelType::Category,
        _ => ui::model::ChannelType::Voice,
    }
}

fn ui_status_from_pb(status: i32) -> ui::model::OnlineStatus {
    match pb::OnlineStatus::try_from(status).ok() {
        Some(pb::OnlineStatus::Online) => ui::model::OnlineStatus::Online,
        Some(pb::OnlineStatus::Idle) => ui::model::OnlineStatus::Idle,
        Some(pb::OnlineStatus::DoNotDisturb) => ui::model::OnlineStatus::DoNotDisturb,
        Some(pb::OnlineStatus::Invisible) => ui::model::OnlineStatus::Invisible,
        Some(pb::OnlineStatus::Offline) => ui::model::OnlineStatus::Offline,
        _ => ui::model::OnlineStatus::Online,
    }
}

fn opus_profile_from_pb(opus_profile: i32) -> audio::opus::OpusEncoderProfile {
    match pb::OpusProfile::try_from(opus_profile).ok() {
        Some(pb::OpusProfile::OpusMusic) => audio::opus::OpusEncoderProfile::Music,
        _ => audio::opus::OpusEncoderProfile::Voice,
    }
}

fn encoder_profile_for_mode(
    mode: ui::model::VoiceProcessingMode,
) -> audio::opus::OpusEncoderProfile {
    match mode {
        ui::model::VoiceProcessingMode::Music => audio::opus::OpusEncoderProfile::Music,
        _ => audio::opus::OpusEncoderProfile::Voice,
    }
}

fn apply_authoritative_snapshot(
    snapshot: &pb::InitialStateSnapshot,
    tx_event: &Sender<UiEvent>,
    requested_channel_id: Option<&str>,
) {
    let channels = snapshot
        .channels
        .iter()
        .filter_map(|ch| ch.info.as_ref())
        .map(|info| ui::model::ChannelEntry {
            id: info
                .channel_id
                .as_ref()
                .map(|id| id.value.clone())
                .unwrap_or_default(),
            name: info.name.clone(),
            channel_type: pb_channel_type_to_ui(info.channel_type),
            parent_id: info.parent_channel_id.as_ref().map(|pid| pid.value.clone()),
            position: info.position,
            member_count: 0,
            user_limit: info.user_limit,
            description: info.description.clone(),
            bitrate_bps: info.bitrate,
            opus_profile: info.opus_profile,
        })
        .collect::<Vec<_>>();

    let _ = tx_event.send(UiEvent::SetChannels(channels.clone()));
    let _ = tx_event.send(UiEvent::SetDefaultChannelId(
        snapshot
            .default_channel_id
            .as_ref()
            .map(|channel_id| channel_id.value.clone()),
    ));
    let _ = tx_event.send(UiEvent::SetLastEventSeq(snapshot.snapshot_version));

    for scope in &snapshot.channel_members {
        let channel_id = scope
            .channel_id
            .as_ref()
            .map(|id| id.value.clone())
            .unwrap_or_default();
        let members = scope
            .members
            .iter()
            .map(|m| ui::model::MemberEntry {
                user_id: m
                    .user_id
                    .as_ref()
                    .map(|u| u.value.clone())
                    .unwrap_or_default(),
                display_name: m.display_name.clone(),
                away_message: String::new(),
                muted: m.muted,
                deafened: m.deafened,
                self_muted: m.self_muted,
                self_deafened: m.self_deafened,
                streaming: m.streaming,
                speaking: false,
                avatar_url: None,
            })
            .collect::<Vec<_>>();
        let _ = tx_event.send(UiEvent::UpdateChannelMembers {
            channel_id,
            members,
        });
    }

    let selected = choose_initial_selected_channel(snapshot, requested_channel_id);
    if let Some(selected_channel) = selected {
        let _ = tx_event.send(UiEvent::SetChannelName(selected_channel));
    }

    let _ = tx_event.send(UiEvent::AppendLog(format!(
        "[sync] authoritative snapshot applied server_id={} auth_user_id={} channels={} member_scopes={} members_semantics=selected-channel scoped",
        snapshot.server_id.as_ref().map(|sid| sid.value.clone()).unwrap_or_default(),
        snapshot.self_user_id.as_ref().map(|u| u.value.clone()).unwrap_or_default(),
        snapshot.channels.len(),
        snapshot.channel_members.len(),
    )));
}

fn choose_initial_selected_channel(
    snapshot: &pb::InitialStateSnapshot,
    requested_channel_id: Option<&str>,
) -> Option<String> {
    if let Some(requested) = requested_channel_id {
        if snapshot.channels.iter().any(|channel| {
            channel
                .info
                .as_ref()
                .and_then(|info| info.channel_id.as_ref())
                .is_some_and(|cid| cid.value == requested)
        }) {
            return Some(requested.to_string());
        }
    }

    snapshot
        .default_channel_id
        .as_ref()
        .map(|id| id.value.clone())
        .or_else(|| {
            snapshot
                .channels
                .first()
                .and_then(|channel| channel.info.as_ref())
                .and_then(|info| info.channel_id.as_ref())
                .map(|id| id.value.clone())
        })
}

#[derive(Clone, Debug)]
struct AudioSelection {
    input_device: AudioDeviceId,
    output_device: AudioDeviceId,
    capture_mode: Option<String>,
    playback_mode: Option<String>,
}

fn preferred_device_id(device: &AudioDeviceId) -> Option<&str> {
    if device.is_default() {
        None
    } else {
        Some(device.id.as_str())
    }
}

fn should_enable_aec_reference(device: &AudioDeviceId) -> bool {
    let id = device.id.to_ascii_lowercase();
    let looks_like_headset = ["headset", "headphone", "earbud", "airpods"]
        .iter()
        .any(|needle| id.contains(needle));
    !looks_like_headset
}

fn normalize_capture_mode(mode: &str) -> Option<String> {
    let trimmed = mode.trim();
    if trimmed.is_empty() || trimmed == audio::capture::CAPTURE_MODE_AUTO {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_playback_mode(mode: &str) -> Option<String> {
    let trimmed = mode.trim();
    if trimmed.is_empty() || trimmed == audio::playout::PLAYBACK_MODE_AUTO {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn start_capture_with_fallback(
    sample_rate: u32,
    channels: u16,
    frame_ms: u32,
    preferred_device: Option<&str>,
    preferred_mode: Option<&str>,
    tx_event: &Sender<UiEvent>,
) -> Result<audio::capture::Capture> {
    if let Some(device) = preferred_device {
        info!("audio open input by id: {device}");
        match audio::capture::Capture::start_with_mode(
            sample_rate,
            channels,
            frame_ms,
            Some(device),
            preferred_mode,
            Some(tx_event.clone()),
        ) {
            Ok(capture) => return Ok(capture),
            Err(e) => {
                warn!(
                    "[audio] open input by id failed: {device} err={e:#}; falling back to default"
                );
                let _ = tx_event.send(UiEvent::AppendLog(format!(
                    "[audio] open input by id failed: {device} err={e:#}; falling back to default"
                )));
            }
        }
    }
    match audio::capture::Capture::start_with_mode(
        sample_rate,
        channels,
        frame_ms,
        None,
        preferred_mode,
        Some(tx_event.clone()),
    ) {
        Ok(capture) => Ok(capture),
        Err(e) => {
            let _ = tx_event.send(UiEvent::AppendLog(format!(
                "[audio] open input default failed: err={e:#}"
            )));
            Err(e)
        }
    }
}

fn start_playout_with_fallback(
    sample_rate: u32,
    channels: u16,
    preferred_device: Option<&str>,
    preferred_mode: Option<&str>,
    tx_event: &Sender<UiEvent>,
) -> Result<audio::playout::Playout> {
    if let Some(device) = preferred_device {
        info!("audio open output by id: {device}");
        match audio::playout::Playout::start_with_mode(
            sample_rate,
            channels,
            Some(device),
            preferred_mode,
            Some(tx_event.clone()),
        ) {
            Ok(playout) => return Ok(playout),
            Err(e) => {
                warn!(
                    "[audio] open output by id failed: {device} err={e:#}; falling back to default"
                );
                let _ = tx_event.send(UiEvent::AppendLog(format!(
                    "[audio] open output by id failed: {device} err={e:#}; falling back to default"
                )));
            }
        }
    }
    match audio::playout::Playout::start_with_mode(
        sample_rate,
        channels,
        None,
        preferred_mode,
        Some(tx_event.clone()),
    ) {
        Ok(playout) => Ok(playout),
        Err(e) => {
            let _ = tx_event.send(UiEvent::AppendLog(format!(
                "[audio] open output default failed: err={e:#}"
            )));
            Err(e)
        }
    }
}

async fn restart_audio_streams(
    capture: &Arc<RwLock<Arc<audio::capture::Capture>>>,
    playout: &Arc<RwLock<Arc<audio::playout::Playout>>>,
    selection: &Arc<Mutex<AudioSelection>>,
    tx_event: &Sender<UiEvent>,
    sample_rate: u32,
    channels: u16,
    frame_ms: u32,
) -> Result<()> {
    let selected = selection.lock().await.clone();
    let preferred_input = preferred_device_id(&selected.input_device);
    let preferred_output = preferred_device_id(&selected.output_device);
    let preferred_capture_mode = selected.capture_mode.as_deref();
    let preferred_mode = selected.playback_mode.as_deref();
    let input_label = resolve_device_label(&selected.input_device, true);
    let output_label = resolve_device_label(&selected.output_device, false);

    info!(
        "switch input -> {:?} {} ({})",
        selected.input_device.backend, selected.input_device.id, input_label
    );
    info!(
        "switch output -> {:?} {} ({})",
        selected.output_device.backend, selected.output_device.id, output_label
    );
    let _ = tx_event.send(UiEvent::AppendLog(format!(
        "[audio] switch input -> {:?} {} ({})",
        selected.input_device.backend, selected.input_device.id, input_label
    )));
    let _ = tx_event.send(UiEvent::AppendLog(format!(
        "[audio] switch output -> {:?} {} ({})",
        selected.output_device.backend, selected.output_device.id, output_label
    )));

    let new_capture = start_capture_with_fallback(
        sample_rate,
        channels,
        frame_ms,
        preferred_input,
        preferred_capture_mode,
        tx_event,
    )
    .context("restart capture")?;
    let new_playout = start_playout_with_fallback(
        sample_rate,
        channels,
        preferred_output,
        preferred_mode,
        tx_event,
    )
    .context("restart playout")?;

    {
        let mut cap = capture.write().await;
        *cap = Arc::new(new_capture);
    }
    {
        let mut out = playout.write().await;
        *out = Arc::new(new_playout);
    }

    let _ = tx_event.send(UiEvent::AppendLog(format!(
        "[audio] streams restarted (input={}, output={}, capture_mode={}, playback_mode={})",
        preferred_device_id(&selected.input_device).unwrap_or("(system default)"),
        preferred_device_id(&selected.output_device).unwrap_or("(system default)"),
        selected
            .capture_mode
            .as_deref()
            .unwrap_or(audio::capture::CAPTURE_MODE_AUTO),
        selected
            .playback_mode
            .as_deref()
            .unwrap_or(audio::playout::PLAYBACK_MODE_AUTO)
    )));

    Ok(())
}

fn resolve_device_label(device: &AudioDeviceId, input: bool) -> String {
    if device.is_default() {
        return "Default (system)".to_string();
    }
    let all = if input {
        audio::capture::enumerate_input_devices()
    } else {
        audio::playout::enumerate_output_devices()
    };
    all.into_iter()
        .find(|d| d.key == *device)
        .map(|d| d.display_label)
        .unwrap_or_else(|| "Unknown device".to_string())
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
    encoder: Arc<Mutex<audio::opus::OpusEncoder>>,
    capture: Arc<RwLock<Arc<audio::capture::Capture>>>,
    playout: Arc<RwLock<Arc<audio::playout::Playout>>>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    dsp_enabled: Arc<AtomicBool>,
    active_voice_channel_route: Arc<AtomicU32>,
    active_channel_audio_mode: Arc<std::sync::RwLock<ChannelAudioMode>>,
    selected_audio: Arc<Mutex<AudioSelection>>,
    ptt_active: Arc<AtomicBool>,
    ptt_state: &mut PttState,
    capture_mode: Arc<AtomicU8>,
    self_muted: Arc<AtomicBool>,
    self_deafened: Arc<AtomicBool>,
    server_deafened: Arc<AtomicBool>,
    input_gain: Arc<std::sync::atomic::AtomicU32>,
    output_gain: Arc<std::sync::atomic::AtomicU32>,
    per_user_audio: Arc<std::sync::RwLock<HashMap<String, PerUserAudioSettings>>>,
    loopback_active: Arc<AtomicBool>,
    session_voice_active: Arc<AtomicBool>,
    voice_counters: Arc<VoiceTelemetryCounters>,
    network_telemetry: Arc<SharedNetworkTelemetry>,
    send_queue_drop_count: Arc<AtomicU32>,
    audio_runtime: AudioRuntimeSettings,
    sample_rate: u32,
    channels: u16,
    frame_ms: u32,
    shutdown_rx: &mut watch::Receiver<bool>,
    saved_settings: &mut ui::model::AppSettings,
) -> Result<()> {
    let _ = tx_event.send(UiEvent::SetConnected(false));
    let _ = tx_event.send(UiEvent::SetAuthed(false));
    server_deafened.store(false, Ordering::Relaxed);

    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Resolving,
        format!("Connect requested for {}", cfg.server),
    );
    let resolve_started = Instant::now();
    let endpoint = make_endpoint_with_optional_pinning(cfg)?;
    let addr = cfg.server.parse().context("parse server addr")?;
    let resolve_elapsed = resolve_started.elapsed();
    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Resolving,
        format!("Host/addr prepared in {} ms", resolve_elapsed.as_millis()),
    );

    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Handshaking,
        format!("Establishing QUIC/TLS to {}", cfg.server_name),
    );
    let handshake_started = Instant::now();
    let conn = endpoint
        .connect(addr, &cfg.server_name)
        .context("connect start")?
        .await
        .context("connect await")?;
    let handshake_elapsed = handshake_started.elapsed();

    let _ = tx_event.send(UiEvent::SetConnected(true));
    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Handshaking,
        format!(
            "QUIC/TLS established in {} ms",
            handshake_elapsed.as_millis()
        ),
    );

    let (ui_log_tx, mut ui_log_rx) = mpsc::unbounded_channel::<String>();
    let tx_event_log = tx_event.clone();
    tokio::spawn(async move {
        while let Some(line) = ui_log_rx.recv().await {
            let _ = tx_event_log.send(UiEvent::AppendLog(line));
        }
    });

    let (send, recv) = conn.open_bi().await.context("open control stream")?;
    let dispatcher = ControlDispatcher::start(send, recv, shutdown_rx.clone(), ui_log_tx.clone());

    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Authenticating,
        "Authenticating with gateway",
    );
    let device_identity =
        DeviceIdentity::load_or_create().context("load/create device identity")?;
    let auth_started = Instant::now();
    let auth_info = dispatcher
        .hello_auth(&cfg.alpn, &device_identity, &cfg.display_name)
        .await
        .context("hello/auth")?;
    let auth_elapsed = auth_started.elapsed();
    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Authenticating,
        format!(
            "Authentication completed in {} ms",
            auth_elapsed.as_millis()
        ),
    );

    debug!(
        session_id = %auth_info.session_id,
        user_id = %auth_info.user_id,
        display_name = %cfg.display_name,
        "auth success"
    );
    if !auth_info.user_id.is_empty() {
        let _ = tx_event.send(UiEvent::SetUserId(auth_info.user_id.clone()));
    }

    #[cfg(debug_assertions)]
    if !auth_info.user_id.trim().is_empty() {
        let seen = DEBUG_SEEN_AUTH_USER_IDS.get_or_init(|| StdMutex::new(HashSet::new()));
        if let Ok(mut seen_ids) = seen.lock() {
            if !seen_ids.insert(auth_info.user_id.clone()) {
                warn!(
                    user_id = %auth_info.user_id,
                    session_id = %auth_info.session_id,
                    "debug warning: auth user_id already seen in this process; sessions may represent the same identity"
                );
                let _ = tx_event.send(UiEvent::AppendLog(
                    format!(
                        "[auth] warning: authenticated user_id {} already exists in a local session; nickname does not change auth identity",
                        auth_info.user_id
                    ),
                ));
            }
        }
    }

    let local_user_id = if auth_info.user_id.trim().is_empty() {
        cfg.display_name.clone()
    } else {
        auth_info.user_id.clone()
    };

    let stream_state = SharedStreamState::new();

    let _ = tx_event.send(UiEvent::SetAuthed(true));
    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Syncing,
        "Syncing initial state",
    );

    let initial_active_channel: Option<String> = cfg.channel_id.clone();

    let snapshot = dispatcher
        .get_initial_state_snapshot()
        .await
        .context("get_initial_state_snapshot")?;
    let initially_server_deafened = snapshot.channel_members.iter().any(|scope| {
        scope.members.iter().any(|member| {
            member.user_id.as_ref().map(|u| u.value.as_str()) == Some(local_user_id.as_str())
                && member.deafened
        })
    });
    server_deafened.store(initially_server_deafened, Ordering::Relaxed);
    apply_authoritative_snapshot(&snapshot, tx_event, initial_active_channel.as_deref());

    // Server push consumer
    let mut push_rx = dispatcher.take_push_receiver().await;
    {
        let tx_event = tx_event.clone();
        let mut last_event_seq = snapshot.snapshot_version;
        let local_user_id = local_user_id.clone();
        let conn = conn.clone();
        let active_voice_channel_route = active_voice_channel_route.clone();
        let server_deafened = server_deafened.clone();
        let stream_state = stream_state.clone();
        tokio::spawn(async move {
            while let Some(ev) = push_rx.recv().await {
                match ev {
                    PushEvent::Chat {
                        event: c,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
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

                                    let message_id = mp
                                        .message_id
                                        .as_ref()
                                        .map(|m| m.value.clone())
                                        .unwrap_or_default();
                                    debug!(
                                        message_id = %message_id,
                                        author_user_id = %author_id,
                                        channel_id = %channel_id,
                                        "received message_posted push event"
                                    );

                                    let mut attachments: Vec<ui::model::AttachmentData> = mp
                                        .attachments
                                        .into_iter()
                                        .map(|a| ui::model::AttachmentData {
                                            asset: AttachmentAsset::UploadedAssetId(
                                                a.asset_id.map(|x| x.value).unwrap_or_default(),
                                            ),
                                            filename: a.filename,
                                            mime_type: a.mime_type,
                                            size_bytes: a.size_bytes,
                                            download_url: String::new(),
                                            thumbnail_url: None,
                                        })
                                        .collect();

                                    for attachment in &mut attachments {
                                        if !matches!(
                                            &attachment.asset,
                                            AttachmentAsset::UploadedAssetId(ref asset_id) if !asset_id.is_empty()
                                        ) {
                                            continue;
                                        }
                                        if let Ok(path) =
                                            resolve_attachment_local_path(&conn, attachment).await
                                        {
                                            attachment.download_url =
                                                format!("file://{}", path.display());
                                        }
                                    }

                                    let _ = tx_event.send(UiEvent::MessageReceived(
                                        ui::model::ChatMessage {
                                            message_id,
                                            channel_id,
                                            author_name: author_id.clone(),
                                            author_id: author_id.clone(),
                                            text: mp.text.clone(),
                                            timestamp,
                                            attachments,
                                            reply_to: mp.reply_to_message_id.map(|r| r.value),
                                            reactions: Vec::new(),
                                            pinned: mp.pinned,
                                            edited: mp.edited_at.is_some(),
                                        },
                                    ));
                                    if author_id != local_user_id {
                                        let _ = tx_event.send(UiEvent::PlayChatMessageSfx);
                                    }
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
                                pb::chat_event::Kind::ReactionAdded(ra) => {
                                    let channel_id = ra
                                        .channel_id
                                        .as_ref()
                                        .map(|c| c.value.clone())
                                        .unwrap_or_default();
                                    let message_id = ra
                                        .message_id
                                        .as_ref()
                                        .map(|m| m.value.clone())
                                        .unwrap_or_default();
                                    let user_id = ra
                                        .user_id
                                        .as_ref()
                                        .map(|u| u.value.clone())
                                        .unwrap_or_default();
                                    let _ = tx_event.send(UiEvent::ReactionAdded {
                                        channel_id,
                                        message_id,
                                        emoji: ra.emoji,
                                        me: user_id == local_user_id,
                                        user_id,
                                    });
                                }
                                pb::chat_event::Kind::ReactionRemoved(rr) => {
                                    let channel_id = rr
                                        .channel_id
                                        .as_ref()
                                        .map(|c| c.value.clone())
                                        .unwrap_or_default();
                                    let message_id = rr
                                        .message_id
                                        .as_ref()
                                        .map(|m| m.value.clone())
                                        .unwrap_or_default();
                                    let user_id = rr
                                        .user_id
                                        .as_ref()
                                        .map(|u| u.value.clone())
                                        .unwrap_or_default();
                                    let _ = tx_event.send(UiEvent::ReactionRemoved {
                                        channel_id,
                                        message_id,
                                        emoji: rr.emoji,
                                        me: user_id == local_user_id,
                                        user_id,
                                    });
                                }
                                pb::chat_event::Kind::TypingStarted(ts) => {
                                    let channel_id = ts
                                        .channel_id
                                        .as_ref()
                                        .map(|c| c.value.clone())
                                        .unwrap_or_default();
                                    let user_id = ts
                                        .user_id
                                        .as_ref()
                                        .map(|u| u.value.clone())
                                        .unwrap_or_default();
                                    if user_id != local_user_id {
                                        let _ = tx_event.send(UiEvent::TypingIndicator {
                                            channel_id,
                                            user_id,
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    PushEvent::Presence {
                        event: p,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
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
                                        debug!(
                                            channel_id = %channel_id.value,
                                            user_id = %user_id,
                                            display_name = %member.display_name,
                                            "received member-joined push event"
                                        );
                                        if user_id == local_user_id {
                                            server_deafened
                                                .store(member.deafened, Ordering::Relaxed);
                                            let route = uuid::Uuid::parse_str(&channel_id.value)
                                                .map(vp_route_hash::channel_route_hash)
                                                .unwrap_or(0);
                                            active_voice_channel_route
                                                .store(route, Ordering::Relaxed);
                                            let _ =
                                                tx_event.send(UiEvent::SetActiveVoiceRoute(route));
                                        }
                                        let _ = tx_event.send(UiEvent::MemberJoined {
                                            channel_id: channel_id.value,
                                            member: ui::model::MemberEntry {
                                                user_id,
                                                display_name: member.display_name,
                                                away_message: String::new(),
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
                                    let left_user = ml
                                        .user_id
                                        .as_ref()
                                        .map(|u| u.value.clone())
                                        .unwrap_or_default();
                                    debug!(channel_id=%ml.channel_id.as_ref().map(|c| c.value.clone()).unwrap_or_default(), user_id=%left_user, "received member-left push event");
                                    if left_user == local_user_id {
                                        server_deafened.store(false, Ordering::Relaxed);
                                        active_voice_channel_route.store(0, Ordering::Relaxed);
                                        let _ = tx_event.send(UiEvent::SetActiveVoiceRoute(0));
                                        let _ = tx_event.send(UiEvent::AppendLog(
                                            "[moderation] you were removed from this channel"
                                                .into(),
                                        ));
                                    }
                                    let _ = tx_event.send(UiEvent::MemberLeft {
                                        channel_id: ml
                                            .channel_id
                                            .map(|c| c.value)
                                            .unwrap_or_default(),
                                        user_id: ml.user_id.map(|u| u.value).unwrap_or_default(),
                                    });
                                }
                                pb::presence_event::Kind::MemberVoiceStateChanged(vs) => {
                                    let user_id = vs
                                        .user_id
                                        .as_ref()
                                        .map(|u| u.value.clone())
                                        .unwrap_or_default();
                                    if user_id == local_user_id {
                                        server_deafened.store(vs.deafened, Ordering::Relaxed);
                                    }
                                    let _ = tx_event.send(UiEvent::MemberVoiceStateUpdated {
                                        channel_id: vs
                                            .channel_id
                                            .map(|c| c.value)
                                            .unwrap_or_default(),
                                        user_id,
                                        muted: vs.muted,
                                        deafened: vs.deafened,
                                        self_muted: vs.self_muted,
                                        self_deafened: vs.self_deafened,
                                        streaming: vs.streaming,
                                    });
                                }
                                pb::presence_event::Kind::UserOnlineStatusChanged(status) => {
                                    let user_id = status
                                        .user_id
                                        .as_ref()
                                        .map(|u| u.value.clone())
                                        .unwrap_or_default();
                                    let _ = tx_event.send(UiEvent::MemberAwayMessageUpdated {
                                        user_id,
                                        away_message: status.custom_status_text,
                                    });
                                }
                            }
                        }
                    }
                    PushEvent::Moderation {
                        event: m,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        if let Some(pb::moderation_event::Kind::UserKicked(ev)) = m.kind.clone() {
                            let _ = tx_event.send(UiEvent::MemberLeft {
                                channel_id: ev.channel_id.map(|c| c.value).unwrap_or_default(),
                                user_id: ev.target_user_id.map(|u| u.value).unwrap_or_default(),
                            });
                        }
                        let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] {:?}", m)));
                    }
                    PushEvent::Poke { event, event_seq } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                            "[poke] from={} message={}",
                            event.from_display_name, event.message
                        )));
                        let _ = tx_event.send(UiEvent::PokeReceived {
                            from_name: event.from_display_name,
                            message: event.message,
                        });
                    }
                    PushEvent::ChannelCreated {
                        event: cr,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        if let Some(channel) = cr.channel {
                            if let Some(ch_id) = channel.channel_id {
                                debug!(channel_id=%ch_id.value, name=%channel.name, event_seq, "received channel-created push event");
                                let _ = tx_event.send(UiEvent::ChannelCreated(
                                    ui::model::ChannelEntry {
                                        id: ch_id.value,
                                        name: channel.name,
                                        channel_type: pb_channel_type_to_ui(channel.channel_type),
                                        parent_id: channel.parent_channel_id.map(|pid| pid.value),
                                        position: channel.position,
                                        member_count: 0,
                                        user_limit: channel.user_limit,
                                        description: channel.description,
                                        bitrate_bps: channel.bitrate,
                                        opus_profile: channel.opus_profile,
                                    },
                                ));
                            }
                        }
                    }
                    PushEvent::ChannelRenamed {
                        event: cr,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        if let Some(channel) = cr.channel {
                            if let Some(ch_id) = channel.channel_id {
                                debug!(channel_id=%ch_id.value, name=%channel.name, event_seq, "received channel-renamed push event");
                                let _ = tx_event.send(UiEvent::ChannelRenamed(
                                    ui::model::ChannelEntry {
                                        id: ch_id.value,
                                        name: channel.name,
                                        channel_type: pb_channel_type_to_ui(channel.channel_type),
                                        parent_id: channel.parent_channel_id.map(|pid| pid.value),
                                        position: channel.position,
                                        member_count: 0,
                                        user_limit: channel.user_limit,
                                        description: channel.description,
                                        bitrate_bps: channel.bitrate,
                                        opus_profile: channel.opus_profile,
                                    },
                                ));
                            }
                        }
                    }
                    PushEvent::ChannelDeleted { event, event_seq } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        if let Some(channel_id) = event.channel_id {
                            debug!(channel_id=%channel_id.value, event_seq, "received channel-deleted push event");
                            let _ = tx_event.send(UiEvent::ChannelDeleted {
                                channel_id: channel_id.value,
                            });
                        }
                    }
                    PushEvent::VoiceTelemetry { event, event_seq } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        if let Some(user_id) = event.user_id {
                            let _ = tx_event.send(UiEvent::MemberTelemetryUpdate {
                                user_id: user_id.value,
                                telemetry: ui::model::TelemetryData {
                                    rtt_ms: event.rtt_ms,
                                    loss_rate: event.loss_rate,
                                    jitter_ms: event.jitter_ms,
                                    rx_bitrate_bps: event.goodput_bps,
                                    playout_delay_ms: event.playout_delay_ms,
                                    ..Default::default()
                                },
                            });
                        }
                    }
                    PushEvent::ServerHint { hint: h, event_seq } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
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
                    PushEvent::Snapshot {
                        snapshot,
                        event_seq,
                    } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                            "[sync] received snapshot push server_id={} channels={} member_scopes={} self_user_id={}",
                            snapshot.server_id.as_ref().map(|sid| sid.value.clone()).unwrap_or_default(),
                            snapshot.channels.len(),
                            snapshot.channel_members.len(),
                            snapshot.self_user_id.as_ref().map(|u| u.value.clone()).unwrap_or_default(),
                        )));
                    }
                    PushEvent::Permissions { event, event_seq } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        let summary = match event.evt {
                            Some(pb::push_event::Evt::RoleUpserted(e)) => {
                                format!(
                                    "role_upserted role_id={} position={}",
                                    e.role_id, e.position
                                )
                            }
                            Some(pb::push_event::Evt::RoleDeleted(e)) => {
                                format!("role_deleted role_id={}", e.role_id)
                            }
                            Some(pb::push_event::Evt::RoleOrder(e)) => {
                                format!("role_order_changed count={}", e.role_ids.len())
                            }
                            Some(pb::push_event::Evt::RoleCaps(e)) => {
                                format!("role_caps_changed role_id={}", e.role_id)
                            }
                            Some(pb::push_event::Evt::UserRoles(e)) => {
                                format!(
                                    "user_roles_changed user_id={}",
                                    e.user_id.map(|u| u.value).unwrap_or_default()
                                )
                            }
                            Some(pb::push_event::Evt::ChanOvr(e)) => {
                                format!(
                                    "channel_overrides_changed channel_id={}",
                                    e.channel_id.map(|c| c.value).unwrap_or_default()
                                )
                            }
                            Some(pb::push_event::Evt::AuditAppended(e)) => {
                                format!(
                                    "audit_appended action={} target={}:{}",
                                    e.action, e.target_type, e.target_id
                                )
                            }
                            None => "permissions_push empty".to_string(),
                        };
                        let _ =
                            tx_event.send(UiEvent::AppendLog(format!("[perm-push] {}", summary)));
                    }
                    PushEvent::UnsubscribeStream { event, event_seq } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        {
                            let mut streams = stream_state.active_streams.write().await;
                            streams.remove(&event.stream_tag);
                        }
                        {
                            let mut stream_codecs = stream_state.stream_codecs.write().await;
                            stream_codecs.remove(&event.stream_tag);
                        }
                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                            "[video] unsubscribed stream_tag={}",
                            event.stream_tag
                        )));
                    }
                    PushEvent::SubscribeStream { event, event_seq } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        {
                            let mut streams = stream_state.active_streams.write().await;
                            streams.insert(
                                event.stream_tag,
                                Arc::new(Mutex::new(VideoReceiver::new(
                                    4,
                                    vp_voice::MAX_FRAGS_PER_FRAME,
                                ))),
                            );
                        }
                        if let Ok(codec) = pb::VideoCodec::try_from(event.codec) {
                            let mut stream_codecs = stream_state.stream_codecs.write().await;
                            stream_codecs.insert(event.stream_tag, codec);
                        }
                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                            "[video] subscribed stream_tag={} codec={}",
                            event.stream_tag, event.codec
                        )));
                    }
                    PushEvent::UserProfile { event, event_seq } => {
                        maybe_note_event_gap(&tx_event, event_seq);
                        if !should_apply_event_seq(&tx_event, &mut last_event_seq, event_seq) {
                            continue;
                        }
                        match event.kind {
                            Some(pb::user_profile_event::Kind::UserProfileUpdated(updated)) => {
                                let uid = updated.user_id.map(|u| u.value).unwrap_or_default();
                                let _ = tx_event.send(UiEvent::UserProfileCacheInvalidated {
                                    user_id: uid.clone(),
                                });
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[profile] profile updated for {uid}"
                                )));
                            }
                            Some(pb::user_profile_event::Kind::UserStatusChanged(changed)) => {
                                let uid = changed.user_id.map(|u| u.value).unwrap_or_default();
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[profile] status changed for {uid}: {:?}",
                                    changed.status
                                )));
                            }
                            None => {}
                        }
                    }
                    PushEvent::Unknown(_) => {}
                }
            }
        });
    }

    let selected_after_sync =
        choose_initial_selected_channel(&snapshot, initial_active_channel.as_deref());

    if let Some(channel_id) = selected_after_sync.as_ref() {
        let route = uuid::Uuid::parse_str(&channel_id)
            .map(vp_route_hash::channel_route_hash)
            .unwrap_or(0);
        active_voice_channel_route.store(route, Ordering::Relaxed);
        if let Some(info) = snapshot
            .channels
            .iter()
            .filter_map(|ch| ch.info.as_ref())
            .find(|info| {
                info.channel_id.as_ref().map(|id| id.value.as_str()) == Some(channel_id.as_str())
            })
        {
            if let Ok(mut mode) = active_channel_audio_mode.write() {
                *mode = ChannelAudioMode {
                    opus_profile: info.opus_profile,
                    bitrate_bps: info.bitrate,
                };
            }
        }
    } else {
        active_voice_channel_route.store(0, Ordering::Relaxed);
    }

    set_connection_stage(
        tx_event,
        ui::model::ConnectionStage::Connected,
        "Connected and ready",
    );

    let mtu = conn
        .max_datagram_size()
        .unwrap_or(vp_voice::APP_MEDIA_MTU)
        .min(vp_voice::APP_MEDIA_MTU);
    let egress = EgressScheduler::new(
        conn.clone(),
        net::egress::EgressConfig {
            mtu_bytes: mtu,
            frame_deadline_ms: 160,
            ..Default::default()
        },
        ui_log_tx.clone(),
    );
    let egress_stats = egress.stats();
    let _egress_task = egress.clone().start();

    let (voice_die_tx, mut voice_die_rx) = watch::channel::<bool>(false);
    let _session_voice_flag = SessionVoiceFlag::new(session_voice_active.clone());
    let _ = tx_event.send(UiEvent::VoiceSessionHealth(true));

    let voice_max_inbound = mtu.saturating_sub(vp_voice::FORWARDER_ADDED_HEADER_BYTES);
    let max_opus_payload_runtime =
        voice_max_inbound.saturating_sub(vp_voice::CLIENT_VOICE_HEADER_BYTES);
    let _ = tx_event.send(UiEvent::AppendLog(format!(
        "[net] mtu={} voice_max_inbound={} max_opus_payload={}",
        mtu, voice_max_inbound, max_opus_payload_runtime
    )));

    let _voice_send = tokio::spawn(voice_send_loop(
        egress.clone(),
        mtu,
        encoder.clone(),
        capture.clone(),
        playout.clone(),
        capture_dsp.clone(),
        dsp_enabled.clone(),
        tx_event.clone(),
        active_voice_channel_route.clone(),
        active_channel_audio_mode.clone(),
        ptt_active.clone(),
        capture_mode.clone(),
        self_muted.clone(),
        self_deafened.clone(),
        server_deafened.clone(),
        input_gain.clone(),
        loopback_active.clone(),
        audio_runtime.clone(),
        voice_counters.clone(),
        network_telemetry.clone(),
        send_queue_drop_count.clone(),
        local_user_id.clone(),
        voice_die_tx.clone(),
    ));

    // End-to-end screenshare flow:
    // UI intent -> control StartScreenShareRequest -> stream_tag -> sender task ->
    // datagrams -> demux loop -> VideoReceiver -> UI StreamDebugUpdate panel.
    let voice_ingress_q = Arc::new(OverwriteQueue::<StampedBytes>::new(VOICE_INGRESS_CAP));
    let voice_stale_drops_total = Arc::new(AtomicU64::new(0));
    let voice_drain_drops_total = Arc::new(AtomicU64::new(0));
    let (video_rx_tx, video_rx_rx) = mpsc::channel::<Bytes>(512);

    let _datagram_demux = tokio::spawn(datagram_demux_loop(
        conn.clone(),
        voice_ingress_q.clone(),
        video_rx_tx,
        stream_state.counters.clone(),
        voice_stale_drops_total.clone(),
        voice_drain_drops_total.clone(),
        voice_die_tx.clone(),
    ));

    let _voice_recv = tokio::spawn(voice_recv_loop(
        voice_ingress_q,
        playout.clone(),
        capture_dsp.clone(),
        local_user_id.clone(),
        self_deafened.clone(),
        server_deafened.clone(),
        output_gain.clone(),
        per_user_audio.clone(),
        audio_runtime.clone(),
        tx_event.clone(),
        voice_counters.clone(),
        voice_stale_drops_total.clone(),
        voice_drain_drops_total.clone(),
        voice_die_tx.clone(),
    ));

    let _video_recv = tokio::spawn(video_recv_loop(video_rx_rx, stream_state.clone()));

    let disp_keepalive = dispatcher.clone();
    let disp_health = dispatcher.clone();
    let disp_voice_rr = dispatcher.clone();
    let ctl_keepalive = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            if let Err(e) = disp_keepalive.ping().await {
                return Err::<(), anyhow::Error>(e.context("control keepalive ping failed"));
            }
        }
    });

    // Track the active channel (for SendChat and other channel-scoped operations)
    let active_channel_for_reports =
        Arc::new(tokio::sync::RwLock::new(selected_after_sync.clone()));
    let mut active_channel: Option<String> = selected_after_sync;

    let mut voice_rr_die_rx = voice_die_rx.clone();
    let rr_active_channel = active_channel_for_reports.clone();
    let rr_voice_counters = voice_counters.clone();
    let rr_network_telemetry = network_telemetry.clone();
    let rr_tx_event = tx_event.clone();
    let _voice_receiver_report = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut prev_rx_bytes = rr_voice_counters.rx_bytes.load(Ordering::Relaxed);
        loop {
            tokio::select! {
                _ = voice_rr_die_rx.changed() => {
                    if *voice_rr_die_rx.borrow() {
                        break;
                    }
                }
                _ = tick.tick() => {
                    let channel_id = rr_active_channel.read().await.clone();
                    let Some(channel_id) = channel_id else {
                        continue;
                    };

                    let rx_bytes = rr_voice_counters.rx_bytes.load(Ordering::Relaxed);
                    let goodput_bps = rx_bytes
                        .saturating_sub(prev_rx_bytes)
                        .saturating_mul(8)
                        .min(u32::MAX as u64) as u32;
                    prev_rx_bytes = rx_bytes;

                    let loss_rate = (rr_network_telemetry.loss_ppm.load(Ordering::Relaxed) as f32 / 1_000_000.0)
                        .clamp(0.0, 1.0);
                    let report = pb::VoiceReceiverReport {
                        channel_id: Some(pb::ChannelId { value: channel_id }),
                        loss_rate,
                        rtt_ms: rr_network_telemetry.rtt_ms.load(Ordering::Relaxed),
                        jitter_ms: rr_network_telemetry.jitter_ms.load(Ordering::Relaxed),
                        goodput_bps,
                        playout_delay_ms: rr_voice_counters.playout_delay_ms.load(Ordering::Relaxed),
                    };

                    if let Err(e) = disp_voice_rr
                        .send_no_response(pb::client_to_server::Payload::VoiceReceiverReport(report))
                        .await
                    {
                        let _ = rr_tx_event.send(UiEvent::AppendLog(format!(
                            "[telemetry] voice receiver report send failed: {e:#}"
                        )));
                        break;
                    }
                }
            }
        }
    });
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ShareState {
        Idle,
        Starting,
        Active,
    }

    let mut active_share_stop: Option<watch::Sender<bool>> = None;
    let mut active_local_stream_id: Option<pb::StreamId> = None;
    let mut share_state = ShareState::Idle;

    tokio::pin!(ctl_keepalive);
    let mut audio_health_tick = tokio::time::interval(Duration::from_secs(1));
    let mut stream_ui_tick = tokio::time::interval(Duration::from_secs(1));
    let mut consecutive_audio_stalls = 0_u32;
    let mut last_stall_recovery_notice = Instant::now() - Duration::from_secs(30);
    loop {
        tokio::select! {
            _ = stream_ui_tick.tick() => {
                if let Ok(frame) = stream_state.latest_frame.read() {
                    if let Some(frame) = frame.clone() {
                        let _ = tx_event.send(UiEvent::StreamFrame(frame));
                    }
                }
                let active_stream_tags = {
                    let streams = stream_state.active_streams.read().await;
                    streams.keys().copied().collect::<Vec<_>>()
                };
                let video_tx_bytes_per_sec = egress_stats.tx_bytes.swap(0, Ordering::Relaxed);
                let completed_frames_per_sec = stream_state.counters.completed_frames.swap(0, Ordering::Relaxed);
                let dropped_frames = stream_state.counters.dropped_no_subscription.load(Ordering::Relaxed)
                    + stream_state.counters.dropped_channel_full.load(Ordering::Relaxed)
                    + stream_state.counters.sender_frame_errors.load(Ordering::Relaxed);
                let total_frames = completed_frames_per_sec + dropped_frames;
                let negotiated_video_codecs = {
                    let stream_codecs = stream_state.stream_codecs.read().await;
                    let mut names = active_stream_tags
                        .iter()
                        .filter_map(|tag| stream_codecs.get(tag).copied())
                        .map(video_codec_name)
                        .collect::<Vec<_>>();
                    names.sort_unstable();
                    names.dedup();
                    if names.is_empty() {
                        "unknown".to_string()
                    } else {
                        names.join("/")
                    }
                };
                let snapshot = ui::model::StreamDebugView {
                    active_stream_tags,
                    video_datagrams_per_sec: stream_state.counters.video_datagrams.swap(0, Ordering::Relaxed),
                    video_tx_datagrams_per_sec: egress_stats.tx_video.swap(0, Ordering::Relaxed),
                    video_tx_bytes_per_sec,
                    video_tx_blocked_per_sec: egress_stats.blocked_events.swap(0, Ordering::Relaxed),
                    video_tx_drop_queue_full: egress_stats.drop_queue_full_video.load(Ordering::Relaxed),
                    video_tx_drop_deadline: egress_stats.drop_deadline_video.load(Ordering::Relaxed),
                    voice_tx_drop_queue_full: egress_stats.drop_queue_full_voice.load(Ordering::Relaxed),
                    voice_tx_drop_too_large: egress_stats.drop_too_large_voice.load(Ordering::Relaxed),
                    video_tx_drop_too_large: egress_stats.drop_too_large_video.load(Ordering::Relaxed),
                    completed_frames_per_sec,
                    dropped_no_subscription: stream_state.counters.dropped_no_subscription.load(Ordering::Relaxed),
                    dropped_channel_full: stream_state.counters.dropped_channel_full.load(Ordering::Relaxed),
                    sender_frame_errors: stream_state.counters.sender_frame_errors.load(Ordering::Relaxed),
                    last_frame_size_bytes: stream_state.counters.last_frame_size_bytes.load(Ordering::Relaxed) as usize,
                    last_frame_seq: stream_state.counters.last_frame_seq.load(Ordering::Relaxed),
                    last_frame_ts_ms: stream_state.counters.last_frame_ts_ms.load(Ordering::Relaxed),
                    codec_video: negotiated_video_codecs,
                    codec_audio: "opus (251)".to_string(),
                    connection_speed_kbps: (video_tx_bytes_per_sec.saturating_mul(8)) / 1000,
                    network_activity_bytes: video_tx_bytes_per_sec,
                    buffer_health_seconds: if completed_frames_per_sec > 0 { 1.0 } else { 0.0 },
                    current_resolution: "0x0@0".to_string(),
                    optimal_resolution: "0x0@0".to_string(),
                    viewport: "0x0*1.00".to_string(),
                    dropped_frames,
                    total_frames,
                };
                let _ = tx_event.send(UiEvent::StreamDebugUpdate(snapshot));
            }
            _ = audio_health_tick.tick() => {
                let ping_rtt_ms = disp_health
                    .ping()
                    .await
                    .map(|rtt| rtt.as_millis().min(u32::MAX as u128) as u32)
                    .unwrap_or_else(|e| {
                        warn!(error = %e, "control ping failed; falling back to transport RTT");
                        conn.rtt().as_millis().min(u32::MAX as u128) as u32
                    });
                network_telemetry
                    .rtt_ms
                    .store(ping_rtt_ms, Ordering::Relaxed);

                let capture_healthy = {
                    let cap = capture.read().await;
                    cap.is_healthy()
                };
                let playout_healthy = {
                    let out = playout.read().await;
                    out.is_healthy()
                };

                if capture_healthy && playout_healthy {
                    consecutive_audio_stalls = 0;
                    continue;
                }

                consecutive_audio_stalls = consecutive_audio_stalls.saturating_add(1);
                let _ = tx_event.send(UiEvent::AppendLog(format!(
                    "[audio] stall detected (capture_healthy={}, playout_healthy={}, consecutive={}); monitoring",
                    capture_healthy, playout_healthy, consecutive_audio_stalls
                )));

                if consecutive_audio_stalls < 3 {
                    continue;
                }

                let _ = tx_event.send(UiEvent::AppendLog(format!(
                    "[audio] repeated stalls detected ({} checks); attempting seamless stream re-init",
                    consecutive_audio_stalls
                )));
                if let Err(e) = restart_audio_streams(
                    &capture,
                    &playout,
                    &selected_audio,
                    tx_event,
                    sample_rate,
                    channels,
                    frame_ms,
                )
                .await
                {
                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                        "[audio] stream re-init failed: {e:#}"
                    )));
                } else if last_stall_recovery_notice.elapsed() >= Duration::from_secs(10) {
                    let _ = tx_event.send(UiEvent::Notify {
                        text: "Audio stream stalled repeatedly. Reinitialized capture/playout.".to_string(),
                        kind: ui::model::NotificationKind::Error,
                    });
                    last_stall_recovery_notice = Instant::now();
                }
                consecutive_audio_stalls = 0;
            }
            // Check for UI intents (non-blocking poll from crossbeam)
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                if ptt_state.pressed {
                    ptt_state.release_deadline = None;
                } else if let Some(deadline) = ptt_state.release_deadline {
                    if Instant::now() >= deadline {
                        ptt_active.store(false, Ordering::Relaxed);
                        ptt_state.release_deadline = None;
                    }
                }

                while let Ok(intent) = rx_intent.try_recv() {
                    match intent {
                        UiIntent::Quit => return Ok(()),
                        UiIntent::CancelConnect => {
                            set_connection_stage(tx_event, ui::model::ConnectionStage::Idle, "Disconnect requested by user");
                            return Err(anyhow!("disconnect requested"));
                        }
                        UiIntent::ConnectToServer { host, port, nickname } => {
                            cfg.display_name = nickname.clone();
                            let _ = tx_event.send(UiEvent::SetNick(nickname));

                            let new_server = format!("{host}:{port}");
                            cfg.server = new_server.clone();
                            cfg.server_name = host.clone();
                            let _ = tx_event.send(UiEvent::SetServerAddress { host, port });
                            set_connection_stage(
                                tx_event,
                                ui::model::ConnectionStage::Resolving,
                                format!("Reconnect requested: {new_server}"),
                            );
                            return Err(anyhow!("reconnect requested"));
                        }
                        UiIntent::TogglePtt => {
                            if active_voice_channel_route.load(Ordering::Relaxed) != 0 {
                                let new = !ptt_active.load(Ordering::Relaxed);
                                ptt_active.store(new, Ordering::Relaxed);
                                ptt_state.pressed = new;
                                if new {
                                    ptt_state.release_deadline = None;
                                }
                            }
                        }
                        UiIntent::PttDown => {
                            if active_voice_channel_route.load(Ordering::Relaxed) != 0 {
                                ptt_state.pressed = true;
                                ptt_state.release_deadline = None;
                                ptt_active.store(true, Ordering::Relaxed);
                            }
                        }
                        UiIntent::PttUp => {
                            if active_voice_channel_route.load(Ordering::Relaxed) != 0 {
                                ptt_state.pressed = false;
                                ptt_state.release_deadline = Some(
                                    Instant::now()
                                        + Duration::from_millis(saved_settings.ptt_delay_ms as u64),
                                );
                            }
                        }
                        UiIntent::ToggleSelfMute => {
                            if active_voice_channel_route.load(Ordering::Relaxed) != 0 {
                                let new = !self_muted.load(Ordering::Relaxed);
                                self_muted.store(new, Ordering::Relaxed);
                                let _ = tx_event.send(UiEvent::SetSelfMuted(new));
                            }
                        }
                        UiIntent::ToggleSelfDeafen => {
                            if active_voice_channel_route.load(Ordering::Relaxed) != 0 {
                                let new = !self_deafened.load(Ordering::Relaxed);
                                self_deafened.store(new, Ordering::Relaxed);
                                let _ = tx_event.send(UiEvent::SetSelfDeafened(new));
                            }
                        }
                        UiIntent::SendChat { text, attachments } => {
                            if let Some(ref ch) = active_channel {
                                // Optimistic local echo
                                let now_ms = unix_ms() as i64;
                                let local_message_id = format!("local-{now_ms}");
                                debug!(
                                    message_id = %local_message_id,
                                    author_user_id = %local_user_id,
                                    channel_id = %ch,
                                    "sending chat message (optimistic local echo)"
                                );
                                let mut uploaded_attachments = Vec::new();
                                let mut upload_failed = false;
                                for attachment in &attachments {
                                    let result: anyhow::Result<()> = async {
                                        match &attachment.asset {
                                            AttachmentAsset::PendingLocalPath(_) => {
                                                let uploaded = upload_attachment_quic(&conn, ch, attachment).await?;
                                                uploaded_attachments.push(uploaded);
                                            }
                                            AttachmentAsset::UploadedAssetId(_) => {
                                                uploaded_attachments.push(attachment.clone());
                                            }
                                        }
                                        Ok(())
                                    }.await;
                                    if let Err(e) = result {
                                        let _ = tx_event.send(UiEvent::AttachmentUploadError {
                                            path: attachment_source_label(attachment),
                                            error: e.to_string(),
                                        });
                                        let _ = tx_event.send(UiEvent::AppendLog(format!("[upload] failed: {e:#}")));
                                        uploaded_attachments.clear();
                                        upload_failed = true;
                                        break;
                                    }
                                }

                                if upload_failed {
                                    continue;
                                }

                                let _ = tx_event.send(UiEvent::MessageReceived(
                                    ui::model::ChatMessage {
                                        message_id: local_message_id,
                                        channel_id: ch.clone(),
                                        author_id: local_user_id.clone(),
                                        author_name: cfg.display_name.clone(),
                                        text: text.clone(),
                                        timestamp: now_ms,
                                        attachments: uploaded_attachments.clone(),
                                        reply_to: None,
                                        reactions: Vec::new(),
                                        pinned: false,
                                        edited: false,
                                    },
                                ));
                                let _ = tx_event.send(UiEvent::PlayChatMessageSfx);
                                let pb_attachments = uploaded_attachments
                                    .into_iter()
                                    .filter_map(|a| {
                                        let asset_id = match a.asset {
                                            AttachmentAsset::UploadedAssetId(asset_id)
                                                if !asset_id.is_empty() =>
                                            {
                                                asset_id
                                            }
                                            _ => return None,
                                        };
                                        Some(pb::AttachmentRef {
                                            asset_id: Some(pb::AssetId { value: asset_id }),
                                            filename: a.filename,
                                            mime_type: a.mime_type,
                                            size_bytes: a.size_bytes,
                                            sha256: String::new(),
                                            ..Default::default()
                                        })
                                    })
                                    .collect();
                                if let Err(e) = dispatcher.send_chat(ch, &text, pb_attachments).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[ctl] send_chat failed: {e:#}",
                                    )));
                                } else {
                                    let _ = tx_event.send(UiEvent::ClearPendingAttachments);
                                }
                            } else {
                                let _ = tx_event.send(UiEvent::AppendLog(
                                    "[ctl] no channel selected, cannot send message".into(),
                                ));
                            }
                        }
                        UiIntent::AddReaction { message_id, emoji } => {
                            if let Some(ref ch) = active_channel {
                                if let Err(e) = dispatcher.add_reaction(ch, &message_id, &emoji).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[ctl] add_reaction failed: {e:#}",
                                    )));
                                }
                            }
                        }
                        UiIntent::RemoveReaction { message_id, emoji } => {
                            if let Some(ref ch) = active_channel {
                                if let Err(e) =
                                    dispatcher.remove_reaction(ch, &message_id, &emoji).await
                                {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[ctl] remove_reaction failed: {e:#}",
                                    )));
                                }
                            }
                        }
                        UiIntent::SendTyping => {
                            if let Some(ref ch) = active_channel {
                                let _ = dispatcher.send_typing(ch).await;
                            }
                        }
                        UiIntent::OpenAttachment { attachment } => {
                            match resolve_attachment_local_path(&conn, &attachment).await {
                                Ok(path) => {
                                    if let Err(e) = open::that(&path) {
                                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                                            "[chat] open attachment failed: {e:#}"
                                        )));
                                    }
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[chat] download attachment failed: {e:#}"
                                    )));
                                }
                            }
                        }
                        UiIntent::JoinChannel { channel_id } => {
                            match dispatcher.join_channel(&channel_id).await {
                                Ok(state) => {
                                    if let Some(info) = state.info.as_ref() {
                                        let mut enc = encoder.lock().await;
                                        match audio::opus::OpusEncoder::new(
                                            sample_rate,
                                            channels as u8,
                                            encoder_profile_for_mode(saved_settings.voice_processing_mode),
                                        ) {
                                            Ok(mut new_encoder) => {
                                                let _ = new_encoder.set_bitrate(info.bitrate as i32);
                                                let _ = apply_fec_encoder_settings(&mut new_encoder, &audio_runtime);
                                                *enc = new_encoder;
                                            }
                                            Err(e) => {
                                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                                    "[audio] failed to reconfigure encoder: {e:#}"
                                                )));
                                            }
                                        }
                                    }
                                    for member in &state.members {
                                        debug!(
                                            channel_id = %channel_id,
                                            user_id = %member.user_id.as_ref().map(|u| u.value.clone()).unwrap_or_default(),
                                            display_name = %member.display_name,
                                            "join/member upsert snapshot"
                                        );
                                    }
                                    active_channel = Some(channel_id.clone());
                                    *active_channel_for_reports.write().await = active_channel.clone();
                                    if let Ok(mut mode) = active_channel_audio_mode.write() {
                                        *mode = ChannelAudioMode {
                                            opus_profile: state
                                                .info
                                                .as_ref()
                                                .map(|c| c.opus_profile)
                                                .unwrap_or(pb::OpusProfile::OpusVoice as i32),
                                            bitrate_bps: state.info.as_ref().map(|c| c.bitrate).unwrap_or(64_000),
                                        };
                                    }
                                    if let Some(local_member) =
                                        state.members.iter().find(|m| {
                                            m.user_id
                                                .as_ref()
                                                .map(|u| u.value.as_str())
                                                == Some(local_user_id.as_str())
                                        })
                                    {
                                        server_deafened.store(local_member.deafened, Ordering::Relaxed);
                                    }
                                    let route = uuid::Uuid::parse_str(&channel_id)
                                        .map(vp_route_hash::channel_route_hash)
                                        .unwrap_or(0);
                                    active_voice_channel_route.store(route, Ordering::Relaxed);
                                    let _ = tx_event.send(UiEvent::SetActiveVoiceRoute(route));
                                    let _ = tx_event.send(UiEvent::SetChannelName(channel_id.clone()));
                                    let _ = tx_event.send(UiEvent::UpdateChannelMembers {
                                        channel_id: channel_id.clone(),
                                        members: state
                                            .members
                                            .into_iter()
                                            .map(|m| ui::model::MemberEntry {
                                                user_id: m.user_id.map(|u| u.value).unwrap_or_default(),
                                                display_name: m.display_name,
                                                away_message: String::new(),
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
                            *active_channel_for_reports.write().await = None;
                            if let Ok(mut mode) = active_channel_audio_mode.write() {
                                *mode = ChannelAudioMode::default();
                            }
                            server_deafened.store(false, Ordering::Relaxed);
                            active_voice_channel_route.store(0, Ordering::Relaxed);
                            let _ = tx_event.send(UiEvent::SetActiveVoiceRoute(0));
                        }
                        UiIntent::CreateChannel { name, description, channel_type, codec, quality, user_limit, parent_channel_id } => {
                            match dispatcher.create_channel(&name, &description, channel_type, codec, quality * 1000, user_limit, parent_channel_id.as_deref()).await {
                                Ok(ch_id) => {
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
                        UiIntent::RenameChannel { channel_id, new_name, codec, quality } => {
                            match dispatcher
                                .rename_channel(&channel_id, &new_name, codec, quality * 1000)
                                .await
                            {
                                Ok(()) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] renamed channel {channel_id} -> '{new_name}'"),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] rename_channel failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::DeleteChannel { channel_id } => {
                            match dispatcher.delete_channel(&channel_id).await {
                                Ok(()) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] deleted channel {channel_id}"),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[ctl] delete_channel failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::Help => {
                            let _ = tx_event.send(UiEvent::AppendLog(
                                "[help] Space=PTT | Enter=Send | Settings for audio config".into(),
                            ));
                        }
                        UiIntent::SetVoiceProcessingMode(mode) => {
                                saved_settings.voice_processing_mode = mode;
                                mode.apply_to_settings(saved_settings);
                                dsp_enabled.store(
                                    saved_settings.dsp_enabled && !cfg.no_noise_suppression,
                                    Ordering::Relaxed,
                                );
                                if let Some(ref dsp) = capture_dsp {
                                    let mut d = dsp.lock().await;
                                    d.set_noise_suppression(saved_settings.noise_suppression);
                                    d.set_agc(saved_settings.agc_enabled);
                                    d.set_agc_preset(saved_settings.agc_preset);
                                    d.set_agc_target(saved_settings.agc_target_db);
                                }
                                audio_runtime.fec_mode.store(saved_settings.fec_mode as u32, Ordering::Relaxed);
                                audio_runtime.fec_strength.store(saved_settings.fec_strength as u32, Ordering::Relaxed);
                                let bitrate = active_channel_audio_mode
                                    .read()
                                    .map(|mode| mode.bitrate_bps)
                                    .unwrap_or(64_000);
                                let mut enc = encoder.lock().await;
                                match audio::opus::OpusEncoder::new(
                                    sample_rate,
                                    channels as u8,
                                    encoder_profile_for_mode(saved_settings.voice_processing_mode),
                                ) {
                                    Ok(mut new_encoder) => {
                                        let _ = new_encoder.set_bitrate(bitrate as i32);
                                        let _ = apply_fec_encoder_settings(&mut new_encoder, &audio_runtime);
                                        *enc = new_encoder;
                                    }
                                    Err(e) => {
                                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                                            "[audio] failed to reconfigure encoder profile: {e:#}"
                                        )));
                                    }
                                }
                                persist_settings(&tx_event, &saved_settings);
                            }
                            UiIntent::SetDspEnabled(enabled) => {
                            saved_settings.dsp_enabled = enabled;
                            dsp_enabled.store(enabled && !cfg.no_noise_suppression, Ordering::Relaxed);
                            info!("[audio] set dsp_enabled={enabled}");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetDspMethod(method) => {
                            saved_settings.dsp_method = method;
                            apply_resampler_mode(method);
                            info!("[audio] set dsp_method={}", method.label());
                            if let Err(e) = restart_audio_streams(
                                &capture,
                                &playout,
                                &selected_audio,
                                tx_event,
                                sample_rate,
                                channels,
                                frame_ms,
                            )
                            .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to switch DSP method: {e:#}"
                                )));
                            }
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetNoiseSuppression(enabled) => {
                            saved_settings.noise_suppression = enabled;
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_noise_suppression(enabled);
                            }
                            info!("[audio] set noise_suppression={enabled}");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetAgcEnabled(enabled) => {
                            saved_settings.agc_enabled = enabled;
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_agc(enabled);
                            }
                            info!("[audio] set agc_enabled={enabled}");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetAgcPreset(preset) => {
                            saved_settings.agc_preset = preset;
                            saved_settings.agc_target_db = preset.target_db();
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_agc_preset(preset);
                            }
                            info!("[audio] set agc_preset={}", preset.label());
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetAgcTargetDb(target_db) => {
                            saved_settings.agc_target_db = target_db;
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_agc_target(target_db);
                            }
                            info!("[audio] set agc_target={target_db:.1} dBFS");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetEchoCancellation(enabled) => {
                            saved_settings.echo_cancellation = enabled;
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_echo_cancellation(enabled);
                            }
                            info!("[audio] set echo_cancellation={enabled}");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetTypingAttenuation(enabled) => {
                            saved_settings.typing_attenuation = enabled;
                            audio_runtime
                                .typing_attenuation
                                .store(enabled, Ordering::Relaxed);
                            info!("[audio] set typing_attenuation={enabled}");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetFecMode(mode) => {
                            saved_settings.fec_mode = mode;
                            audio_runtime.fec_mode.store(mode as u32, Ordering::Relaxed);
                            let mut enc = encoder.lock().await;
                            if let Err(e) = apply_fec_encoder_settings(&mut enc, &audio_runtime) {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to apply FEC mode: {e:#}"
                                )));
                            }
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetFecStrength(strength) => {
                            saved_settings.fec_strength = strength.min(100);
                            audio_runtime
                                .fec_strength
                                .store(saved_settings.fec_strength as u32, Ordering::Relaxed);
                            let mut enc = encoder.lock().await;
                            if let Err(e) = apply_fec_encoder_settings(&mut enc, &audio_runtime) {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to apply FEC strength: {e:#}"
                                )));
                            }
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetVadThreshold(threshold) => {
                            saved_settings.vad_threshold = threshold;
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_vad_threshold(threshold);
                            }
                            info!("[audio] set vad_threshold={threshold:.2}");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetInputGain(gain) => {
                            saved_settings.input_gain = gain;
                            input_gain.store(f32_to_u32(gain), Ordering::Relaxed);
                            info!("[audio] set input_gain={gain:.2}");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetOutputGain(gain) => {
                            saved_settings.output_gain = gain;
                            output_gain.store(f32_to_u32(gain), Ordering::Relaxed);
                            info!("[audio] set output_gain={gain:.2}");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetOutputAutoLevel(enabled) => {
                            saved_settings.output_auto_level = enabled;
                            audio_runtime.output_auto_level.store(enabled, Ordering::Relaxed);
                            info!("[audio] set output_auto_level={enabled}");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetMonoExpansion(enabled) => {
                            saved_settings.mono_expansion = enabled;
                            audio_runtime.mono_expansion.store(enabled, Ordering::Relaxed);
                            info!("[audio] set mono_expansion={enabled}");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetComfortNoise(enabled) => {
                            saved_settings.comfort_noise = enabled;
                            audio_runtime.comfort_noise.store(enabled, Ordering::Relaxed);
                            info!("[audio] set comfort_noise={enabled}");
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetComfortNoiseLevel(level) => {
                            saved_settings.comfort_noise_level = level.clamp(0.0, 0.1);
                            audio_runtime.comfort_noise_level.store(
                                f32_to_u32(saved_settings.comfort_noise_level),
                                Ordering::Relaxed,
                            );
                            info!(
                                "[audio] set comfort_noise_level={:.3}",
                                saved_settings.comfort_noise_level
                            );
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetDuckingEnabled(enabled) => {
                            saved_settings.ducking_enabled = enabled;
                            audio_runtime.ducking_enabled.store(enabled, Ordering::Relaxed);
                            info!(
                                "[audio] set ducking enabled={} attenuation_db={}",
                                enabled, saved_settings.ducking_attenuation_db
                            );
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetDuckingAttenuationDb(db) => {
                            saved_settings.ducking_attenuation_db = db.clamp(-40, 0);
                            audio_runtime.ducking_attenuation_db.store(
                                f32_to_u32(saved_settings.ducking_attenuation_db as f32),
                                Ordering::Relaxed,
                            );
                            info!(
                                "[audio] set ducking enabled={} attenuation_db={}",
                                saved_settings.ducking_enabled, saved_settings.ducking_attenuation_db
                            );
                            persist_settings(tx_event, &saved_settings);
                        }
                        UiIntent::SetUserOutputGain { user_id, gain } => {
                            if let Ok(mut per_user) = per_user_audio.write() {
                                per_user.entry(user_id).or_default().gain = gain.clamp(0.0, 2.0);
                            }
                        }
                        UiIntent::SetUserLocalMute { user_id, muted } => {
                            if let Ok(mut per_user) = per_user_audio.write() {
                                per_user.entry(user_id).or_default().muted = muted;
                            }
                        }
                        UiIntent::ToggleLoopback => {
                            let new = !loopback_active.load(Ordering::Relaxed);
                            loopback_active.store(new, Ordering::Relaxed);
                            let _ = tx_event.send(UiEvent::SetLoopbackActive(new));
                            let _ = tx_event.send(UiEvent::AppendLog(
                                format!("[audio] loopback: {new}"),
                            ));
                        }
                        UiIntent::StartScreenShare { selection } => {
                            if !matches!(share_state, ShareState::Idle) {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[video] start share ignored (state={share_state:?})"
                                )));
                                continue;
                            }
                            let source = map_share_selection(selection);
                            let include_audio = saved_settings.screen_share_capture_audio
                                && platform_supports_system_audio();
                            let available_codecs = net::dispatcher::available_screen_share_codecs();
                            if available_codecs.is_empty() {
                                let _ = tx_event.send(UiEvent::AppendLog(
                                    "[video] start share aborted: no supported codecs available".into(),
                                ));
                                continue;
                            }
                            let selected_codec = if available_codecs
                                .iter()
                                .any(|codec| *codec == saved_settings.screen_share_codec)
                            {
                                saved_settings.screen_share_codec.clone()
                            } else {
                                available_codecs.first().copied().unwrap_or("VP9").to_string()
                            };
                            let preferred_codec = match selected_codec.as_str() {
                                "AV1" => pb::video_caps::Codec::Av1,
                                "VP8" => pb::video_caps::Codec::Vp8,
                                _ => pb::video_caps::Codec::Vp9,
                            };
                            let (profile_layer, sender_profile) = if saved_settings.screen_share_profile == "1440p60" {
                                (pb::SimulcastLayer {
                                    layer_id: 2,
                                    width: 2560,
                                    height: 1440,
                                    max_fps: 60,
                                    max_bitrate_bps: 16_000_000,
                                }, VideoStreamProfile::P1440p60)
                            } else {
                                (pb::SimulcastLayer {
                                    layer_id: 1,
                                    width: 1920,
                                    height: 1080,
                                    max_fps: 60,
                                    max_bitrate_bps: 8_000_000,
                                }, VideoStreamProfile::P1080p60)
                            };
                            let req = pb::StartScreenShareRequest {
                                channel_id: active_channel.as_ref().map(|id| pb::ChannelId { value: id.clone() }),
                                codec: preferred_codec as i32,
                                layers: vec![profile_layer],
                                include_audio,
                            };
                            match dispatcher
                                .send_request(pb::client_to_server::Payload::StartScreenShareRequest(req), Duration::from_secs(5))
                                .await
                            {
                                Ok(Ok(resp)) => {
                                    if let Some(pb::server_to_client::Payload::StartScreenShareResponse(r)) = resp.payload {
                                        let mut negotiated_streams = Vec::new();
                                        negotiated_streams.push((
                                            r.primary_stream_tag,
                                            pb::VideoCodec::try_from(r.primary_codec)
                                                .unwrap_or(pb::VideoCodec::Unspecified),
                                        ));
                                        if let (Some(fallback_tag), Some(fallback_codec)) =
                                            (r.fallback_stream_tag, r.fallback_codec)
                                        {
                                            negotiated_streams.push((
                                                fallback_tag,
                                                pb::VideoCodec::try_from(fallback_codec)
                                                    .unwrap_or(pb::VideoCodec::Unspecified),
                                            ));
                                        }

                                        active_local_stream_id = r.stream_id.clone();
                                        {
                                            let mut streams = stream_state.active_streams.write().await;
                                            for (stream_tag, _) in &negotiated_streams {
                                                streams
                                                    .entry(*stream_tag)
                                                    .or_insert_with(|| Arc::new(Mutex::new(VideoReceiver::new(4, vp_voice::MAX_FRAGS_PER_FRAME))));
                                            }
                                        }
                                        {
                                            let mut stream_codecs = stream_state.stream_codecs.write().await;
                                            for (stream_tag, codec) in &negotiated_streams {
                                                stream_codecs.insert(*stream_tag, *codec);
                                            }
                                        }
                                        let stream_descriptions = negotiated_streams
                                            .iter()
                                            .map(|(stream_tag, codec)| {
                                                format!("{stream_tag}:{}", video_codec_name(*codec))
                                            })
                                            .collect::<Vec<_>>()
                                            .join(", ");
                                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                                            "[video] StartScreenShareRequest ok streams=[{stream_descriptions}] source={source:?} include_audio={include_audio}"
                                        )));
                                        share_state = ShareState::Active;
                                        let (share_stop_tx, share_stop_rx) = watch::channel(false);
                                        active_share_stop = Some(share_stop_tx);

                                        let egress_send = egress.clone();
                                        let video_counters = stream_state.counters.clone();
                                        let selected_profile = saved_settings.screen_share_profile.clone();
                                        tokio::spawn(async move {
                                            let stream_senders = negotiated_streams
                                                .into_iter()
                                                .filter_map(|(stream_tag, codec)| {
                                                    let codec_name = video_codec_encoder_name(codec)?;
                                                    let mut sender =
                                                        VideoSender::new(stream_tag, 0, sender_profile, mtu);
                                                    sender.set_pacing_enabled(false);
                                                    Some((stream_tag, codec_name.to_string(), sender))
                                                })
                                                .collect::<Vec<_>>();

                                            if stream_senders.is_empty() {
                                                warn!(
                                                    "[video] no encodable stream codecs negotiated; aborting local send"
                                                );
                                                return;
                                            }

                                            let (capture_tx, mut capture_rx) = mpsc::channel::<CapturedFrame>(4);
                                            let stop_flag = Arc::new(AtomicBool::new(false));

                                            let watcher_flag = stop_flag.clone();
                                            let mut stop_watch = share_stop_rx.clone();
                                            let stop_watch_task = tokio::spawn(async move {
                                                while stop_watch.changed().await.is_ok() {
                                                    if *stop_watch.borrow() {
                                                        watcher_flag.store(true, Ordering::Relaxed);
                                                        break;
                                                    }
                                                }
                                            });

                                            let capture_source = source.clone();
                                            let capture_stop = stop_flag.clone();
                                            let capture_task = tokio::task::spawn_blocking(move || {
                                                let mut cap = match build_screen_capture(&capture_source) {
                                                    Ok(cap) => cap,
                                                    Err(e) => {
                                                        warn!(error=?e, "[video] failed to build screen capture backend");
                                                        return;
                                                    }
                                                };
                                                while !capture_stop.load(Ordering::Relaxed) {
                                                    match cap.next_frame() {
                                                        Ok(frame) => {
                                                            let _ = capture_tx.try_send(frame);
                                                        }
                                                        Err(e) => {
                                                            warn!(error=?e, "[video] capture frame failed");
                                                            break;
                                                        }
                                                    }
                                                }
                                            });

                                            let send_task = tokio::spawn(async move {
                                                let mut stream_encoders = stream_senders
                                                    .into_iter()
                                                    .filter_map(|(stream_tag, codec_name, sender)| {
                                                        let encoder = match build_screen_encoder(
                                                            &codec_name,
                                                            &selected_profile,
                                                        ) {
                                                            Ok(enc) => enc,
                                                            Err(e) => {
                                                                warn!(
                                                                    error=?e,
                                                                    stream_tag,
                                                                    codec=%codec_name,
                                                                    "[video] failed to build screen encoder"
                                                                );
                                                                return None;
                                                            }
                                                        };
                                                        Some((stream_tag, sender, encoder, 0_u32))
                                                    })
                                                    .collect::<Vec<_>>();

                                                if stream_encoders.is_empty() {
                                                    warn!(
                                                        "[video] no screen encoders available for negotiated stream set"
                                                    );
                                                    return;
                                                }

                                                while let Some(mut frame) = capture_rx.recv().await {
                                                    while let Ok(next) = capture_rx.try_recv() {
                                                        frame = next;
                                                    }

                                                    for (stream_tag, sender, encoder, frame_idx) in
                                                        &mut stream_encoders
                                                    {
                                                        let encoded = match encoder.encode(frame.clone()) {
                                                            Ok(encoded) => encoded,
                                                            Err(e) => {
                                                                warn!(
                                                                    error=?e,
                                                                    stream_tag,
                                                                    "[video] encode failed"
                                                                );
                                                                continue;
                                                            }
                                                        };

                                                        if let Err(e) = sender
                                                            .send_frame_async(
                                                                encoded.ts_ms,
                                                                encoded.is_keyframe,
                                                                &encoded.data,
                                                                |dg| {
                                                                    match egress_send.enqueue_video_fragment(
                                                                        *stream_tag,
                                                                        *frame_idx,
                                                                        encoded.is_keyframe,
                                                                        std::time::Instant::now(),
                                                                        dg,
                                                                    ) {
                                                                        Ok(report) => {
                                                                            video_counters
                                                                                .video_tx_datagrams
                                                                                .fetch_add(1, Ordering::Relaxed);
                                                                            if let Some(dropped) = report.dropped {
                                                                                video_counters
                                                                                    .video_tx_drop_queue_full
                                                                                    .fetch_add(
                                                                                        dropped.count as u64,
                                                                                        Ordering::Relaxed,
                                                                                    );
                                                                            }
                                                                        }
                                                                        Err(reason) => {
                                                                            video_counters
                                                                                .video_tx_drop_deadline
                                                                                .fetch_add(1, Ordering::Relaxed);
                                                                            warn!(
                                                                                ?reason,
                                                                                stream_tag,
                                                                                frame_idx,
                                                                                "[video] enqueue rejected"
                                                                            );
                                                                        }
                                                                    }
                                                                },
                                                            )
                                                            .await
                                                        {
                                                            video_counters
                                                                .sender_frame_errors
                                                                .fetch_add(1, Ordering::Relaxed);
                                                            warn!(
                                                                error=?e,
                                                                stream_tag,
                                                                frame_size=encoded.data.len(),
                                                                "[video] send_frame failed"
                                                            );
                                                            break;
                                                        }
                                                        *frame_idx = frame_idx.wrapping_add(1);
                                                    }
                                                }
                                            });

                                            if include_audio {
                                                warn!("[audio] include_audio requested but system audio capture is currently disabled");
                                            }

                                            let _ = stop_watch_task.await;
                                            let _ = capture_task.await;
                                            let _ = send_task.await;
                                        });
                                    } else {
                                        share_state = ShareState::Idle;
                                        let _ = tx_event.send(UiEvent::AppendLog(
                                            "[video] start share failed: missing StartScreenShareResponse payload"
                                                .into(),
                                        ));
                                    }
                                }
                                Ok(Err(e)) => {
                                    share_state = ShareState::Idle;
                                    let _ = tx_event.send(UiEvent::AppendLog(format!("[video] start share rejected: {e:#}")));
                                }
                                Err(e) => {
                                    share_state = ShareState::Idle;
                                    let _ = tx_event.send(UiEvent::AppendLog(format!("[video] start share failed: {e:#}")));
                                }
                            }
                        }
                        UiIntent::StopScreenShare => {
                            share_state = ShareState::Idle;
                            if let Some(stop_tx) = active_share_stop.take() {
                                let _ = stop_tx.send(true);
                            }
                            if let Some(stream_id) = active_local_stream_id.take() {
                                let req = pb::StopScreenShareRequest { stream_id: Some(stream_id.clone()) };
                                let _ = dispatcher
                                    .send_request(pb::client_to_server::Payload::StopScreenShareRequest(req), Duration::from_secs(5))
                                    .await;
                                let _ = tx_event.send(UiEvent::AppendLog(format!("[video] StopScreenShareRequest stream_id={}", stream_id.value)));
                            }
                        }
                        UiIntent::SetInputDevice(dev) => {
                            {
                                let mut state = selected_audio.lock().await;
                                state.input_device = dev;
                            }
                            if let Err(e) = restart_audio_streams(
                                &capture,
                                &playout,
                                &selected_audio,
                                tx_event,
                                sample_rate,
                                channels,
                                frame_ms,
                            )
                            .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to switch input device: {e:#}"
                                )));
                            }
                        }
                        UiIntent::SetOutputDevice(dev) => {
                            {
                                let mut state = selected_audio.lock().await;
                                state.output_device = dev;
                            }
                            if let Err(e) = restart_audio_streams(
                                &capture,
                                &playout,
                                &selected_audio,
                                tx_event,
                                sample_rate,
                                channels,
                                frame_ms,
                            )
                            .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to switch output device: {e:#}"
                                )));
                            }
                        }
                        UiIntent::SetCaptureMode(mode) => {
                            {
                                let mut state = selected_audio.lock().await;
                                state.capture_mode = normalize_capture_mode(&mode);
                            }

                            if let Err(e) = restart_audio_streams(
                                &capture,
                                &playout,
                                &selected_audio,
                                tx_event,
                                sample_rate,
                                channels,
                                frame_ms,
                            )
                            .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to switch capture mode: {e:#}"
                                )));
                            }
                        }
                        UiIntent::SetPlaybackMode(mode) => {
                            {
                                let mut state = selected_audio.lock().await;
                                state.playback_mode = normalize_playback_mode(&mode);
                            }

                            if let Err(e) = restart_audio_streams(
                                &capture,
                                &playout,
                                &selected_audio,
                                tx_event,
                                sample_rate,
                                channels,
                                frame_ms,
                            )
                            .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to switch playback mode: {e:#}"
                                )));
                            }
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
                        UiIntent::UpdateUserProfile {
                            display_name,
                            description,
                            accent_color,
                            links,
                        } => {
                            match dispatcher
                                .update_user_profile(display_name, description, accent_color, links)
                                .await
                            {
                                Ok(()) => {
                                    let _ = tx_event.send(UiEvent::ProfileUpdateComplete);
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        "[profile] profile saved".to_string(),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::ProfileUpdateFailed(
                                        e.to_string(),
                                    ));
                                }
                            }
                        }
                        UiIntent::UploadProfileAvatar { path } => {
                            match upload_profile_image(
                                &conn,
                                &dispatcher,
                                &path,
                                "profile_avatar",
                                8 * 1024 * 1024,
                                256,
                                256,
                            )
                            .await
                            {
                                Ok(asset_id) => {
                                    // Attach to profile.
                                    match dispatcher.set_avatar(&asset_id).await {
                                        Ok(()) => {
                                            let preview = format!(
                                                "file://{}",
                                                path.to_string_lossy()
                                            );
                                            let _ = tx_event.send(UiEvent::AvatarUploadComplete {
                                                asset_id,
                                                preview_url: preview,
                                            });
                                        }
                                        Err(e) => {
                                            let _ = tx_event.send(UiEvent::AvatarUploadFailed(
                                                format!("set avatar failed: {e:#}"),
                                            ));
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = tx_event
                                        .send(UiEvent::AvatarUploadFailed(e.to_string()));
                                }
                            }
                        }
                        UiIntent::UploadProfileBanner { path } => {
                            match upload_profile_image(
                                &conn,
                                &dispatcher,
                                &path,
                                "profile_banner",
                                10 * 1024 * 1024,
                                680,
                                240,
                            )
                            .await
                            {
                                Ok(asset_id) => {
                                    match dispatcher.set_banner(&asset_id).await {
                                        Ok(()) => {
                                            let preview = format!(
                                                "file://{}",
                                                path.to_string_lossy()
                                            );
                                            let _ = tx_event.send(UiEvent::BannerUploadComplete {
                                                asset_id,
                                                preview_url: preview,
                                            });
                                        }
                                        Err(e) => {
                                            let _ = tx_event.send(UiEvent::BannerUploadFailed(
                                                format!("set banner failed: {e:#}"),
                                            ));
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = tx_event
                                        .send(UiEvent::BannerUploadFailed(e.to_string()));
                                }
                            }
                        }
                        UiIntent::RemoveAvatar => {
                            match dispatcher.set_avatar("").await {
                                Ok(()) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        "[profile] avatar removed".to_string(),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[profile] remove avatar failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::RemoveBanner => {
                            match dispatcher.set_banner("").await {
                                Ok(()) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        "[profile] banner removed".to_string(),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[profile] remove banner failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::SetCustomStatus {
                            status_text,
                            status_emoji,
                        } => {
                            match dispatcher
                                .set_custom_status(status_text, status_emoji)
                                .await
                            {
                                Ok(()) => {
                                    let _ = tx_event.send(UiEvent::CustomStatusSet);
                                }
                                Err(e) => {
                                    let _ = tx_event
                                        .send(UiEvent::CustomStatusFailed(e.to_string()));
                                }
                            }
                        }
                        UiIntent::FetchSelfProfile => {
                            match dispatcher.fetch_self_profile(&local_user_id).await {
                                Ok(profile) => {
                                    let _ = tx_event.send(UiEvent::SelfProfileLoaded(profile));
                                }
                                Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(
                                        format!("[profile] fetch self profile failed: {e:#}"),
                                    ));
                                }
                            }
                        }
                        UiIntent::ApplySettings(ref settings) => {
                            *saved_settings = (**settings).clone();
                            dsp_enabled.store(
                                settings.dsp_enabled && !cfg.no_noise_suppression,
                                Ordering::Relaxed,
                            );
                            apply_resampler_mode(settings.dsp_method);
                            // Apply all settings to the audio pipeline
                            if let Some(ref dsp) = capture_dsp {
                                let mut d = dsp.lock().await;
                                d.set_noise_suppression(settings.noise_suppression);
                                d.set_agc(settings.agc_enabled);
                                d.set_vad_threshold(settings.vad_threshold);
                                d.set_agc_preset(settings.agc_preset);
                                d.set_agc_target(settings.agc_target_db);
                                d.set_echo_cancellation(settings.echo_cancellation);
                                d.set_echo_reference_enabled(should_enable_aec_reference(&settings.playback_device));
                            }
                            input_gain.store(f32_to_u32(settings.input_gain), Ordering::Relaxed);
                            output_gain.store(f32_to_u32(settings.output_gain), Ordering::Relaxed);
                            if let Ok(mut per_user) = per_user_audio.write() {
                                *per_user = settings.per_user_audio.clone();
                            }
                            audio_runtime.apply(settings);
                            info!(
                                "[audio] apply settings dsp_enabled={} dsp_method={} ns={} agc={} agc_preset={} agc_target={:.1} aec={} typing_attn={} fec={:?} fec_strength={} auto_level={} mono_expansion={} comfort_noise={} comfort_noise_level={:.3} ducking={} duck_db={}",
                                settings.dsp_enabled,
                                settings.dsp_method.label(),
                                settings.noise_suppression,
                                settings.agc_enabled,
                                settings.agc_preset.label(),
                                settings.agc_target_db,
                                settings.echo_cancellation,
                                settings.typing_attenuation,
                                settings.fec_mode,
                                settings.fec_strength,
                                settings.output_auto_level,
                                settings.mono_expansion,
                                settings.comfort_noise,
                                settings.comfort_noise_level,
                                settings.ducking_enabled,
                                settings.ducking_attenuation_db
                            );
                            {
                                let mut enc = encoder.lock().await;
                                if let Err(e) = apply_fec_encoder_settings(&mut enc, &audio_runtime) {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[audio] failed to apply FEC settings: {e:#}"
                                    )));
                                }
                            }

                            // Update PTT mode
                            let is_ptt = settings.capture_mode == ui::model::CaptureMode::PushToTalk;
                            ptt_active.store(!is_ptt, Ordering::Relaxed);
                            ptt_state.pressed = !is_ptt;
                            if !is_ptt {
                                ptt_state.release_deadline = None;
                            }
                            capture_mode.store(
                                capture_mode_to_u8(settings.capture_mode),
                                Ordering::Relaxed,
                            );

                            {
                                let mut state = selected_audio.lock().await;
                                state.input_device = settings.capture_device.clone();
                                state.output_device = settings.playback_device.clone();
                                state.capture_mode = normalize_capture_mode(&settings.capture_backend_mode);
                                state.playback_mode = normalize_playback_mode(&settings.playback_mode);
                                if let Some(ref dsp) = capture_dsp {
                                    let mut d = dsp.lock().await;
                                    d.set_echo_reference_enabled(should_enable_aec_reference(&state.output_device));
                                }
                            }

                            if let Err(e) = restart_audio_streams(
                                &capture,
                                &playout,
                                &selected_audio,
                                tx_event,
                                sample_rate,
                                channels,
                                frame_ms,
                            )
                            .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[audio] failed to apply audio device settings: {e:#}"
                                )));
                            }

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
                        UiIntent::PermsOpen => {
                            let req = pb::PermListRolesRequest {
                                server_id: None,
                                include_caps: false,
                            };
                            let mut role_order: Vec<String> = Vec::new();
                            match dispatcher
                                .send_request(
                                    pb::client_to_server::Payload::PermListRoles(req),
                                    Duration::from_secs(5),
                                )
                                .await
                            {
                                Ok(Ok(resp)) => {
                                    if let Some(pb::server_to_client::Payload::PermListRoles(
                                        payload,
                                    )) = resp.payload
                                    {
                                        let roles = payload
                                            .roles
                                            .into_iter()
                                            .map(|r| {
                                                role_order.push(r.role_id.clone());
                                                ui::model::RoleDraft {
                                                    role_id: r.role_id,
                                                    name: r.name,
                                                    color_hex: format!("#{:06X}", r.color & 0x00FF_FFFF),
                                                    member_count: 0,
                                                    hoist: false,
                                                    mentionable: false,
                                                    protected: r.is_everyone || r.is_system,
                                                    administrative: false,
                                                }
                                            })
                                            .collect();
                                        let _ = tx_event.send(UiEvent::PermissionsRolesLoaded { roles });
                                    }
                                }
                                Ok(Err(e)) | Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[perm] list roles failed: {e:#}"
                                    )));
                                }
                            }

                            let req = pb::PermListUsersRequest {
                                server_id: None,
                            };
                            match dispatcher
                                .send_request(
                                    pb::client_to_server::Payload::PermListUsers(req),
                                    Duration::from_secs(5),
                                )
                                .await
                            {
                                Ok(Ok(resp)) => {
                                    if let Some(pb::server_to_client::Payload::PermListUsers(
                                        payload,
                                    )) = resp.payload
                                    {
                                        let mut members: Vec<ui::model::MemberPermissionDraft> = payload
                                            .users
                                            .into_iter()
                                            .map(|u| ui::model::MemberPermissionDraft {
                                                display_name: if u.display_name.trim().is_empty() {
                                                    u.user_id
                                                        .as_ref()
                                                        .map(|id| id.value.clone())
                                                        .unwrap_or_else(|| "unknown".into())
                                                } else {
                                                    u.display_name
                                                },
                                                user_id: u
                                                    .user_id
                                                    .map(|id| id.value)
                                                    .unwrap_or_default(),
                                                highest_role_index: u
                                                    .highest_role_position
                                                    .max(0)
                                                    as usize,
                                                role_assignments: role_order
                                                    .iter()
                                                    .map(|id| u.role_ids.iter().any(|rid| rid == id))
                                                    .collect(),
                                                role_ids: u.role_ids,
                                                can_mute_members: u.is_admin,
                                                can_deafen_members: u.is_admin,
                                                can_move_members: u.is_admin,
                                                can_kick_members: u.is_admin,
                                            })
                                            .collect();
                                        for member in &mut members {
                                            if member.role_assignments.len() < role_order.len() {
                                                member.role_assignments.resize(role_order.len(), false);
                                            }
                                        }
                                        let _ = tx_event.send(UiEvent::PermissionsMembersLoaded {
                                            members,
                                            current_user_max_role: payload
                                                .editor_highest_role_position
                                                .max(0)
                                                as usize,
                                            can_moderate_members: payload.editor_is_admin,
                                        });
                                    } else {
                                        let _ = tx_event.send(UiEvent::AppendLog(
                                            "[perm] unexpected response to perm_list_users".into(),
                                        ));
                                    }
                                }
                                Ok(Err(e)) | Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[perm] list users failed: {e:#}"
                                    )));
                                }
                            }

                            if let Some(active_channel_id) = active_channel.clone() {
                                let req = pb::PermListChannelOverridesRequest {
                                    server_id: None,
                                    channel_id: Some(pb::ChannelId {
                                        value: active_channel_id.clone(),
                                    }),
                                };
                                match dispatcher
                                    .send_request(
                                        pb::client_to_server::Payload::PermListChanOvr(req),
                                        Duration::from_secs(5),
                                    )
                                    .await
                                {
                                    Ok(Ok(resp)) => {
                                        if let Some(pb::server_to_client::Payload::PermListChanOvr(payload)) = resp.payload {
                                            let mut role_overrides = Vec::new();
                                            let mut member_overrides = Vec::new();
                                            for ov in payload.overrides {
                                                let mut row = ui::model::PermissionOverrideDraft {
                                                    role_id: None,
                                                    user_id: None,
                                                    subject_name: "unknown".into(),
                                                    capabilities: vec![ui::model::PermissionValue::Inherit; 4],
                                                };
                                                match ov.target {
                                                    Some(pb::perm_channel_override::Target::RoleId(role_id)) => {
                                                        row.subject_name = role_id.clone();
                                                        row.role_id = Some(role_id);
                                                        role_overrides.push(row);
                                                    }
                                                    Some(pb::perm_channel_override::Target::UserId(user_id)) => {
                                                        row.subject_name = user_id.value.clone();
                                                        row.user_id = Some(user_id.value);
                                                        member_overrides.push(row);
                                                    }
                                                    None => {}
                                                }
                                            }
                                            let _ = tx_event.send(UiEvent::PermissionsChannelOverridesLoaded {
                                                channel_id: active_channel_id,
                                                role_overrides,
                                                member_overrides,
                                            });
                                        }
                                    }
                                    Ok(Err(e)) | Err(e) => {
                                        let _ = tx_event.send(UiEvent::AppendLog(format!(
                                            "[perm] list channel overrides failed: {e:#}"
                                        )));
                                    }
                                }
                            }

                            let req = pb::PermAuditQueryRequest {
                                server_id: None,
                                limit: 100,
                                offset: 0,
                            };
                            match dispatcher
                                .send_request(
                                    pb::client_to_server::Payload::PermAuditQuery(req),
                                    Duration::from_secs(5),
                                )
                                .await
                            {
                                Ok(Ok(resp)) => {
                                    if let Some(pb::server_to_client::Payload::PermAuditQuery(
                                        payload,
                                    )) = resp.payload
                                    {
                                        let rows = payload
                                            .rows
                                            .into_iter()
                                            .map(|row| ui::model::PermissionAuditRow {
                                                action: row.action,
                                                target_type: row.target_type,
                                                target_id: row.target_id,
                                                created_at_unix_millis: row
                                                    .created_at
                                                    .map(|ts| ts.unix_millis),
                                            })
                                            .collect();
                                        let _ = tx_event.send(UiEvent::PermissionsAuditLoaded {
                                            rows,
                                        });
                                    } else {
                                        let _ = tx_event.send(UiEvent::AppendLog(
                                            "[perm] unexpected response to perm_audit_query".into(),
                                        ));
                                    }
                                }
                                Ok(Err(e)) | Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[perm] audit query failed: {e:#}"
                                    )));
                                }
                            }
                        }
                        UiIntent::PokeUser { user_id, message } => {
                            if let Err(e) = dispatcher.poke_user(&user_id, &message).await {
                                let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] poke failed: {e:#}")));
                            } else {
                                let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] poked {user_id}")));
                            }
                        }
                        UiIntent::FetchUserProfile { user_id } => {
                            let request = pb::GetUserProfileRequest {
                                user_id: Some(pb::UserId { value: user_id.clone() }),
                            };
                            match dispatcher
                                .send_request(
                                    pb::client_to_server::Payload::GetUserProfileRequest(request),
                                    Duration::from_secs(5),
                                )
                                .await
                            {
                                Ok(Ok(resp)) => {
                                    if let Some(pb::server_to_client::Payload::GetUserProfileResponse(payload)) = resp.payload {
                                        if let Some(profile) = payload.profile {
                                            let user_profile = ui::model::UserProfileData {
                                                user_id: profile.user_id.map(|u| u.value).unwrap_or_default(),
                                                display_name: profile.display_name,
                                                description: profile.description,
                                                status: ui_status_from_pb(profile.status),
                                                custom_status_text: profile.custom_status_text,
                                                custom_status_emoji: profile.custom_status_emoji,
                                                accent_color: profile.accent_color,
                                                avatar_url: (!profile.avatar_asset_url.is_empty()).then_some(profile.avatar_asset_url),
                                                banner_url: (!profile.banner_asset_url.is_empty()).then_some(profile.banner_asset_url),
                                                badges: profile.badges.into_iter().map(|badge| ui::model::BadgeData {
                                                    id: badge.id,
                                                    label: badge.label,
                                                    icon_url: badge.icon_url,
                                                    tooltip: badge.tooltip,
                                                }).collect(),
                                                links: profile.links.into_iter().map(|link| ui::model::ProfileLinkData {
                                                    platform: link.platform,
                                                    url: link.url,
                                                    display_text: link.display_text,
                                                    verified: link.verified,
                                                }).collect(),
                                                created_at: profile.created_at.map(|ts| ts.unix_millis).unwrap_or_default(),
                                                last_seen_at: profile.last_seen_at.map(|ts| ts.unix_millis).unwrap_or_default(),
                                                current_activity: profile.current_activity.map(|activity| ui::model::GameActivityData {
                                                    game_name: activity.game_name,
                                                    details: activity.details,
                                                    state: activity.state,
                                                    started_at: activity.started_at.map(|ts| ts.unix_millis).unwrap_or_default(),
                                                    large_image_url: activity.large_image_url,
                                                }),
                                                roles: Vec::new(),
                                            };
                                            let _ = tx_event.send(UiEvent::UserProfileLoaded(user_profile));
                                        }
                                    }
                                }
                                Ok(Err(e)) | Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!("[profile] failed to fetch profile for {user_id}: {e:#}")));
                                    let _ = tx_event.send(UiEvent::UserProfileFetchFailed { user_id });
                                }
                            }
                        }
                        UiIntent::CreateDmChannel { participant_user_ids } => {
                            let request = pb::CreateDmChannelRequest {
                                participant_user_ids: participant_user_ids
                                    .into_iter()
                                    .map(|id| pb::UserId { value: id })
                                    .collect(),
                                name: String::new(),
                            };
                            match dispatcher
                                .send_request(
                                    pb::client_to_server::Payload::CreateDmChannelRequest(request),
                                    Duration::from_secs(5),
                                )
                                .await
                            {
                                Ok(Ok(_)) => {
                                    let _ = tx_event.send(UiEvent::AppendLog("[dm] opened direct message".into()));
                                }
                                Ok(Err(e)) | Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!("[dm] open failed: {e:#}")));
                                }
                            }
                        }
                        UiIntent::MuteUser { user_id, muted } => {
                            if let Some(ref ch) = active_channel {
                                let action = pb::moderation_action_request::Action::Mute(pb::MuteUser { muted, duration_seconds: 0 });
                                if let Err(e) = dispatcher.moderate_user(ch, &user_id, action).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] mute failed: {e:#}")));
                                }
                            }
                        }
                        UiIntent::DeafenUser { user_id, deafened } => {
                            if let Some(ref ch) = active_channel {
                                let action = pb::moderation_action_request::Action::Deafen(pb::DeafenUser { deafened, duration_seconds: 0 });
                                if let Err(e) = dispatcher.moderate_user(ch, &user_id, action).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] deafen failed: {e:#}")));
                                }
                            }
                        }
                        UiIntent::KickUser { user_id, reason } => {
                            if let Some(ref ch) = active_channel {
                                let action = pb::moderation_action_request::Action::Kick(pb::KickUser { reason });
                                if let Err(e) = dispatcher.moderate_user(ch, &user_id, action).await {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!("[moderation] kick failed: {e:#}")));
                                }
                            }
                        }
                        UiIntent::PermsSaveRoleEdits {
                            role_id,
                            name,
                            color,
                            position,
                            caps,
                        } => {
                            let req = pb::PermUpsertRoleRequest {
                                server_id: None,
                                role_id,
                                name,
                                color,
                                position,
                            };
                            match dispatcher
                                .send_request(
                                    pb::client_to_server::Payload::PermUpsertRole(req),
                                    Duration::from_secs(5),
                                )
                                .await
                            {
                                Ok(Ok(resp)) => {
                                    let Some(pb::server_to_client::Payload::PermUpsertRole(payload)) = resp.payload else { continue; };
                                    if let Some(role) = payload.role {
                                        let req = pb::PermSetRoleCapsRequest {
                                            server_id: None,
                                            role_id: role.role_id,
                                            caps: caps
                                                .into_iter()
                                                .map(|(cap, effect)| pb::PermCapabilityEffect { cap, effect })
                                                .collect(),
                                            cap_updates: vec![],
                                        };
                                        let _ = dispatcher
                                            .send_request(
                                                pb::client_to_server::Payload::PermSetRoleCaps(req),
                                                Duration::from_secs(5),
                                            )
                                            .await;
                                    }
                                }
                                Ok(Err(e)) | Err(e) => {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[perm] save role failed: {e:#}"
                                    )));
                                }
                            }
                        }
                        UiIntent::PermsDeleteRole { role_id } => {
                            let req = pb::PermDeleteRoleRequest {
                                server_id: None,
                                role_id,
                            };
                            if let Err(e) = dispatcher
                                .send_request(
                                    pb::client_to_server::Payload::PermDeleteRole(req),
                                    Duration::from_secs(5),
                                )
                                .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[perm] delete role failed: {e:#}"
                                )));
                            }
                        }
                        UiIntent::PermsAssignRoles { user_id, role_ids } => {
                            let req = pb::PermAssignRolesRequest {
                                server_id: None,
                                user_id: Some(pb::UserId { value: user_id }),
                                role_ids,
                            };
                            if let Err(e) = dispatcher
                                .send_request(
                                    pb::client_to_server::Payload::PermAssignRoles(req),
                                    Duration::from_secs(5),
                                )
                                .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[perm] assign roles failed: {e:#}"
                                )));
                            }
                        }
                        UiIntent::GrantBadgeToUser { user_id, badge_id, label, icon_path, tooltip } => {
                            let create_req = pb::CreateBadgeRequest {
                                id: badge_id.clone(),
                                label,
                                icon_asset_id: Some(pb::AssetId { value: icon_path }),
                                tooltip,
                            };
                            let _ = dispatcher
                                .send_request(
                                    pb::client_to_server::Payload::CreateBadge(create_req),
                                    Duration::from_secs(5),
                                )
                                .await;
                            let grant_req = pb::GrantBadgeRequest {
                                user_id: Some(pb::UserId { value: user_id.clone() }),
                                badge_id: badge_id.clone(),
                            };
                            if let Err(e) = dispatcher
                                .send_request(
                                    pb::client_to_server::Payload::GrantBadge(grant_req),
                                    Duration::from_secs(5),
                                )
                                .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[perm] grant badge failed ({badge_id} -> {user_id}): {e:#}"
                                )));
                            } else {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[perm] granted badge {badge_id} to {user_id}"
                                )));
                            }
                        }
                        UiIntent::RevokeBadgeFromUser { user_id, badge_id } => {
                            let revoke_req = pb::RevokeBadgeRequest {
                                user_id: Some(pb::UserId {
                                    value: user_id.clone(),
                                }),
                                badge_id: badge_id.clone(),
                            };
                            if let Err(e) = dispatcher
                                .send_request(
                                    pb::client_to_server::Payload::RevokeBadge(revoke_req),
                                    Duration::from_secs(5),
                                )
                                .await
                            {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[perm] revoke badge failed ({badge_id} -> {user_id}): {e:#}"
                                )));
                            } else {
                                let _ = tx_event.send(UiEvent::AppendLog(format!(
                                    "[perm] revoked badge {badge_id} from {user_id}"
                                )));
                            }
                        }
                        UiIntent::PermsSetChannelOverride {
                            channel_id,
                            role_id,
                            user_id,
                            cap,
                            effect,
                        } => {
                            let target = if let Some(role_id) = role_id {
                                Some(pb::perm_channel_override::Target::RoleId(role_id))
                            } else if let Some(user_id) = user_id {
                                Some(pb::perm_channel_override::Target::UserId(pb::UserId {
                                    value: user_id,
                                }))
                            } else {
                                None
                            };
                            if let Some(target) = target {
                                let req = pb::PermSetChannelOverrideRequest {
                                    r#override: Some(pb::PermChannelOverride {
                                        channel_id: Some(pb::ChannelId { value: channel_id }),
                                        target: Some(target),
                                        cap,
                                        effect,
                                    }),
                                    server_id: None,
                                    channel_id: None,
                                    principal: None,
                                    cap_effects: vec![],
                                };
                                if let Err(e) = dispatcher
                                    .send_request(
                                        pb::client_to_server::Payload::PermSetChanOvr(req),
                                        Duration::from_secs(5),
                                    )
                                    .await
                                {
                                    let _ = tx_event.send(UiEvent::AppendLog(format!(
                                        "[perm] set channel override failed: {e:#}"
                                    )));
                                }
                            }
                        }
                        _ => {
                            // Remaining intents (moderation, file upload, etc.)
                        }
                    }
                }
            }

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let _ = tx_event.send(UiEvent::VoiceSessionHealth(false));
                    return Ok(());
                }
            }

            _ = voice_die_rx.changed() => {
                if *voice_die_rx.borrow() {
                    let _ = tx_event.send(UiEvent::VoiceSessionHealth(false));
                    return Err(anyhow!("voice loop terminated"));
                }
            }

            r = &mut ctl_keepalive => {
                let _ = tx_event.send(UiEvent::VoiceSessionHealth(false));
                return Err(anyhow!("control keepalive ended: {:?}", r));
            }
        }
    }
}

async fn upload_attachment_quic(
    conn: &quinn::Connection,
    channel_id: &str,
    attachment: &ui::model::AttachmentData,
) -> anyhow::Result<ui::model::AttachmentData> {
    use tokio::io::AsyncReadExt;

    let (mut send, mut recv) = conn.open_bi().await.context("open media stream")?;
    let local_path = pending_local_path(attachment)?;
    let mut file = tokio::fs::File::open(&local_path)
        .await
        .with_context(|| format!("open attachment: {}", local_path.display()))?;
    let size_bytes = file.metadata().await?.len();

    let init = pb::MediaRequest {
        payload: Some(pb::media_request::Payload::UploadInit(pb::UploadInit {
            channel_id: Some(pb::ChannelId {
                value: channel_id.to_string(),
            }),
            filename: attachment.filename.clone(),
            mime: attachment.mime_type.clone(),
            size_bytes,
        })),
    };
    net::frame::write_delimited(&mut send, &init).await?;

    let ready: pb::MediaResponse = net::frame::read_delimited(&mut recv, 64 * 1024).await?;
    let max_chunk = match ready.payload {
        Some(pb::media_response::Payload::UploadReady(r)) => usize::max(r.max_chunk as usize, 4096),
        Some(pb::media_response::Payload::Error(e)) => {
            return Err(anyhow!("media upload rejected: {}", e.message))
        }
        _ => return Err(anyhow!("unexpected media upload response")),
    };

    let mut buf = vec![0u8; max_chunk];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        send.write_all(&buf[..n]).await?;
    }

    let complete: pb::MediaResponse = net::frame::read_delimited(&mut recv, 64 * 1024).await?;
    match complete.payload {
        Some(pb::media_response::Payload::UploadComplete(done)) => Ok(ui::model::AttachmentData {
            asset: AttachmentAsset::UploadedAssetId(
                done.attachment_id.map(|a| a.value).unwrap_or_default(),
            ),
            filename: done.filename,
            mime_type: done.mime,
            size_bytes: done.size_bytes,
            download_url: String::new(),
            thumbnail_url: None,
        }),
        Some(pb::media_response::Payload::Error(e)) => {
            Err(anyhow!("media upload failed: {}", e.message))
        }
        _ => Err(anyhow!("unexpected media upload completion")),
    }
}

async fn resolve_attachment_local_path(
    conn: &quinn::Connection,
    attachment: &ui::model::AttachmentData,
) -> anyhow::Result<PathBuf> {
    if let AttachmentAsset::PendingLocalPath(local) = &attachment.asset {
        if local.is_absolute() && local.exists() {
            return Ok(local.clone());
        }
    }

    let cache_dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("tsod")
        .join("attachment-cache");
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .with_context(|| format!("create attachment cache dir: {}", cache_dir.display()))?;

    let local_path = cache_dir.join(cached_attachment_filename(attachment));
    if tokio::fs::metadata(&local_path).await.is_ok() {
        return Ok(local_path);
    }

    download_attachment_quic(conn, uploaded_asset_id(attachment)?, &local_path).await?;
    Ok(local_path)
}

async fn download_attachment_quic(
    conn: &quinn::Connection,
    asset_id: &str,
    output_path: &Path,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let (mut send, mut recv) = conn.open_bi().await.context("open media stream")?;
    let req = pb::MediaRequest {
        payload: Some(pb::media_request::Payload::DownloadRequest(
            pb::DownloadRequest {
                attachment_id: Some(pb::AssetId {
                    value: asset_id.to_string(),
                }),
            },
        )),
    };
    net::frame::write_delimited(&mut send, &req).await?;

    let meta: pb::MediaResponse = net::frame::read_delimited(&mut recv, 64 * 1024).await?;
    let expected_size = match meta.payload {
        Some(pb::media_response::Payload::DownloadMeta(meta)) => meta.size_bytes,
        Some(pb::media_response::Payload::Error(e)) => {
            return Err(anyhow!("media download rejected: {}", e.message));
        }
        _ => return Err(anyhow!("unexpected media download response")),
    };

    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create download dir: {}", parent.display()))?;
    }
    let mut file = tokio::fs::File::create(output_path)
        .await
        .with_context(|| format!("create downloaded attachment: {}", output_path.display()))?;

    let copied = tokio::io::copy(&mut recv, &mut file)
        .await
        .context("read media stream")?;
    file.flush().await?;

    if expected_size != 0 && copied != expected_size {
        return Err(anyhow!(
            "download size mismatch (expected {}, got {})",
            expected_size,
            copied
        ));
    }
    Ok(())
}

fn pending_local_path(attachment: &ui::model::AttachmentData) -> anyhow::Result<&Path> {
    match &attachment.asset {
        AttachmentAsset::PendingLocalPath(path) => Ok(path.as_path()),
        AttachmentAsset::UploadedAssetId(asset_id) => {
            Err(anyhow!("attachment already uploaded (asset_id={asset_id})"))
        }
    }
}

fn uploaded_asset_id(attachment: &ui::model::AttachmentData) -> anyhow::Result<&str> {
    match &attachment.asset {
        AttachmentAsset::UploadedAssetId(asset_id) if !asset_id.is_empty() => Ok(asset_id),
        AttachmentAsset::UploadedAssetId(_) => Err(anyhow!("attachment asset_id is empty")),
        AttachmentAsset::PendingLocalPath(path) => Err(anyhow!(
            "attachment has local path and is not uploaded: {}",
            path.display()
        )),
    }
}

fn attachment_source_label(attachment: &ui::model::AttachmentData) -> String {
    match &attachment.asset {
        AttachmentAsset::PendingLocalPath(path) => path.display().to_string(),
        AttachmentAsset::UploadedAssetId(asset_id) => asset_id.clone(),
    }
}

fn cache_asset_key(attachment: &ui::model::AttachmentData) -> String {
    match &attachment.asset {
        AttachmentAsset::UploadedAssetId(asset_id) => asset_id.clone(),
        AttachmentAsset::PendingLocalPath(path) => path
            .to_string_lossy()
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
            .collect(),
    }
}

fn cached_attachment_filename(attachment: &ui::model::AttachmentData) -> String {
    let safe_name = attachment
        .filename
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let safe_name = if safe_name.is_empty() {
        "attachment".to_string()
    } else {
        safe_name
    };
    format!("{}-{}", cache_asset_key(attachment), safe_name)
}

async fn emit_telemetry_loop(
    tx_event: Sender<UiEvent>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    dsp_enabled: Arc<AtomicBool>,
    counters: Arc<VoiceTelemetryCounters>,
    network_telemetry: Arc<SharedNetworkTelemetry>,
    send_queue_drop_count: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    let mut prev_tx_packets = 0u64;
    let mut prev_tx_bytes = 0u64;
    let mut prev_rx_packets = 0u64;
    let mut prev_rx_bytes = 0u64;
    let mut prev_late = 0u64;
    let mut prev_lost = 0u64;
    let mut prev_conceal = 0u64;

    while running.load(Ordering::Relaxed) && !*shutdown_rx.borrow() {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = tick.tick() => {}
        }

        let tx_packets = counters.tx_packets.load(Ordering::Relaxed);
        let tx_bytes = counters.tx_bytes.load(Ordering::Relaxed);
        let rx_packets = counters.rx_packets.load(Ordering::Relaxed);
        let rx_bytes = counters.rx_bytes.load(Ordering::Relaxed);
        let late = counters.late_packets.load(Ordering::Relaxed);
        let lost = counters.lost_packets.load(Ordering::Relaxed);
        let conceal = counters.concealment_frames.load(Ordering::Relaxed);
        let jitter_buffer_depth = counters.jitter_buffer_depth.load(Ordering::Relaxed) as u32;
        let peak_stream_level = f32::from_bits(
            counters
                .peak_stream_level_bits
                .swap(0.0f32.to_bits(), Ordering::Relaxed),
        );

        let tx_pps = tx_packets.saturating_sub(prev_tx_packets) as u32;
        let rx_pps = rx_packets.saturating_sub(prev_rx_packets) as u32;
        let tx_bitrate_bps = (tx_bytes.saturating_sub(prev_tx_bytes) * 8) as u32;
        let rx_bitrate_bps = (rx_bytes.saturating_sub(prev_rx_bytes) * 8) as u32;

        prev_tx_packets = tx_packets;
        prev_tx_bytes = tx_bytes;
        prev_rx_packets = rx_packets;
        prev_rx_bytes = rx_bytes;

        let late_delta = late.saturating_sub(prev_late) as u32;
        let lost_delta = lost.saturating_sub(prev_lost) as u32;
        let conceal_delta = conceal.saturating_sub(prev_conceal) as u32;

        prev_late = late;
        prev_lost = lost;
        prev_conceal = conceal;

        let observed_packets = rx_pps.saturating_add(lost_delta).max(1);
        let loss_rate = (lost_delta as f32 / observed_packets as f32).clamp(0.0, 1.0);
        let rtt_ms = network_telemetry.rtt_ms.load(Ordering::Relaxed);
        let jitter_ms = (jitter_buffer_depth.saturating_mul(4)).clamp(0, 250);
        network_telemetry
            .loss_ppm
            .store((loss_rate * 1_000_000.0) as u32, Ordering::Relaxed);
        network_telemetry
            .jitter_ms
            .store(jitter_ms, Ordering::Relaxed);

        let (agc_gain_db, vad_probability) = if dsp_enabled.load(Ordering::Relaxed) {
            if let Some(ref dsp) = capture_dsp {
                let d = dsp.lock().await;
                (d.agc_gain_db(), d.last_vad_probability())
            } else {
                (0.0, 0.0)
            }
        } else {
            (0.0, 0.0)
        };

        let _ = tx_event.send(UiEvent::TelemetryUpdate(ui::model::TelemetryData {
            rtt_ms,
            loss_rate,
            jitter_ms,
            tx_bitrate_bps,
            rx_bitrate_bps,
            tx_pps,
            rx_pps,
            jitter_buffer_depth,
            late_packets: late_delta,
            lost_packets: lost_delta,
            concealment_frames: conceal_delta,
            peak_stream_level,
            send_queue_drop_count: send_queue_drop_count.load(Ordering::Relaxed),
            playout_delay_ms: counters.playout_delay_ms.load(Ordering::Relaxed),
            agc_gain_db,
            vad_probability,
        }));
    }
}

async fn mic_test_loop(
    capture: Arc<RwLock<Arc<audio::capture::Capture>>>,
    playout: Arc<RwLock<Arc<audio::playout::Playout>>>,
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

        let capture_stream = capture.read().await.clone();
        if !capture_stream.read_frame(&mut pcm) {
            continue;
        }

        let gain = u32_to_f32(input_gain.load(Ordering::Relaxed));
        if (gain - 1.0).abs() > 0.001 {
            for s in pcm.iter_mut() {
                *s = (*s as f32 * gain).clamp(-32768.0, 32767.0) as i16;
            }
        }

        let playout_stream = playout.read().await.clone();
        playout_stream.push_pcm(&pcm);
        let waveform = build_mic_test_waveform(&pcm, 96);
        let _ = tx_event.send(UiEvent::MicTestWaveform(waveform));
    }
}

async fn voice_send_loop(
    egress: Arc<EgressScheduler>,
    mtu: usize,
    encoder: Arc<Mutex<audio::opus::OpusEncoder>>,
    capture: Arc<RwLock<Arc<audio::capture::Capture>>>,
    playout: Arc<RwLock<Arc<audio::playout::Playout>>>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    dsp_enabled: Arc<AtomicBool>,
    tx_event: Sender<UiEvent>,
    active_voice_channel_route: Arc<AtomicU32>,
    active_channel_audio_mode: Arc<std::sync::RwLock<ChannelAudioMode>>,
    ptt_active: Arc<AtomicBool>,
    capture_mode: Arc<AtomicU8>,
    self_muted: Arc<AtomicBool>,
    self_deafened: Arc<AtomicBool>,
    server_deafened: Arc<AtomicBool>,
    input_gain: Arc<std::sync::atomic::AtomicU32>,
    loopback_active: Arc<AtomicBool>,
    audio_runtime: AudioRuntimeSettings,
    voice_counters: Arc<VoiceTelemetryCounters>,
    network_telemetry: Arc<SharedNetworkTelemetry>,
    send_queue_drop_count: Arc<AtomicU32>,
    local_user_id: String,
    _voice_die_tx: watch::Sender<bool>,
) {
    let mut seq: u32 = 0;
    let ssrc: u32 = rand::random();

    let sample_rate = 48_000u32;
    let channels = 1usize;
    let frame_ms = 20u32;
    let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels;

    let mut pcm = vec![0i16; frame_samples];
    let mut enc_out = vec![0u8; 4000];

    let mut tick = tokio::time::interval(Duration::from_millis(frame_ms as u64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut vad_report_counter = 0u32;
    let mut stream_ts_ms = 0u32;
    let mut last_local_speaking = false;
    let mut last_oversize_warn = Instant::now();
    let voice_max_inbound = mtu.saturating_sub(vp_voice::FORWARDER_ADDED_HEADER_BYTES);
    let max_opus_payload_runtime =
        voice_max_inbound.saturating_sub(vp_voice::CLIENT_VOICE_HEADER_BYTES);
    let mut vad_hysteresis =
        audio::dsp::vad::VadHysteresis::from_timing(0.6, 0.45, 60, 300, frame_ms);
    let mut adaptation = OpusAdaptationController::default();
    {
        let init_bitrate = active_channel_audio_mode
            .read()
            .map(|m| m.bitrate_bps)
            .unwrap_or(64_000);
        if let Ok(mut enc) = encoder.try_lock() {
            let _ = apply_network_class_encoder_settings(&mut enc, NetworkClass::Good, init_bitrate);
        }
    }

    loop {
        tick.tick().await;

        loop {
            let capture_stream = capture.read().await.clone();
            if capture_stream.read_frame(&mut pcm) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // Reserve fixed headroom on the capture/send path before any user gain so
        // upstream boosts are less likely to clip before AGC/denoise processing.
        const SEND_PATH_PRE_ATTENUATION: f32 = 0.5; // -6 dB

        for s in pcm.iter_mut() {
            *s = (*s as f32 * SEND_PATH_PRE_ATTENUATION).round() as i16;
        }

        // Apply user-configured input gain
        let gain = u32_to_f32(input_gain.load(Ordering::Relaxed));
        if (gain - 1.0).abs() > 0.001 {
            for s in pcm.iter_mut() {
                *s = (*s as f32 * gain).clamp(-32768.0, 32767.0) as i16;
            }
        }

        // Loopback: feed capture directly to playout for mic testing
        if loopback_active.load(Ordering::Relaxed) {
            let playout_stream = playout.read().await.clone();
            playout_stream.push_pcm(&pcm);
            let waveform = build_mic_test_waveform(&pcm, 96);
            let _ = tx_event.send(UiEvent::MicTestWaveform(waveform));
        }

        let can_send = active_voice_channel_route.load(Ordering::Relaxed) != 0
            && !self_muted.load(Ordering::Relaxed)
            && !self_deafened.load(Ordering::Relaxed)
            && !server_deafened.load(Ordering::Relaxed)
            && (capture_mode_from_u8(capture_mode.load(Ordering::Relaxed))
                != ui::model::CaptureMode::PushToTalk
                || ptt_active.load(Ordering::Relaxed));
        if !can_send {
            if last_local_speaking {
                last_local_speaking = false;
                let _ = tx_event.send(UiEvent::VoiceActivity {
                    user_id: local_user_id.clone(),
                    speaking: false,
                });
            }
            let _ = tx_event.send(UiEvent::VoiceMeter {
                user_id: local_user_id.clone(),
                level: 0.0,
            });
            continue;
        }

        let sample = NetworkSample {
            rtt_ms: network_telemetry.rtt_ms.load(Ordering::Relaxed),
            loss_rate: network_telemetry.loss_ppm.load(Ordering::Relaxed) as f32 / 1_000_000.0,
            jitter_ms: network_telemetry.jitter_ms.load(Ordering::Relaxed),
            jitter_buffer_depth: voice_counters.jitter_buffer_depth.load(Ordering::Relaxed) as u32,
        };
        let channel_mode = active_channel_audio_mode
            .read()
            .map(|mode| *mode)
            .unwrap_or_default();
        if let Some(new_class) = adaptation.update(sample) {
            let mut enc = encoder.lock().await;
            if let Err(e) = apply_network_class_encoder_settings(&mut enc, new_class, channel_mode.bitrate_bps) {
                warn!("[audio] failed to apply network-class opus settings: {e:#}");
            }
        }
        let music_channel = is_music_channel(channel_mode);

        // Apply DSP pipeline (noise suppression + AGC + VAD)
        let mut vad_score = 1.0_f32;
        if dsp_enabled.load(Ordering::Relaxed) {
            if let Some(ref dsp) = capture_dsp {
                let mut d = dsp.lock().await;
                vad_score = d.process_frame(&mut pcm);

                // Report VAD level to GUI periodically
                vad_report_counter += 1;
                if vad_report_counter % 10 == 0 {
                    let _ = tx_event.send(UiEvent::VadLevel(d.last_vad_probability()));
                }
            }
        } else {
            vad_score = audio::pcm_peak_level(&pcm);
        }

        let processed_level = audio::pcm_peak_level(&pcm);
        let _ = tx_event.send(UiEvent::VoiceMeter {
            user_id: local_user_id.clone(),
            level: processed_level,
        });

        let gated_on = match capture_mode_from_u8(capture_mode.load(Ordering::Relaxed)) {
            ui::model::CaptureMode::PushToTalk => ptt_active.load(Ordering::Relaxed),
            ui::model::CaptureMode::Continuous => true,
            ui::model::CaptureMode::VoiceActivation => {
                if music_channel {
                    true
                } else {
                    vad_hysteresis.update(vad_score)
                }
            }
        };

        if gated_on != last_local_speaking {
            if gated_on {
                debug!("[audio] vad gate ON (score={:.2})", vad_score);
            } else {
                debug!("[audio] vad gate OFF (hangover elapsed)");
            }
        }

        if !gated_on {
            let mut attenuation_db =
                u32_to_f32(audio_runtime.denoise_attenuation_db.load(Ordering::Relaxed));
            if audio_runtime.typing_attenuation.load(Ordering::Relaxed) {
                attenuation_db = attenuation_db.min(-18.0);
            }
            let attn = 10.0_f32.powf((attenuation_db.min(0.0)) / 20.0);
            if attn < 0.999 {
                for s in pcm.iter_mut() {
                    *s = (*s as f32 * attn).clamp(-32768.0, 32767.0) as i16;
                }
            }
        }

        let speaking_now = gated_on;
        if speaking_now != last_local_speaking {
            last_local_speaking = speaking_now;
            let _ = tx_event.send(UiEvent::VoiceActivity {
                user_id: local_user_id.clone(),
                speaking: speaking_now,
            });
        }

        if !speaking_now {
            continue;
        }

        let n = match encoder.lock().await.encode(&pcm, &mut enc_out) {
            Ok(n) => n,
            Err(_) => continue,
        };

        if n > max_opus_payload_runtime {
            voice_counters
                .tx_oversized_payload_drops
                .fetch_add(1, Ordering::Relaxed);
            if last_oversize_warn.elapsed() >= Duration::from_secs(5) {
                last_oversize_warn = Instant::now();
                let _ = tx_event.send(UiEvent::AppendLog(format!(
                    "[voice] dropping oversized opus payload: {} > {} bytes",
                    n, max_opus_payload_runtime
                )));
            }
            continue;
        }

        let d = make_voice_datagram(
            active_voice_channel_route.load(Ordering::Relaxed),
            ssrc,
            seq,
            stream_ts_ms,
            gated_on,
            &enc_out[..n],
        );
        seq = seq.wrapping_add(1);
        stream_ts_ms = stream_ts_ms.wrapping_add(frame_ms);

        debug_assert!(d.len() <= voice_max_inbound);

        voice_counters.tx_packets.fetch_add(1, Ordering::Relaxed);
        voice_counters
            .tx_bytes
            .fetch_add(d.len() as u64, Ordering::Relaxed);

        match egress.enqueue_voice(d) {
            Ok(report) => {
                if let Some(dropped) = report.dropped {
                    send_queue_drop_count.fetch_add(dropped.count, Ordering::Relaxed);
                }
            }
            Err(reason) => {
                send_queue_drop_count.fetch_add(1, Ordering::Relaxed);
                let _ = tx_event.send(UiEvent::AppendLog(format!(
                    "[voice] egress enqueue rejected: {:?}",
                    reason
                )));
            }
        }
    }
}

async fn voice_recv_loop(
    voice_ingress_q: Arc<OverwriteQueue<StampedBytes>>,
    playout: Arc<RwLock<Arc<audio::playout::Playout>>>,
    capture_dsp: Option<Arc<Mutex<audio::dsp::CaptureDsp>>>,
    local_user_id: String,
    self_deafened: Arc<AtomicBool>,
    server_deafened: Arc<AtomicBool>,
    output_gain: Arc<std::sync::atomic::AtomicU32>,
    per_user_audio: Arc<std::sync::RwLock<HashMap<String, PerUserAudioSettings>>>,
    audio_runtime: AudioRuntimeSettings,
    tx_event: Sender<UiEvent>,
    voice_counters: Arc<VoiceTelemetryCounters>,
    voice_stale_drops_total: Arc<AtomicU64>,
    voice_drain_drops_total: Arc<AtomicU64>,
    voice_die_tx: watch::Sender<bool>,
) {
    const SPEAKING_HANGOVER_MS: u64 = 350;
    const STREAM_IDLE_DROP_MS: u64 = 10_000;
    const PLC_MAX_FRAMES: usize = 5;
    const PLC_TO_NOISE_CROSSFADE_FRAMES: usize = 3;
    const RECOVERY_FADE_IN_FRAMES: usize = 2;
    let sample_rate = 48_000u32;
    let channels = 1usize;
    let frame_ms = 20u32;
    let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels;

    let mut streams = HashMap::<StreamKey, InboundStreamState>::new();
    let mut tick = tokio::time::interval(Duration::from_millis(frame_ms as u64));
    // Prevent long scheduler pauses from triggering a catch-up burst of immediate
    // ticks, which can drain the jitter buffer and inflate apparent packet loss.
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut mix_out = vec![0f32; frame_samples];
    let mut mixed_pcm = vec![0i16; frame_samples];
    let mut last_logged_fec_mode = None::<FecMode>;

    loop {
        tokio::select! {
            maybe_d = pop_voice_realtime(
                &voice_ingress_q,
                VOICE_MAX_AGE,
                VOICE_DRAIN_KEEP_LATEST,
                VOICE_MAX_AGE / 2,
                &voice_stale_drops_total,
                &voice_drain_drops_total,
            ) => {
                let d = match maybe_d {
                    Some(d) => d,
                    None => {
                        let _ = voice_die_tx.send(true);
                        return;
                    }
                };

                if self_deafened.load(Ordering::Relaxed) || server_deafened.load(Ordering::Relaxed) {
                    continue;
                }

                let packet = match parse_voice_payload(&d) {
                    Some(v) => v,
                    None => continue,
                };

                voice_counters.rx_packets.fetch_add(1, Ordering::Relaxed);
                voice_counters.rx_bytes.fetch_add(d.len() as u64, Ordering::Relaxed);

                let now_ms = unix_ms();
                let stream = streams
                    .entry(packet.stream_key())
                    .or_insert_with(|| InboundStreamState::new(sample_rate, channels as u8, 64));
                if stream.last_packet_ts_ms != 0 {
                    let gap = packet.ts_ms.wrapping_sub(stream.last_packet_ts_ms);
                    if gap > 10_000 {
                        stream.jitter.set_expected(packet.seq);
                    }
                    if packet.seq < stream.jitter.expected_seq() {
                        voice_counters.late_packets.fetch_add(1, Ordering::Relaxed);
                    }
                }
                stream.last_packet_ts_ms = packet.ts_ms;
                stream.last_packet_wall_ms = now_ms;
                if let Some(user_id) = packet.sender_user_id {
                    stream.user_id = Some(user_id.to_string());
                }
                stream.jitter.push(packet.seq, packet.payload.to_vec());
                stream.missing_wait.observe_packet(now_ms, packet.ts_ms, frame_ms);
            }
            _ = tick.tick() => {
                if self_deafened.load(Ordering::Relaxed) || server_deafened.load(Ordering::Relaxed) {
                    continue;
                }

                let now_ms = unix_ms();
                mix_out.fill(0.0);
                let mut mixed_streams = 0usize;
                let fec_mode = match audio_runtime.fec_mode.load(Ordering::Relaxed) {
                    0 => FecMode::Off,
                    2 => FecMode::On,
                    _ => FecMode::Auto,
                };
                if last_logged_fec_mode != Some(fec_mode) {
                    info!("[audio] set fec receiver_mode={fec_mode:?}");
                    last_logged_fec_mode = Some(fec_mode);
                }
                let opus_use_inband_fec = fec_mode != FecMode::Off;

                let mut jitter_depth_max = 0u64;
                for stream in streams.values_mut() {
                    let mut frame_present = false;
                    jitter_depth_max = jitter_depth_max.max(stream.jitter.depth() as u64);
                    let mut frame_level = 0.0_f32;

                    let ready = stream
                        .jitter
                        .pop_ready(now_ms, stream.missing_wait.missing_wait_ms());

                    match ready {
                        audio::jitter::PopResult::Frame(frame) => {
                            let n = match stream.decoder.decode(&frame, &mut stream.pcm_out) {
                                Ok(n) => n,
                                Err(_) => 0,
                            };
                            if n > 0 {
                                frame_present = true;
                                stream.plc_frames = 0;
                                stream.consecutive_misses = 0;
                                if stream.in_comfort_noise {
                                    stream.recovery_fade_in_remaining = RECOVERY_FADE_IN_FRAMES;
                                    stream.in_comfort_noise = false;
                                }
                                let recovery_gain = stream.take_recovery_gain(RECOVERY_FADE_IN_FRAMES);
                                for (acc, sample) in mix_out[..n].iter_mut().zip(stream.pcm_out[..n].iter()) {
                                    let scaled = *sample as f32
                                        * recovery_gain
                                        * stream.effective_gain(&per_user_audio);
                                    frame_level = frame_level.max((scaled.abs() / 32768.0).min(1.0));
                                    *acc += scaled;
                                }
                                mixed_streams += 1;
                            }
                        }
                        audio::jitter::PopResult::Missing
                            if stream.last_packet_wall_ms != 0 && stream.plc_frames < PLC_MAX_FRAMES =>
                        {
                            voice_counters.lost_packets.fetch_add(1, Ordering::Relaxed);
                            stream.consecutive_misses += 1;
                            let n = stream.render_concealment_frame(
                                opus_use_inband_fec,
                                audio_runtime.comfort_noise.load(Ordering::Relaxed),
                                u32_to_f32(audio_runtime.comfort_noise_level.load(Ordering::Relaxed)),
                                PLC_MAX_FRAMES,
                                PLC_TO_NOISE_CROSSFADE_FRAMES,
                            );
                            if n > 0 {
                                stream.plc_frames += 1;
                                voice_counters.concealment_frames.fetch_add(1, Ordering::Relaxed);
                                frame_present = true;
                                for (acc, sample) in mix_out[..n].iter_mut().zip(stream.pcm_out[..n].iter()) {
                                    let scaled = *sample as f32 * stream.effective_gain(&per_user_audio);
                                    frame_level = frame_level.max((scaled.abs() / 32768.0).min(1.0));
                                    *acc += scaled;
                                }
                                mixed_streams += 1;
                            }
                        }
                        audio::jitter::PopResult::Waiting
                            if stream.plc_frames < PLC_MAX_FRAMES && stream.last_packet_wall_ms != 0 =>
                        {
                            let since_packet = now_ms.saturating_sub(stream.last_packet_wall_ms);
                            if since_packet <= (PLC_MAX_FRAMES as u64 * frame_ms as u64) {
                                stream.consecutive_misses += 1;
                                let n = stream.render_concealment_frame(
                                    false,
                                    audio_runtime.comfort_noise.load(Ordering::Relaxed),
                                    u32_to_f32(audio_runtime.comfort_noise_level.load(Ordering::Relaxed)),
                                    PLC_MAX_FRAMES,
                                    PLC_TO_NOISE_CROSSFADE_FRAMES,
                                );
                                if n > 0 {
                                    stream.plc_frames += 1;
                                    voice_counters.concealment_frames.fetch_add(1, Ordering::Relaxed);
                                    frame_present = true;
                                    for (acc, sample) in mix_out[..n].iter_mut().zip(stream.pcm_out[..n].iter()) {
                                        let scaled = *sample as f32 * stream.effective_gain(&per_user_audio);
                                        frame_level = frame_level.max((scaled.abs() / 32768.0).min(1.0));
                                        *acc += scaled;
                                    }
                                    mixed_streams += 1;
                                }
                            }
                        }
                        _ => {}
                    }

                    if frame_present {
                        stream.last_voice_frame_wall_ms = now_ms;
                    }

                    let speaking_now =
                        now_ms.saturating_sub(stream.last_voice_frame_wall_ms) <= SPEAKING_HANGOVER_MS;
                    stream.speaking = speaking_now;
                    if speaking_now != stream.last_emitted_speaking {
                        stream.last_emitted_speaking = speaking_now;
                        if let Some(user_id) = stream.user_id.as_ref() {
                            if user_id != &local_user_id {
                                let _ = tx_event.send(UiEvent::VoiceActivity {
                                    user_id: user_id.clone(),
                                    speaking: speaking_now,
                                });
                            }
                        }
                    }

                    stream.level = if speaking_now { frame_level.max(stream.level * 0.75) } else { 0.0 };
                    voice_counters.observe_peak_stream_level(stream.level);
                    if let Some(user_id) = stream.user_id.as_ref() {
                        if user_id != &local_user_id {
                            let _ = tx_event.send(UiEvent::VoiceMeter {
                                user_id: user_id.clone(),
                                level: stream.level,
                            });
                        }
                    }
                }

                streams.retain(|_, stream| {
                    let idle = now_ms.saturating_sub(stream.last_packet_wall_ms);
                    if idle >= STREAM_IDLE_DROP_MS {
                        if stream.last_emitted_speaking {
                            if let Some(user_id) = stream.user_id.as_ref() {
                                if user_id != &local_user_id {
                                    let _ = tx_event.send(UiEvent::VoiceActivity {
                                        user_id: user_id.clone(),
                                        speaking: false,
                                    });
                                }
                            }
                        }
                        return false;
                    }
                    true
                });

                voice_counters
                    .jitter_buffer_depth
                    .store(jitter_depth_max, Ordering::Relaxed);
                let playout_delay_ms = jitter_depth_max
                    .saturating_mul(frame_ms.into());
                voice_counters
                    .playout_delay_ms
                    .store(playout_delay_ms as u32, Ordering::Relaxed);

                let speaking_streams = streams.values().filter(|s| s.speaking).count();
                mixed_pcm.fill(0);

                if mixed_streams > 0 {
                    for (dst, sample) in mixed_pcm.iter_mut().zip(mix_out.iter()) {
                        let x = *sample / 32768.0;
                        let soft = (x / (1.0 + x.abs())).clamp(-1.0, 1.0);
                        *dst = (soft * 32768.0) as i16;
                    }
                    if audio_runtime.mono_expansion.load(Ordering::Relaxed) {
                        let mut prev = 0.0_f32;
                        for s in mixed_pcm.iter_mut() {
                            let dry = *s as f32;
                            let widened = (dry + 0.2 * (dry - prev)).clamp(-32768.0, 32767.0);
                            *s = widened as i16;
                            prev = dry;
                        }
                    }
                }

                let mut output_mul = u32_to_f32(output_gain.load(Ordering::Relaxed));

                if audio_runtime.output_auto_level.load(Ordering::Relaxed) && mixed_streams > 0 {
                    let peak = mixed_pcm
                        .iter()
                        .map(|s| (*s as i32).unsigned_abs() as f32 / 32768.0)
                        .fold(0.0_f32, f32::max);
                    if peak > 0.001 {
                        let target_peak = 0.8_f32;
                        let norm = (target_peak / peak).clamp(0.5, 2.0);
                        output_mul *= norm;
                    }
                }

                if audio_runtime.ducking_enabled.load(Ordering::Relaxed) && speaking_streams > 0 {
                    let duck_db = u32_to_f32(
                        audio_runtime
                            .ducking_attenuation_db
                            .load(Ordering::Relaxed),
                    )
                    .min(0.0);
                    output_mul *= 10.0_f32.powf(duck_db / 20.0);
                }

                if mixed_streams == 0 && audio_runtime.comfort_noise.load(Ordering::Relaxed) {
                    let noise = u32_to_f32(audio_runtime.comfort_noise_level.load(Ordering::Relaxed))
                        .clamp(0.0, 0.1);
                    if noise > 0.0 {
                        for s in mixed_pcm.iter_mut() {
                            let n = (rand::random::<f32>() * 2.0 - 1.0) * noise * 32767.0;
                            *s = n as i16;
                        }
                    }
                }

                if mixed_streams == 0 && !audio_runtime.comfort_noise.load(Ordering::Relaxed) {
                    continue;
                }

                if (output_mul - 1.0).abs() > 0.001 {
                    for s in mixed_pcm.iter_mut() {
                        *s = (*s as f32 * output_mul).clamp(-32768.0, 32767.0) as i16;
                    }
                }

                if let Some(ref dsp) = capture_dsp {
                    let mut d = dsp.lock().await;
                    d.feed_echo_reference(&mixed_pcm);
                }

                let playout_stream = playout.read().await.clone();
                playout_stream.push_pcm(&mixed_pcm);
            }
        }
    }
}

struct InboundStreamState {
    jitter: audio::jitter::JitterBuffer,
    decoder: audio::opus::OpusDecoder,
    pcm_out: Vec<i16>,
    user_id: Option<String>,
    level: f32,
    last_packet_ts_ms: u32,
    last_packet_wall_ms: u64,
    last_voice_frame_wall_ms: u64,
    plc_frames: usize,
    consecutive_misses: usize,
    in_comfort_noise: bool,
    recovery_fade_in_remaining: usize,
    noise_rng_state: u32,
    missing_wait: MissingWaitController,
    speaking: bool,
    last_emitted_speaking: bool,
}

impl InboundStreamState {
    fn new(sample_rate: u32, channels: u8, max_frames: usize) -> Self {
        let channel_count = channels as usize;
        let frame_samples = (sample_rate as usize * 20 / 1000) * channel_count;
        Self {
            jitter: audio::jitter::JitterBuffer::new(max_frames),
            decoder: audio::opus::OpusDecoder::new(sample_rate, channels)
                .expect("inbound opus decoder init"),
            pcm_out: vec![0i16; frame_samples],
            user_id: None,
            level: 0.0,
            last_packet_ts_ms: 0,
            last_packet_wall_ms: 0,
            last_voice_frame_wall_ms: 0,
            plc_frames: 0,
            consecutive_misses: 0,
            in_comfort_noise: false,
            recovery_fade_in_remaining: 0,
            noise_rng_state: 0xA5A5_1F3Du32,
            missing_wait: MissingWaitController::new(),
            speaking: false,
            last_emitted_speaking: false,
        }
    }

    fn take_recovery_gain(&mut self, fade_frames: usize) -> f32 {
        if self.recovery_fade_in_remaining == 0 || fade_frames == 0 {
            return 1.0;
        }
        let completed = fade_frames.saturating_sub(self.recovery_fade_in_remaining);
        self.recovery_fade_in_remaining = self.recovery_fade_in_remaining.saturating_sub(1);
        ((completed + 1) as f32 / fade_frames as f32).clamp(0.0, 1.0)
    }

    fn render_concealment_frame(
        &mut self,
        use_fec: bool,
        comfort_noise_enabled: bool,
        comfort_noise_level: f32,
        plc_max_frames: usize,
        crossfade_frames: usize,
    ) -> usize {
        if self.consecutive_misses <= plc_max_frames {
            self.in_comfort_noise = false;
            return if use_fec {
                match self.jitter.peek_expected() {
                    Some(next_frame) => self
                        .decoder
                        .decode_fec(next_frame, &mut self.pcm_out)
                        .or_else(|_| self.decoder.decode_plc(&mut self.pcm_out))
                        .unwrap_or(0),
                    None => self.decoder.decode_plc(&mut self.pcm_out).unwrap_or(0),
                }
            } else {
                self.decoder.decode_plc(&mut self.pcm_out).unwrap_or(0)
            };
        }

        let noise_level = if comfort_noise_enabled {
            comfort_noise_level.clamp(0.0, 0.1)
        } else {
            0.0
        };
        if noise_level <= 0.0 {
            return 0;
        }

        let transition_idx = self.consecutive_misses.saturating_sub(plc_max_frames + 1);
        let crossfade_pos = transition_idx.min(crossfade_frames);
        let noise_gain = if crossfade_frames == 0 {
            1.0
        } else {
            (crossfade_pos as f32 / crossfade_frames as f32).clamp(0.0, 1.0)
        };
        let plc_gain = 1.0 - noise_gain;

        if plc_gain > 0.0 {
            let _ = self.decoder.decode_plc(&mut self.pcm_out).unwrap_or(0);
        } else {
            self.pcm_out.fill(0);
        }

        for sample in &mut self.pcm_out {
            self.noise_rng_state = self
                .noise_rng_state
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223);
            let centered = ((self.noise_rng_state >> 16) as f32 / 65_535.0) * 2.0 - 1.0;
            let plc = *sample as f32 * plc_gain;
            let noise = centered * 32_767.0 * noise_level * noise_gain;
            *sample = (plc + noise).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }

        self.in_comfort_noise = true;
        self.pcm_out.len()
    }
}

impl InboundStreamState {
    fn effective_gain(
        &self,
        per_user_audio: &std::sync::RwLock<HashMap<String, PerUserAudioSettings>>,
    ) -> f32 {
        let Some(user_id) = self.user_id.as_ref() else {
            return 1.0;
        };
        let Ok(per_user) = per_user_audio.read() else {
            return 1.0;
        };
        per_user
            .get(user_id)
            .map(|settings| {
                if settings.muted {
                    0.0
                } else {
                    settings.gain.clamp(0.0, 2.0)
                }
            })
            .unwrap_or(1.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum StreamKey {
    Sender(uuid::Uuid),
    Ssrc(u32),
}

struct InboundVoice<'a> {
    sender_user_id: Option<uuid::Uuid>,
    channel_id: Option<uuid::Uuid>,
    ssrc: u32,
    seq: u32,
    ts_ms: u32,
    payload: &'a [u8],
}

impl InboundVoice<'_> {
    fn stream_key(&self) -> StreamKey {
        self.sender_user_id
            .map(StreamKey::Sender)
            .unwrap_or(StreamKey::Ssrc(self.ssrc))
    }
}

fn parse_voice_payload(d: &Bytes) -> Option<InboundVoice<'_>> {
    if d.len() < VOICE_HDR_LEN {
        return None;
    }
    if d[0] != VOICE_VERSION {
        return None;
    }
    let hdr_len = u16::from_be_bytes([d[2], d[3]]) as usize;
    if d.len() <= hdr_len {
        return None;
    }
    let ssrc = u32::from_be_bytes([d[8], d[9], d[10], d[11]]);
    let seq = u32::from_be_bytes([d[12], d[13], d[14], d[15]]);
    let ts_ms = u32::from_be_bytes([d[16], d[17], d[18], d[19]]);

    match hdr_len {
        VOICE_HDR_LEN => Some(InboundVoice {
            sender_user_id: None,
            channel_id: None,
            ssrc,
            seq,
            ts_ms,
            payload: &d[hdr_len..],
        }),
        VOICE_FORWARDED_HDR_LEN => {
            let sender_user_id = uuid::Uuid::from_slice(&d[20..36]).ok();
            let channel_id = uuid::Uuid::from_slice(&d[36..52]).ok();
            Some(InboundVoice {
                sender_user_id,
                channel_id,
                ssrc,
                seq,
                ts_ms,
                payload: &d[hdr_len..],
            })
        }
        _ => None,
    }
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

    if cfg.ca_cert_pem.trim().is_empty() {
        return Err(anyhow!(
            "VP_CA_CERT_PEM (or --ca-cert-pem) is required in this build"
        ));
    }

    net::quic::make_ca_endpoint(&cfg.ca_cert_pem, &cfg.alpn)
}

fn make_pinned_endpoint(pin_sha256: [u8; 32], alpn: &str) -> Result<quinn::Endpoint> {
    use quinn::Endpoint;
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
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
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
    endpoint.set_default_client_config(net::quic::client_config_with_transport(crypto)?);
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

/// Load an image from disk, resize/crop it to exact target dimensions, encode to WebP,
/// then upload it to the server as a profile asset (avatar or banner).
/// Returns the verified asset_id.
///
/// For avatars (square): center-crops the source to a square, then resizes to `target_w×target_h`.
/// For banners (wide): uses cover-fill to resize, then center-crops to `target_w×target_h`.
/// This ensures the uploaded image always matches the expected dimensions exactly.
async fn upload_profile_image(
    conn: &quinn::Connection,
    dispatcher: &net::dispatcher::ControlDispatcher,
    path: &std::path::Path,
    purpose: &str,
    max_bytes: u64,
    target_w: u32,
    target_h: u32,
) -> anyhow::Result<String> {
    use anyhow::Context as _;

    // Read the file.
    let raw = tokio::fs::read(path)
        .await
        .with_context(|| format!("read image: {}", path.display()))?;

    if raw.len() as u64 > max_bytes {
        anyhow::bail!("image too large ({} bytes, limit {max_bytes})", raw.len());
    }

    // Validate extension (GIF not accepted).
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "gif" {
        anyhow::bail!("GIF is not accepted; please use PNG, JPEG, or WebP");
    }

    // Decode with the `image` crate for validation and resize.
    let img = image::load_from_memory(&raw).context("decode image")?;

    // Resize to exact target dimensions using cover-fill (crop excess).
    // This fills the target rect completely, then center-crops to exact size.
    let img = img.resize_to_fill(target_w, target_h, image::imageops::FilterType::Lanczos3);

    // Encode as WebP for superior compression.
    let mut webp_bytes: Vec<u8> = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut webp_bytes),
        image::ImageFormat::WebP,
    )
    .context("encode image to WebP")?;

    // Upload via the dispatcher (control stream begin + data stream).
    let asset_id = dispatcher
        .upload_profile_asset(conn, purpose, webp_bytes, "image/webp")
        .await
        .context("upload profile asset")?;

    Ok(asset_id)
}

#[cfg(test)]
mod tests {
    use super::{apply_authoritative_snapshot, choose_initial_selected_channel};
    use crate::{
        proto::voiceplatform::v1 as pb,
        ui::{model::ChannelType, UiEvent},
    };
    use crossbeam_channel::bounded;

    #[test]
    fn choose_initial_selected_channel_preserves_requested_when_present() {
        let requested = "channel-b";
        let snapshot = pb::InitialStateSnapshot {
            channels: vec![
                pb::ChannelSnapshot {
                    info: Some(pb::ChannelInfo {
                        channel_id: Some(pb::ChannelId {
                            value: "channel-a".into(),
                        }),
                        name: "General".into(),
                        ..Default::default()
                    }),
                },
                pb::ChannelSnapshot {
                    info: Some(pb::ChannelInfo {
                        channel_id: Some(pb::ChannelId {
                            value: requested.into(),
                        }),
                        name: "Gaming".into(),
                        ..Default::default()
                    }),
                },
            ],
            default_channel_id: Some(pb::ChannelId {
                value: "channel-a".into(),
            }),
            ..Default::default()
        };

        assert_eq!(
            choose_initial_selected_channel(&snapshot, Some(requested)),
            Some(requested.to_string())
        );
    }

    #[test]
    fn demux_predicate_routes_video_by_version_and_kind() {
        let video = bytes::Bytes::from_static(&[
            vp_voice::VIDEO_VERSION,
            vp_voice::DATAGRAM_KIND_VIDEO,
            1,
            2,
            3,
        ]);
        let voice = bytes::Bytes::from_static(&[
            vp_voice::VOICE_VERSION,
            vp_voice::DATAGRAM_KIND_VOICE,
            1,
            2,
            3,
        ]);
        assert!(super::is_video_datagram(&video));
        assert!(!super::is_video_datagram(&voice));
    }

    #[tokio::test]
    async fn subscribe_stream_adds_state_and_routes_by_stream_tag() {
        let state = super::SharedStreamState::new();
        let stream_tag = 4242u64;
        {
            let mut streams = state.active_streams.write().await;
            streams.insert(
                stream_tag,
                std::sync::Arc::new(tokio::sync::Mutex::new(
                    crate::net::video_transport::VideoReceiver::new(
                        4,
                        vp_voice::MAX_FRAGS_PER_FRAME,
                    ),
                )),
            );
        }

        let hdr = crate::net::video_datagram::VideoHeader {
            stream_tag,
            layer_id: 0,
            flags: vp_voice::VIDEO_FLAG_END_OF_FRAME,
            frame_seq: 7,
            frag_idx: 0,
            frag_total: 1,
            ts_ms: 101,
        };
        let dg = crate::net::video_datagram::make_video_datagram(&hdr, b"abc");

        let receiver = {
            let g = state.active_streams.read().await;
            g.get(&stream_tag).cloned()
        }
        .expect("receiver exists");

        let mut rx = receiver.lock().await;
        let frame = rx.receive(&dg).expect("frame routed to subscribed stream");
        assert_eq!(frame.stream_tag, stream_tag);
        assert_eq!(frame.frame_seq, 7);
    }

    #[test]
    fn apply_authoritative_snapshot_sets_channel_and_members_lists() {
        let snapshot = pb::InitialStateSnapshot {
            server_id: Some(pb::ServerId {
                value: "server-1".into(),
            }),
            self_user_id: Some(pb::UserId {
                value: "user-1".into(),
            }),
            channels: vec![pb::ChannelSnapshot {
                info: Some(pb::ChannelInfo {
                    channel_id: Some(pb::ChannelId {
                        value: "channel-a".into(),
                    }),
                    name: "General".into(),
                    ..Default::default()
                }),
            }],
            channel_members: vec![pb::ChannelMembersSnapshot {
                channel_id: Some(pb::ChannelId {
                    value: "channel-a".into(),
                }),
                members: vec![pb::ChannelMember {
                    user_id: Some(pb::UserId {
                        value: "user-2".into(),
                    }),
                    display_name: "Alice".into(),
                    ..Default::default()
                }],
            }],
            default_channel_id: Some(pb::ChannelId {
                value: "channel-a".into(),
            }),
            ..Default::default()
        };

        let (tx, rx) = bounded::<UiEvent>(16);
        apply_authoritative_snapshot(&snapshot, &tx, None);

        let events = rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|ev| matches!(
            ev,
            UiEvent::SetChannels(channels)
                if channels.len() == 1
                    && channels[0].id == "channel-a"
                    && channels[0].name == "General"
                    && matches!(channels[0].channel_type, ChannelType::Voice)
        )));
        assert!(events.iter().any(|ev| matches!(
            ev,
            UiEvent::UpdateChannelMembers { channel_id, members }
                if channel_id == "channel-a"
                    && members.len() == 1
                    && members[0].user_id == "user-2"
                    && members[0].display_name == "Alice"
        )));
    }
    #[test]
    fn voice_ingress_cap_guardrail() {
        // Do not increase without justification; latency risk.
        assert!(super::VOICE_INGRESS_CAP <= 64);
    }
}
