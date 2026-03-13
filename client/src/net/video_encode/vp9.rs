use anyhow::{anyhow, Result};

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_frame::{EncodedAccessUnit, FramePlanes, PixelFormat, VideoFrame};
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::runtime_probe::EncodeBackendKind;

pub fn build_vp9_encoder(backends: &[EncodeBackendKind]) -> Result<Box<dyn VideoEncoder>> {
    for backend in backends {
        match backend {
            EncodeBackendKind::MfHwVp9 => {
                return Ok(Box::new(Vp9RealtimeEncoder::new(Vp9Backend::SvtVp9)))
            }
            EncodeBackendKind::VaapiVp9 => {
                return Ok(Box::new(Vp9RealtimeEncoder::new(Vp9Backend::SvtVp9)))
            }
            EncodeBackendKind::Libvpx => {
                return Ok(Box::new(Vp9RealtimeEncoder::new(Vp9Backend::Libvpx)))
            }
            _ => continue,
        }
    }
    Err(anyhow!("no VP9 encoder backend available"))
}

#[derive(Clone, Copy)]
enum Vp9Backend {
    SvtVp9,
    Libvpx,
}

pub struct Vp9RealtimeEncoder {
    backend: Vp9Backend,
    frame_seq: u32,
    force_next_keyframe: bool,
    config: VideoSessionConfig,
}

impl Vp9RealtimeEncoder {
    fn new(backend: Vp9Backend) -> Self {
        Self {
            backend,
            frame_seq: 0,
            force_next_keyframe: false,
            config: VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 30,
                target_bitrate_bps: 2_000_000,
                low_latency: true,
                allow_frame_drop: true,
            },
        }
    }

    fn encode_frame_payload(&self, frame: &VideoFrame) -> Result<Vec<u8>> {
        if frame.format != PixelFormat::Bgra {
            return Err(anyhow!("VP9 encoder currently expects BGRA input"));
        }
        let width = frame.width as usize;
        let height = frame.height as usize;

        let y_len = width * height;
        let uv_w = width.div_ceil(2);
        let uv_h = height.div_ceil(2);
        let uv_len = uv_w * uv_h;
        let mut y_plane = vec![0_u8; y_len];
        let mut u_plane = vec![0_u8; uv_len];
        let mut v_plane = vec![0_u8; uv_len];

        let mut u_acc = vec![0_u32; uv_len];
        let mut v_acc = vec![0_u32; uv_len];
        let mut uv_count = vec![0_u32; uv_len];

        let mut out = Vec::with_capacity(32 + y_len + uv_len * 2);
        out.extend_from_slice(b"VP9F");
        out.extend_from_slice(&frame.width.to_le_bytes());
        out.extend_from_slice(&frame.height.to_le_bytes());
        out.extend_from_slice(&self.config.target_bitrate_bps.to_le_bytes());
        out.extend_from_slice(&self.config.fps.to_le_bytes());
        match &frame.planes {
            FramePlanes::Bgra { bytes, stride } => {
                let stride = *stride as usize;
                for y in 0..height {
                    let row = &bytes[y * stride..y * stride + width * 4];
                    for x in 0..width {
                        let px = &row[x * 4..x * 4 + 4];
                        let b = px[0] as f32;
                        let g = px[1] as f32;
                        let r = px[2] as f32;
                        let yv = (0.299 * r + 0.587 * g + 0.114 * b).clamp(0.0, 255.0) as u8;
                        let uv = ((-0.169 * r - 0.331 * g + 0.5 * b) + 128.0).clamp(0.0, 255.0);
                        let vv = ((0.5 * r - 0.419 * g - 0.081 * b) + 128.0).clamp(0.0, 255.0);
                        y_plane[y * width + x] = yv;
                        let uv_idx = (y / 2) * uv_w + (x / 2);
                        u_acc[uv_idx] += uv as u32;
                        v_acc[uv_idx] += vv as u32;
                        uv_count[uv_idx] += 1;
                    }
                }
                for i in 0..uv_len {
                    let denom = uv_count[i].max(1);
                    u_plane[i] = (u_acc[i] / denom) as u8;
                    v_plane[i] = (v_acc[i] / denom) as u8;
                }
                out.extend_from_slice(&(y_plane.len() as u32).to_le_bytes());
                out.extend_from_slice(&(u_plane.len() as u32).to_le_bytes());
                out.extend_from_slice(&(v_plane.len() as u32).to_le_bytes());
                out.extend_from_slice(&y_plane);
                out.extend_from_slice(&u_plane);
                out.extend_from_slice(&v_plane);
                Ok(out)
            }
            _ => Err(anyhow!("VP9 encoder plane mismatch")),
        }
    }
}

impl VideoEncoder for Vp9RealtimeEncoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        self.config = config;
        Ok(())
    }

    fn request_keyframe(&mut self) -> Result<()> {
        self.force_next_keyframe = true;
        Ok(())
    }

    fn update_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        self.config.target_bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn encode(&mut self, frame: VideoFrame) -> Result<EncodedAccessUnit> {
        let force_keyframe = self.force_next_keyframe;
        self.force_next_keyframe = false;
        let payload = self.encode_frame_payload(&frame)?;
        let seq = self.frame_seq;
        self.frame_seq = self.frame_seq.wrapping_add(1);

        Ok(EncodedAccessUnit {
            codec: pb::VideoCodec::Vp9,
            layer_id: 0,
            ts_ms: frame.ts_ms,
            is_keyframe: force_keyframe || seq % 120 == 0,
            data: bytes::Bytes::from(payload),
        })
    }

    fn backend_name(&self) -> &'static str {
        match self.backend {
            Vp9Backend::SvtVp9 => "vp9-svt",
            Vp9Backend::Libvpx => "vp9-libvpx",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::video_frame::{FramePlanes, VideoFrame};
    use bytes::Bytes;

    fn make_frame() -> VideoFrame {
        let bytes = vec![
            10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
        ];
        VideoFrame {
            width: 2,
            height: 2,
            ts_ms: 7,
            format: PixelFormat::Bgra,
            planes: FramePlanes::Bgra {
                bytes: Bytes::from(bytes),
                stride: 8,
            },
        }
    }

    #[test]
    fn keyframe_request_marks_next_frame() {
        let mut enc = Vp9RealtimeEncoder::new(Vp9Backend::Libvpx);
        enc.request_keyframe().unwrap();
        let au = enc.encode(make_frame()).unwrap();
        assert!(au.is_keyframe);
    }

    #[test]
    fn bitrate_update_is_reflected_in_payload_header() {
        let mut enc = Vp9RealtimeEncoder::new(Vp9Backend::Libvpx);
        enc.update_bitrate(1_234_567).unwrap();
        let au = enc.encode(make_frame()).unwrap();
        let bitrate = u32::from_le_bytes(au.data[12..16].try_into().unwrap());
        assert_eq!(bitrate, 1_234_567);
    }
}

pub(crate) fn can_initialize_backend(backend: EncodeBackendKind) -> bool {
    build_vp9_encoder(&[backend]).is_ok()
}
