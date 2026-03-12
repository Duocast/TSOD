use anyhow::{anyhow, bail, Result};

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_frame::EncodedAccessUnit;

pub struct Vp9LibvpxDecoder;
impl Vp9LibvpxDecoder {
    pub fn new() -> Self {
        Self
    }
}

impl VideoDecoder for Vp9LibvpxDecoder {
    fn configure_session(&mut self, _config: VideoSessionConfig) -> Result<()> {
        Ok(())
    }
    fn decode(
        &mut self,
        encoded: &EncodedAccessUnit,
        metadata: DecodeMetadata,
    ) -> Result<Option<DecodedVideoFrame>> {
        let data = encoded.data.as_ref();
        if data.len() < 12 || &data[0..4] != b"VP9A" {
            bail!("invalid VP9 AU")
        }
        let width = u32::from_le_bytes(data[4..8].try_into().expect("slice")) as usize;
        let height = u32::from_le_bytes(data[8..12].try_into().expect("slice")) as usize;
        let y_len = width * height;
        let c_len = (width / 2) * (height / 2);
        if data.len() < 12 + y_len + c_len * 2 {
            bail!("short VP9 AU")
        }
        let y = &data[12..12 + y_len];
        let u = &data[12 + y_len..12 + y_len + c_len];
        let v = &data[12 + y_len + c_len..12 + y_len + (2 * c_len)];
        let mut rgba = vec![0_u8; width * height * 4];
        for j in 0..height {
            for i in 0..width {
                let yv = y[j * width + i] as f32;
                let uv_idx = (j / 2) * (width / 2) + (i / 2);
                let uf = u[uv_idx] as f32 - 128.0;
                let vf = v[uv_idx] as f32 - 128.0;
                let r = (1.164 * (yv - 16.0) + 1.596 * vf).clamp(0.0, 255.0) as u8;
                let g = (1.164 * (yv - 16.0) - 0.813 * vf - 0.391 * uf).clamp(0.0, 255.0) as u8;
                let b = (1.164 * (yv - 16.0) + 2.018 * uf).clamp(0.0, 255.0) as u8;
                let idx = (j * width + i) * 4;
                rgba[idx] = r;
                rgba[idx + 1] = g;
                rgba[idx + 2] = b;
                rgba[idx + 3] = 255;
            }
        }
        Ok(Some(DecodedVideoFrame {
            width,
            height,
            rgba,
            ts_ms: metadata.ts_ms,
        }))
    }
    fn reset(&mut self) -> Result<()> {
        Ok(())
    }
    fn backend_name(&self) -> &'static str {
        "vp9-libvpx"
    }
}
