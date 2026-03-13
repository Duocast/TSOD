use anyhow::{anyhow, Context};
use bytes::Bytes;

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};

use super::scrap_fallback::ScrapCapture;

pub struct X11Capture {
    display_capture: ScrapCapture,
    window_id: u64,
    geometry: WindowGeometry,
}

#[derive(Clone, Copy, Debug)]
struct WindowGeometry {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl X11Capture {
    pub fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
        let crate::ShareSource::X11Window(window_id) = source else {
            return Err(anyhow!(
                "x11 capture requires ShareSource::X11Window (dedicated X11 path)"
            ));
        };

        if std::env::var_os("DISPLAY").is_none() {
            return Err(anyhow!(
                "x11 capture requested but DISPLAY is not set; verify X11 session and permissions"
            ));
        }

        let geometry = query_geometry(*window_id)?;
        let display_capture =
            ScrapCapture::from_source(&crate::ShareSource::LinuxPortal("screen-1".to_string()))
                .context("initialize X11 display capture")?;
        Ok(Self {
            display_capture,
            window_id: *window_id,
            geometry,
        })
    }
}

impl CaptureBackend for X11Capture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        if let Ok(updated) = query_geometry(self.window_id) {
            self.geometry = updated;
        }
        let frame = self
            .display_capture
            .next_frame()
            .context("failed to capture X11 frame")?;
        crop_bgra(&frame, self.geometry)
    }

    fn backend_name(&self) -> &'static str {
        "x11-window"
    }

    fn native_format(&self) -> PixelFormat {
        PixelFormat::Bgra
    }
}

fn crop_bgra(frame: &VideoFrame, geometry: WindowGeometry) -> anyhow::Result<VideoFrame> {
    let FramePlanes::Bgra { bytes, stride } = &frame.planes else {
        return Err(anyhow!("x11 window capture expects BGRA frame"));
    };
    let src_stride = *stride as usize;
    let x = geometry.x.min(frame.width.saturating_sub(1));
    let y = geometry.y.min(frame.height.saturating_sub(1));
    let max_w = frame.width.saturating_sub(x);
    let max_h = frame.height.saturating_sub(y);
    let width = geometry.width.max(1).min(max_w);
    let height = geometry.height.max(1).min(max_h);

    let mut out = vec![0_u8; (width * height * 4) as usize];
    for row in 0..height as usize {
        let src_off = (y as usize + row) * src_stride + x as usize * 4;
        let dst_off = row * (width as usize * 4);
        let len = width as usize * 4;
        out[dst_off..dst_off + len].copy_from_slice(&bytes[src_off..src_off + len]);
    }

    Ok(VideoFrame {
        width,
        height,
        ts_ms: frame.ts_ms,
        format: PixelFormat::Bgra,
        planes: FramePlanes::Bgra {
            bytes: Bytes::from(out),
            stride: width * 4,
        },
    })
}

fn query_geometry(window_id: u64) -> anyhow::Result<WindowGeometry> {
    let output = std::process::Command::new("xwininfo")
        .arg("-id")
        .arg(format!("0x{window_id:x}"))
        .output()
        .context("run xwininfo")?;
    if !output.status.success() {
        return Err(anyhow!("xwininfo failed for window=0x{window_id:x}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);

    let x = parse_xwininfo_field(&stdout, "Absolute upper-left X:")?;
    let y = parse_xwininfo_field(&stdout, "Absolute upper-left Y:")?;
    let width = parse_xwininfo_field(&stdout, "Width:")?;
    let height = parse_xwininfo_field(&stdout, "Height:")?;
    Ok(WindowGeometry {
        x,
        y,
        width,
        height,
    })
}

fn parse_xwininfo_field(text: &str, key: &str) -> anyhow::Result<u32> {
    let value = text
        .lines()
        .find_map(|line| line.trim().strip_prefix(key).map(str::trim))
        .ok_or_else(|| anyhow!("missing xwininfo field: {key}"))?;
    value
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("invalid xwininfo field: {key}"))?
        .parse::<u32>()
        .map_err(|_| anyhow!("invalid numeric xwininfo value for {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xwininfo_numeric_field() {
        let sample = "  Width: 256\n  Height: 144\n";
        assert_eq!(parse_xwininfo_field(sample, "Width:").unwrap(), 256);
        assert_eq!(parse_xwininfo_field(sample, "Height:").unwrap(), 144);
    }
}
