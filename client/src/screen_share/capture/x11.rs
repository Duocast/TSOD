use anyhow::{anyhow, Context};

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{PixelFormat, VideoFrame};

use super::scrap_fallback::ScrapCapture;

pub struct X11Capture {
    fallback: ScrapCapture,
}

impl X11Capture {
    pub fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
        if !matches!(source, crate::ShareSource::X11Window(_)) {
            return Err(anyhow!(
                "x11 capture requires ShareSource::X11Window (dedicated X11 path)"
            ));
        }

        if std::env::var_os("DISPLAY").is_none() {
            return Err(anyhow!(
                "x11 capture requested but DISPLAY is not set; verify X11 session and permissions"
            ));
        }

        let fallback = ScrapCapture::from_source(source)
            .context("initialize dedicated X11 capture backend")?;
        Ok(Self { fallback })
    }
}

impl CaptureBackend for X11Capture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        self.fallback
            .next_frame()
            .context("failed to capture X11 frame")
    }

    fn backend_name(&self) -> &'static str {
        "x11"
    }

    fn native_format(&self) -> PixelFormat {
        PixelFormat::Bgra
    }
}
