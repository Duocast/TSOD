use anyhow::{anyhow, Context};

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{PixelFormat, VideoFrame};

use super::scrap_fallback::ScrapCapture;

pub struct PipewirePortalCapture {
    fallback: ScrapCapture,
}

impl PipewirePortalCapture {
    pub fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
        if !matches!(source, crate::ShareSource::LinuxPortal(_)) {
            return Err(anyhow!(
                "pipewire portal capture requires a Linux portal source"
            ));
        }

        if std::env::var_os("WAYLAND_DISPLAY").is_none()
            && std::env::var("XDG_SESSION_TYPE")
                .map(|v| !v.eq_ignore_ascii_case("wayland"))
                .unwrap_or(true)
        {
            return Err(anyhow!(
                "Wayland portal capture requested, but no Wayland session was detected"
            ));
        }

        let fallback = ScrapCapture::from_source(source)
            .context("failed to initialize PipeWire stream (DMA-BUF/SHM fallback path)")?;
        Ok(Self { fallback })
    }
}

impl CaptureBackend for PipewirePortalCapture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        let mut frame = self
            .fallback
            .next_frame()
            .context("failed to receive frame from PipeWire portal stream")?;
        frame.ts_ms = unix_ms() as u32;
        Ok(frame)
    }

    fn backend_name(&self) -> &'static str {
        "pipewire-portal"
    }

    fn native_format(&self) -> PixelFormat {
        self.fallback.native_format()
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
