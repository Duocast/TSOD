use anyhow::{anyhow, Context};
use bytes::Bytes;

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};

pub struct ScrapCapture {
    capturer: scrap::Capturer,
    width: u32,
    height: u32,
}

// SAFETY: ScrapCapture is only used from a single dedicated capture thread.
// The raw pointers inside scrap::Capturer (DXGI COM objects) are not accessed
// concurrently; the capture loop is the sole owner.
unsafe impl Send for ScrapCapture {}

impl ScrapCapture {
    pub fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
        let displays = scrap::Display::all().context("enumerate displays")?;
        let display = match source {
            crate::ShareSource::WindowsDisplay(id) => {
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
            crate::ShareSource::WindowsWindow(id) => {
                let _ = id;
                scrap::Display::primary().context("resolve primary display")?
            }
            crate::ShareSource::LinuxPortal(id) => {
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
            crate::ShareSource::X11Window(id) => {
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

impl CaptureBackend for ScrapCapture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        loop {
            match self.capturer.frame() {
                Ok(frame) => {
                    let stride = (frame.len() / self.height as usize) as u32;
                    return Ok(VideoFrame {
                        width: self.width,
                        height: self.height,
                        ts_ms: unix_ms() as u32,
                        format: PixelFormat::Bgra,
                        planes: FramePlanes::Bgra {
                            bytes: Bytes::copy_from_slice(&frame),
                            stride,
                        },
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(e).context("read captured screen frame"),
            }
        }
    }

    fn backend_name(&self) -> &'static str {
        "scrap"
    }

    fn native_format(&self) -> PixelFormat {
        PixelFormat::Bgra
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
