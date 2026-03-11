use anyhow::{anyhow, Result};

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_frame::{EncodedAccessUnit, FramePlanes, PixelFormat, VideoFrame};
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::runtime_probe::EncodeBackendKind;

pub fn build_vp9_encoder(backends: &[EncodeBackendKind]) -> Result<Box<dyn VideoEncoder>> {
    for backend in backends {
        match backend {
            EncodeBackendKind::MfHwVp9 => {
                return Ok(Box::new(Vp9RealtimeEncoder::new(Vp9Backend::MfHw)))
            }
            EncodeBackendKind::VaapiVp9 => {
                return Ok(Box::new(Vp9RealtimeEncoder::new(Vp9Backend::VaapiHw)))
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
    Libvpx,
    MfHw,
    VaapiHw,
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
        // Placeholder payload shape for current branch: 8-byte WH header + BGRA bytes.
        // This keeps transport on EncodedAccessUnit while backend plumbing is now real/stateful.
        if frame.format != PixelFormat::Bgra {
            return Err(anyhow!("VP9 encoder currently expects BGRA input"));
        }
        let width = frame.width as usize;
        let height = frame.height as usize;
        let mut out = Vec::with_capacity(8 + (width * height * 4));
        out.extend_from_slice(&frame.width.to_le_bytes());
        out.extend_from_slice(&frame.height.to_le_bytes());
        match &frame.planes {
            FramePlanes::Bgra { bytes, stride } => {
                let stride = *stride as usize;
                for y in 0..height {
                    let row = &bytes[y * stride..y * stride + width * 4];
                    out.extend_from_slice(row);
                }
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
            Vp9Backend::Libvpx => "vp9-libvpx",
            Vp9Backend::MfHw => "vp9-mf",
            Vp9Backend::VaapiHw => "vp9-vaapi",
        }
    }
}
