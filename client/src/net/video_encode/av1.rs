use anyhow::{anyhow, bail, Result};

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_frame::{EncodedAccessUnit, FramePlanes, PixelFormat, VideoFrame};
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::config::SenderPolicy;
use crate::screen_share::runtime_probe::EncodeBackendKind;

pub fn build_av1_encoder(
    backends: &[EncodeBackendKind],
    policy: SenderPolicy,
) -> Result<Box<dyn VideoEncoder>> {
    for backend in backends {
        match backend {
            EncodeBackendKind::MfHwAv1 => {
                return Ok(Box::new(Av1RealtimeEncoder::new(Av1Backend::MfHw)))
            }
            EncodeBackendKind::VaapiAv1 => {
                return Ok(Box::new(Av1RealtimeEncoder::new(Av1Backend::VaapiHw)))
            }
            EncodeBackendKind::SvtAv1 if cfg!(feature = "video-av1-software") => {
                return Ok(Box::new(Av1RealtimeEncoder::new(Av1Backend::SvtAv1)))
            }
            _ => continue,
        }
    }

    if matches!(policy, SenderPolicy::AutoLowLatency) {
        bail!("interactive AV1 encode requires hardware or the `video-av1-software` feature")
    }

    Err(anyhow!("no AV1 encoder backend available"))
}

#[derive(Clone, Copy)]
enum Av1Backend {
    MfHw,
    VaapiHw,
    SvtAv1,
}

pub struct Av1RealtimeEncoder {
    backend: Av1Backend,
    frame_seq: u32,
    force_next_keyframe: bool,
    config: VideoSessionConfig,
}

impl Av1RealtimeEncoder {
    fn new(backend: Av1Backend) -> Self {
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
            return Err(anyhow!("AV1 encoder currently expects BGRA input"));
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
            _ => Err(anyhow!("AV1 encoder plane mismatch")),
        }
    }
}

impl VideoEncoder for Av1RealtimeEncoder {
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
            codec: pb::VideoCodec::Av1,
            layer_id: 0,
            ts_ms: frame.ts_ms,
            is_keyframe: force_keyframe || seq % 120 == 0,
            data: bytes::Bytes::from(payload),
        })
    }

    fn backend_name(&self) -> &'static str {
        match self.backend {
            Av1Backend::MfHw => "av1-mf",
            Av1Backend::VaapiHw => "av1-vaapi",
            Av1Backend::SvtAv1 => "av1-svt",
        }
    }
}
