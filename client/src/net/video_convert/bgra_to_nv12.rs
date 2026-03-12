use anyhow::{anyhow, bail, Result};
use bytes::Bytes;

use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};

pub fn convert(frame: VideoFrame) -> Result<VideoFrame> {
    let (bgra, stride) = match &frame.planes {
        FramePlanes::Bgra { bytes, stride } => (bytes, *stride as usize),
        _ => bail!("BGRA input required"),
    };
    let w = frame.width as usize;
    let h = frame.height as usize;
    if w == 0 || h == 0 || (w & 1) != 0 || (h & 1) != 0 {
        bail!("NV12 conversion requires non-zero even dimensions")
    }

    let mut y = vec![0u8; w * h];
    let mut uv = vec![0u8; w * (h / 2)];

    for j in (0..h).step_by(2) {
        for i in (0..w).step_by(2) {
            let mut u_acc = 0f32;
            let mut v_acc = 0f32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let idx = (j + dy) * stride + (i + dx) * 4;
                    let b = bgra
                        .get(idx)
                        .ok_or_else(|| anyhow!("bgra row stride out of bounds"))?;
                    let g = bgra[idx + 1];
                    let r = bgra[idx + 2];
                    let rf = r as f32;
                    let gf = g as f32;
                    let bf = *b as f32;
                    let yv = (0.257 * rf + 0.504 * gf + 0.098 * bf + 16.0).clamp(0.0, 255.0);
                    y[(j + dy) * w + i + dx] = yv as u8;
                    u_acc += (-0.148 * rf - 0.291 * gf + 0.439 * bf + 128.0).clamp(0.0, 255.0);
                    v_acc += (0.439 * rf - 0.368 * gf - 0.071 * bf + 128.0).clamp(0.0, 255.0);
                }
            }
            let uv_idx = (j / 2) * w + i;
            uv[uv_idx] = (u_acc / 4.0) as u8;
            uv[uv_idx + 1] = (v_acc / 4.0) as u8;
        }
    }

    Ok(VideoFrame {
        width: frame.width,
        height: frame.height,
        ts_ms: frame.ts_ms,
        format: PixelFormat::Nv12,
        planes: FramePlanes::Nv12 {
            y: Bytes::from(y),
            uv: Bytes::from(uv),
            y_stride: frame.width,
            uv_stride: frame.width,
        },
    })
}
