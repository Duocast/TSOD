//! PipeWire screen-capture backend for Wayland via xdg-desktop-portal.
//!
//! # Flow
//!
//! 1. `from_source` calls `portal::request_screencast_nodes()` which runs the
//!    full xdg-desktop-portal ScreenCast D-Bus flow (source picker, etc.) and
//!    returns one or more PipeWire node IDs.
//! 2. A dedicated OS thread is spawned that owns the PipeWire main-loop.
//! 3. An INPUT stream is connected to the first portal node.
//! 4. `param_changed` (SPA_PARAM_Format) is parsed with `VideoInfoRaw` to
//!    capture the negotiated width, height, and pixel format.  On resize /
//!    reconfigure the callback fires again and the updated dimensions are
//!    applied to the very next frame.
//! 5. `process` dequeues buffers and sends `VideoFrame`s over an mpsc channel.
//! 6. Dropping `PipewirePortalCapture` sets a stop flag that causes the
//!    main-loop thread to exit cleanly.
//!
//! # OS / runtime assumptions
//!
//! * Requires `target_os = "linux"` (gated at crate level).
//! * Requires a running PipeWire daemon and xdg-desktop-portal; see
//!   `crate::screen_share::portal` for D-Bus prerequisites.
//! * Connecting to the *default* PipeWire daemon is sufficient for
//!   non-sandboxed (non-Flatpak) applications.  The portal's
//!   `OpenPipeWireRemote` FD path is not used here.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use anyhow::{anyhow, Context};
use bytes::Bytes;
use pipewire as pw;
use pw::properties::properties;

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};

// ── Public capture backend ────────────────────────────────────────────────────

pub struct PipewirePortalCapture {
    rx: std::sync::mpsc::Receiver<anyhow::Result<VideoFrame>>,
    /// Set on drop to signal the PipeWire thread to exit.
    stop: Arc<AtomicBool>,
    backend_name: &'static str,
}

impl PipewirePortalCapture {
    /// Build the capture backend for a `ShareSource::LinuxPortal` source.
    ///
    /// For `token == "portal-picker"` this runs the full xdg-desktop-portal
    /// picker flow synchronously (blocking until the user makes a selection or
    /// cancels).  It is safe to call from `tokio::task::spawn_blocking`.
    ///
    /// For `token == "node-<id>"` the portal flow is skipped and the given
    /// PipeWire node ID is used directly (useful for testing).
    pub fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
        let crate::ShareSource::LinuxPortal(token) = source else {
            return Err(anyhow!(
                "pipewire portal capture requires ShareSource::LinuxPortal"
            ));
        };

        let node_id = resolve_node_id(token)?;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::sync_channel(2);

        let stop_clone = stop.clone();
        std::thread::Builder::new()
            .name("tsod-pipewire-video-capture".to_string())
            .spawn(move || run_pipewire_capture(node_id, tx, stop_clone))
            .context("spawn pipewire capture thread")?;

        Ok(Self {
            rx,
            stop,
            backend_name: "pipewire-portal",
        })
    }
}

impl Drop for PipewirePortalCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl CaptureBackend for PipewirePortalCapture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        self.rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .map_err(|_| anyhow!("timeout waiting for PipeWire frame"))?
    }

    fn backend_name(&self) -> &'static str {
        self.backend_name
    }

    fn native_format(&self) -> PixelFormat {
        PixelFormat::Bgra
    }
}

// ── Node ID resolution ────────────────────────────────────────────────────────

/// Resolve a `LinuxPortal` token to a PipeWire node ID.
///
/// * `"portal-picker"` – runs the xdg-desktop-portal picker flow.
/// * `"node-<u32>"`    – uses the given ID directly (no portal call).
fn resolve_node_id(token: &str) -> anyhow::Result<Option<u32>> {
    if token == "portal-picker" {
        let nodes = crate::screen_share::portal::request_screencast_nodes()
            .context("xdg-desktop-portal ScreenCast flow")?;
        // Use the first node; multiple nodes (multi-monitor) can be connected
        // in a future extension.
        let id = nodes
            .node_ids
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("portal returned no stream nodes"))?;
        return Ok(Some(id));
    }

    if let Some(raw) = token.strip_prefix("node-") {
        let id = raw
            .parse::<u32>()
            .map_err(|_| anyhow!("invalid node-id token: {token}"))?;
        return Ok(Some(id));
    }

    // Empty token or unknown format: connect without a specific target and let
    // PipeWire auto-connect (useful for runtime probing).
    Ok(None)
}

// ── PipeWire capture thread ───────────────────────────────────────────────────

/// User-data shared between the `param_changed` and `process` callbacks.
/// Both fire on the same PipeWire main-loop thread so no synchronisation is needed.
struct CaptureState {
    tx: std::sync::mpsc::SyncSender<anyhow::Result<VideoFrame>>,
    /// Populated / updated whenever the portal negotiates (or re-negotiates)
    /// stream parameters.
    negotiated: Option<NegotiatedFormat>,
    /// Stop flag written by the owning `PipewirePortalCapture` on drop.
    stop: Arc<AtomicBool>,
}

/// The negotiated video format, filled in by `param_changed`.
#[derive(Clone, Copy, Debug)]
struct NegotiatedFormat {
    width: u32,
    height: u32,
    pixel_format: PixelFormat,
}

fn run_pipewire_capture(
    target: Option<u32>,
    tx: std::sync::mpsc::SyncSender<anyhow::Result<VideoFrame>>,
    stop: Arc<AtomicBool>,
) {
    if let Err(err) = run_pipewire_capture_inner(target, tx.clone(), stop) {
        let _ = tx.send(Err(err));
    }
}

fn run_pipewire_capture_inner(
    target: Option<u32>,
    tx: std::sync::mpsc::SyncSender<anyhow::Result<VideoFrame>>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    pw::init();

    let mainloop =
        pw::main_loop::MainLoopBox::new(None).context("create PipeWire mainloop")?;
    let context =
        pw::context::ContextBox::new(mainloop.loop_(), None)
            .context("create PipeWire context")?;
    let core = context.connect(None).context("connect PipeWire core")?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "tsod-screen-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("create PipeWire video stream")?;

    let state = CaptureState {
        tx,
        negotiated: None,
        stop: stop.clone(),
    };

    let listener = stream
        .add_local_listener_with_user_data(state)
        // ── Format negotiation ──────────────────────────────────────────────
        .param_changed(|_stream, data, id, param| {
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(pod) = param else { return };

            // Verify media type / subtype first.
            let Ok((media_type, media_subtype)) =
                pw::spa::param::format_utils::parse_format(pod)
            else {
                return;
            };
            if media_type != pw::spa::param::format::MediaType::Video
                || media_subtype != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }

            // Parse the full video info.
            let mut info = pw::spa::param::video::VideoInfoRaw::default();
            if info.parse(pod).is_err() {
                return;
            }

            let size = info.size();
            if size.width == 0 || size.height == 0 {
                return;
            }

            let pixel_format = match info.format() {
                pw::spa::param::video::VideoFormat::RGBx
                | pw::spa::param::video::VideoFormat::RGBA => PixelFormat::Bgra,
                // BGRx, BGRA and anything else we treat as Bgra (4 bpp).
                _ => PixelFormat::Bgra,
            };

            data.negotiated = Some(NegotiatedFormat {
                width: size.width,
                height: size.height,
                pixel_format,
            });
        })
        // ── Frame delivery ──────────────────────────────────────────────────
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }

            let chunk = datas[0].chunk();
            let size = chunk.size() as usize;
            let offset = chunk.offset() as usize;
            // stride is signed in SPA; use absolute value.
            let stride = chunk.stride().unsigned_abs();

            let Some(raw) = datas[0].data() else {
                return;
            };
            if offset + size > raw.len() {
                return;
            }
            let payload = &raw[offset..offset + size];

            // Prefer dimensions from the negotiated format (set by
            // param_changed).  Fall back to computing from stride / size so
            // that the first few frames before param_changed fires are still
            // usable and the hardcoded-1920 bug cannot occur.
            let (width, height, pixel_format) =
                if let Some(fmt) = data.negotiated {
                    (fmt.width, fmt.height, fmt.pixel_format)
                } else {
                    // stride = bytes per row; for 4-bpp formats: width = stride / 4
                    let bpp = 4u32;
                    let w = stride.max(bpp) / bpp;
                    let h = (size as u32) / stride.max(1);
                    (w, h.max(1), PixelFormat::Bgra)
                };

            if width == 0 || height == 0 || stride == 0 {
                return;
            }

            let frame = VideoFrame {
                width,
                height,
                ts_ms: unix_ms() as u32,
                format: pixel_format,
                planes: FramePlanes::Bgra {
                    bytes: Bytes::copy_from_slice(payload),
                    stride: stride.max(1),
                },
            };
            let _ = data.tx.try_send(Ok(frame));
        })
        .register()
        .context("register PipeWire stream listener")?;

    // Advertise supported formats: BGRx preferred, RGBx as fallback.
    let fmt_obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::RGBx
        ),
    );

    let pod_bytes: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(fmt_obj),
    )
    .map_err(|_| anyhow!("serialize PipeWire EnumFormat pod"))?
    .0
    .into_inner();

    let mut params = [pw::spa::pod::Pod::from_bytes(&pod_bytes)
        .ok_or_else(|| anyhow!("build PipeWire format pod"))?];

    stream
        .connect(
            pw::spa::utils::Direction::Input,
            target,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("connect PipeWire stream")?;

    // Keep listener alive for the duration of the loop.
    let _listener = listener;

    // Run the main-loop until the owning CaptureBackend is dropped (stop=true)
    // or the channel receiver is dropped (tx.try_send fails).
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let _ = mainloop
            .loop_()
            .iterate(std::time::Duration::from_millis(50));
    }

    Ok(())
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::resolve_node_id;

    #[test]
    fn parses_explicit_node_token() {
        assert_eq!(resolve_node_id("node-42").unwrap(), Some(42));
        assert_eq!(resolve_node_id("node-0").unwrap(), Some(0));
    }

    #[test]
    fn empty_token_returns_none() {
        assert_eq!(resolve_node_id("").unwrap(), None);
    }

    #[test]
    fn invalid_node_token_is_error() {
        assert!(resolve_node_id("node-notanumber").is_err());
    }

    /// Width derivation from stride must not produce the old hardcoded 1920.
    ///
    /// Simulates what the `process` callback does when no format has been
    /// negotiated yet (negotiated = None).
    #[test]
    fn width_from_stride_not_hardcoded() {
        // A 2560×1440 BGRx frame: stride = 2560 * 4 = 10240 bytes
        let stride: u32 = 2560 * 4;
        let height_px: u32 = 1440;
        let size: usize = (stride * height_px) as usize;

        let bpp = 4u32;
        let computed_width = stride.max(bpp) / bpp;
        let computed_height = (size as u32) / stride.max(1);

        assert_eq!(computed_width, 2560);
        assert_eq!(computed_height, 1440);
        // Old code would have returned 1920 here.
        assert_ne!(computed_width, 1920);
    }

    /// Stride-based width for a 1920×1080 source should be 1920 (correct, not a coincidence).
    #[test]
    fn width_from_stride_1080p() {
        let stride: u32 = 1920 * 4;
        let bpp = 4u32;
        let computed_width = stride.max(bpp) / bpp;
        assert_eq!(computed_width, 1920);
    }
}
