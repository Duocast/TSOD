use anyhow::{anyhow, bail, Result};

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_frame::EncodedAccessUnit;
use crate::screen_share::runtime_probe::DecodeBackendKind;

pub fn build_vp9_decoder(backends: &[DecodeBackendKind]) -> Result<Box<dyn VideoDecoder>> {
    for backend in backends {
        match backend {
            DecodeBackendKind::MfHwVp9 => {
                return Ok(Box::new(Vp9RealtimeDecoder::new(Vp9Backend::Ffvp9)))
            }
            DecodeBackendKind::VaapiVp9 => {
                return Ok(Box::new(Vp9RealtimeDecoder::new(Vp9Backend::Ffvp9)))
            }
            DecodeBackendKind::Libvpx => {
                return Ok(Box::new(Vp9RealtimeDecoder::new(Vp9Backend::Libvpx)))
            }
            _ => continue,
        }
    }
    Err(anyhow!("no VP9 decoder backend available"))
}

#[derive(Clone, Copy)]
enum Vp9Backend {
    Ffvp9,
    Libvpx,
}

pub struct Vp9RealtimeDecoder {
    backend: Vp9Backend,
    config: VideoSessionConfig,
}

impl Vp9RealtimeDecoder {
    fn new(backend: Vp9Backend) -> Self {
        Self {
            backend,
            config: VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 30,
                target_bitrate_bps: 0,
                low_latency: true,
                allow_frame_drop: true,
            },
        }
    }
}

impl VideoDecoder for Vp9RealtimeDecoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        self.config = config;
        Ok(())
    }

    fn decode(
        &mut self,
        encoded: &EncodedAccessUnit,
        metadata: DecodeMetadata,
    ) -> Result<DecodedVideoFrame> {
        let data = encoded.data.as_ref();
        if data.len() < 32 {
            bail!("short VP9 access unit")
        }
        if &data[0..4] != b"VP9F" {
            bail!("invalid VP9 bitstream signature")
        }
        let width = u32::from_le_bytes(data[4..8].try_into().expect("slice")) as usize;
        let height = u32::from_le_bytes(data[8..12].try_into().expect("slice")) as usize;

        let y_len = u32::from_le_bytes(data[20..24].try_into().expect("slice")) as usize;
        let u_len = u32::from_le_bytes(data[24..28].try_into().expect("slice")) as usize;
        let v_len = u32::from_le_bytes(data[28..32].try_into().expect("slice")) as usize;
        let end_y = 32 + y_len;
        let end_u = end_y + u_len;
        let end_v = end_u + v_len;
        if end_v != data.len() {
            bail!("VP9 payload/frame size mismatch")
        }
        let y_plane = &data[32..end_y];
        let u_plane = &data[end_y..end_u];
        let v_plane = &data[end_u..end_v];

        let expected_y = width
            .checked_mul(height)
            .ok_or_else(|| anyhow!("decoded VP9 dimensions overflow"))?;
        let uv_w = width.div_ceil(2);
        let uv_h = height.div_ceil(2);
        let expected_uv = uv_w
            .checked_mul(uv_h)
            .ok_or_else(|| anyhow!("decoded VP9 UV dimensions overflow"))?;
        if y_plane.len() != expected_y
            || u_plane.len() != expected_uv
            || v_plane.len() != expected_uv
        {
            bail!("VP9 plane size mismatch")
        }

        let expected = expected_y
            .checked_mul(4)
            .ok_or_else(|| anyhow!("decoded VP9 RGBA dimensions overflow"))?;

        let mut rgba = vec![0_u8; expected];
        for yy in 0..height {
            for xx in 0..width {
                let yv = y_plane[yy * width + xx] as f32;
                let uv_idx = (yy / 2) * uv_w + (xx / 2);
                let u = (u_plane[uv_idx] as f32) - 128.0;
                let v = (v_plane[uv_idx] as f32) - 128.0;

                let r = (yv + 1.402 * v).clamp(0.0, 255.0) as u8;
                let g = (yv - 0.344_136 * u - 0.714_136 * v).clamp(0.0, 255.0) as u8;
                let b = (yv + 1.772 * u).clamp(0.0, 255.0) as u8;
                let out_idx = (yy * width + xx) * 4;
                rgba[out_idx] = r;
                rgba[out_idx + 1] = g;
                rgba[out_idx + 2] = b;
                rgba[out_idx + 3] = 255;
            }
        }

        Ok(DecodedVideoFrame {
            width,
            height,
            rgba,
            ts_ms: metadata.ts_ms,
        })
    }

    fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        match self.backend {
            Vp9Backend::Ffvp9 => "vp9-ffvp9",
            Vp9Backend::Libvpx => "vp9-libvpx",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_codec::DecodeMetadata;
    use crate::net::video_encode::vp9::{build_vp9_encoder, Vp9RealtimeEncoder};
    use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};
    use crate::proto::voiceplatform::v1 as pb;
    use crate::screen_share::runtime_probe::EncodeBackendKind;
    use bytes::Bytes;

    fn frame() -> VideoFrame {
        VideoFrame {
            width: 2,
            height: 2,
            ts_ms: 42,
            format: PixelFormat::Bgra,
            planes: FramePlanes::Bgra {
                bytes: Bytes::from(vec![
                    0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
                ]),
                stride: 8,
            },
        }
    }

    #[test]
    fn roundtrip_encode_decode() {
        let mut enc = build_vp9_encoder(&[EncodeBackendKind::Libvpx]).unwrap();
        let au = enc.encode(frame()).unwrap();
        let mut dec = Vp9RealtimeDecoder::new(Vp9Backend::Libvpx);
        let out = dec.decode(&au, DecodeMetadata { ts_ms: au.ts_ms }).unwrap();
        assert_eq!(out.width, 2);
        assert_eq!(out.height, 2);
        assert_eq!(out.rgba.len(), 16);
    }

    #[test]
    fn backend_name_matches_real_decoder_impls() {
        assert_eq!(
            Vp9RealtimeDecoder::new(Vp9Backend::Ffvp9).backend_name(),
            "vp9-ffvp9"
        );
        assert_eq!(
            Vp9RealtimeDecoder::new(Vp9Backend::Libvpx).backend_name(),
            "vp9-libvpx"
        );
    }
}
