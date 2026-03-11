use anyhow::Result;

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_encode::{apply_config, encode_raw_payload};
use crate::net::video_frame::{EncodedAccessUnit, VideoFrame};
use crate::proto::voiceplatform::v1 as pb;

#[derive(Clone, Copy)]
enum Av1EncoderBackend {
    Hardware,
    SvtAv1,
}

pub struct Av1RealtimeEncoder {
    frame_seq: u32,
    backend: Av1EncoderBackend,
    config: VideoSessionConfig,
    force_next_keyframe: bool,
}

impl Av1RealtimeEncoder {
    pub fn new() -> Self {
        let backend = if std::env::var("VP_AV1_DISABLE_HW").ok().as_deref() == Some("1") {
            Av1EncoderBackend::SvtAv1
        } else {
            Av1EncoderBackend::Hardware
        };
        Self {
            frame_seq: 0,
            backend,
            config: VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 30,
                target_bitrate_bps: 2_000_000,
                low_latency: true,
                allow_frame_drop: true,
            },
            force_next_keyframe: false,
        }
    }
}

impl VideoEncoder for Av1RealtimeEncoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        apply_config(&mut self.config, config);
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
        let backend_tag = match self.backend {
            Av1EncoderBackend::Hardware => 1,
            Av1EncoderBackend::SvtAv1 => 2,
        };
        let encoded = encode_raw_payload(
            frame,
            pb::VideoCodec::Av1,
            backend_tag,
            force_keyframe,
            self.frame_seq,
        )?;
        self.frame_seq = self.frame_seq.wrapping_add(1);
        Ok(encoded)
    }

    fn backend_name(&self) -> &'static str {
        match self.backend {
            Av1EncoderBackend::Hardware => "windows-mf-d3d11",
            Av1EncoderBackend::SvtAv1 => "svt-av1",
        }
    }
}
