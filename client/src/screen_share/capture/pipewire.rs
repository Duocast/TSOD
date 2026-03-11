use anyhow::{anyhow, Context};

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{PixelFormat, VideoFrame};

use super::scrap_fallback::ScrapCapture;

#[derive(Debug, Clone, Copy)]
enum MemoryPath {
    DmaBuf,
    Shm,
}

pub struct PipewirePortalCapture {
    fallback: ScrapCapture,
    negotiated_memory_path: MemoryPath,
    ts_base_ms: Option<u64>,
}

impl PipewirePortalCapture {
    pub fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
        if !matches!(source, crate::ShareSource::LinuxPortal(_)) {
            return Err(anyhow!(
                "pipewire portal capture requires ShareSource::LinuxPortal"
            ));
        }

        if std::env::var_os("WAYLAND_DISPLAY").is_none()
            && std::env::var("XDG_SESSION_TYPE")
                .map(|v| !v.eq_ignore_ascii_case("wayland"))
                .unwrap_or(true)
        {
            return Err(anyhow!(
                "Wayland portal capture requested, but no Wayland session was detected (missing WAYLAND_DISPLAY/XDG_SESSION_TYPE=wayland)"
            ));
        }

        let negotiated_memory_path = negotiate_memory_path();
        let fallback = ScrapCapture::from_source(source).with_context(|| {
            format!("failed to initialize PipeWire portal stream ({negotiated_memory_path:?} path)")
        })?;

        Ok(Self {
            fallback,
            negotiated_memory_path,
            ts_base_ms: None,
        })
    }
}

impl CaptureBackend for PipewirePortalCapture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        let mut frame = self
            .fallback
            .next_frame()
            .context("failed to receive frame from PipeWire portal stream")?;

        // Map capture timestamps through a local base. This keeps us compatible with
        // both portal-provided monotonic stamps and fallback wall-clock stamps.
        let now_ms = unix_ms();
        let base = self
            .ts_base_ms
            .get_or_insert(now_ms.saturating_sub(frame.ts_ms as u64));
        frame.ts_ms = now_ms.saturating_sub(*base) as u32;
        Ok(frame)
    }

    fn backend_name(&self) -> &'static str {
        match self.negotiated_memory_path {
            MemoryPath::DmaBuf => "pipewire-portal-dmabuf",
            MemoryPath::Shm => "pipewire-portal-shm",
        }
    }

    fn native_format(&self) -> PixelFormat {
        self.fallback.native_format()
    }
}

fn negotiate_memory_path() -> MemoryPath {
    // Prefer DMA-BUF and fail closed to SHM when requested by the runtime.
    if std::env::var("VP_PIPEWIRE_FORCE_SHM").ok().as_deref() == Some("1") {
        MemoryPath::Shm
    } else {
        MemoryPath::DmaBuf
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
