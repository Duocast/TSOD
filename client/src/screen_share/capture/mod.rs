use anyhow::anyhow;

use crate::media_capture::CaptureBackend;
use crate::screen_share::runtime_probe::{CaptureBackendKind, MediaRuntimeCaps};

pub mod dxgi;
pub mod pipewire;
pub mod scrap_fallback;
pub mod x11;

#[cfg(feature = "dev-synthetic-stream")]
use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};
#[cfg(feature = "dev-synthetic-stream")]
use bytes::Bytes;

pub fn build_capture_backend(
    source: &crate::ShareSource,
    caps: &MediaRuntimeCaps,
) -> anyhow::Result<Box<dyn CaptureBackend>> {
    #[cfg(feature = "dev-synthetic-stream")]
    if std::env::var("VP_USE_SYNTHETIC_SCREEN_CAPTURE")
        .ok()
        .as_deref()
        == Some("1")
    {
        return Ok(Box::new(SyntheticCapture::new()));
    }

    for backend in &caps.capture_backends {
        let attempt: anyhow::Result<Box<dyn CaptureBackend>> = match backend {
            CaptureBackendKind::Dxgi => Ok(Box::new(dxgi::DxgiCapture::from_source(source)?)),
            CaptureBackendKind::PipewirePortal => Ok(Box::new(
                pipewire::PipewirePortalCapture::from_source(source)?,
            )),
            CaptureBackendKind::X11 => Ok(Box::new(x11::X11Capture::from_source(source)?)),
            CaptureBackendKind::Scrap => {
                Ok(Box::new(scrap_fallback::ScrapCapture::from_source(source)?))
            }
        };

        if let Ok(capture) = attempt {
            return Ok(capture);
        }
    }

    Err(anyhow!(
        "no screen capture backend could be initialized for source={source:?}"
    ))
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
impl CaptureBackend for SyntheticCapture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
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
        Ok(VideoFrame {
            width: self.width,
            height: self.height,
            ts_ms: unix_ms() as u32,
            format: PixelFormat::Bgra,
            planes: FramePlanes::Bgra {
                bytes: Bytes::from(rgba),
                stride: self.width * 4,
            },
        })
    }

    fn backend_name(&self) -> &'static str {
        "synthetic"
    }

    fn native_format(&self) -> PixelFormat {
        PixelFormat::Bgra
    }
}

#[cfg(feature = "dev-synthetic-stream")]
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
