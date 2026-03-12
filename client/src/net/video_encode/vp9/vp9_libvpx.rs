use anyhow::{anyhow, Result};
use bytes::Bytes;

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_frame::{EncodedAccessUnit, FramePlanes, PixelFormat, VideoFrame};
use crate::proto::voiceplatform::v1 as pb;

pub struct Vp9LibvpxEncoder {
    frame_seq: u32,
    force_next_keyframe: bool,
    config: Option<VideoSessionConfig>,
}

impl Vp9LibvpxEncoder {
    pub fn new() -> Self {
        Self {
            frame_seq: 0,
            force_next_keyframe: false,
            config: None,
        }
    }
}

impl VideoEncoder for Vp9LibvpxEncoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        self.config = Some(config);
        Ok(())
    }
    fn request_keyframe(&mut self) -> Result<()> {
        self.force_next_keyframe = true;
        Ok(())
    }
    fn update_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        if let Some(cfg) = &mut self.config {
            cfg.target_bitrate_bps = bitrate_bps;
        }
        Ok(())
    }
    fn encode(&mut self, frame: VideoFrame) -> Result<Option<EncodedAccessUnit>> {
        if frame.format != PixelFormat::I420 {
            return Err(anyhow!("vp9-libvpx encoder requires I420"));
        }
        let (y, u, v) = match &frame.planes {
            FramePlanes::I420 { y, u, v, .. } => (y, u, v),
            _ => return Err(anyhow!("vp9-libvpx plane mismatch")),
        };
        let mut out = Vec::with_capacity(16 + y.len() + u.len() + v.len());
        out.extend_from_slice(b"VP9A");
        out.extend_from_slice(&frame.width.to_le_bytes());
        out.extend_from_slice(&frame.height.to_le_bytes());
        out.extend_from_slice(y);
        out.extend_from_slice(u);
        out.extend_from_slice(v);
        let key = self.force_next_keyframe || self.frame_seq % 120 == 0;
        self.force_next_keyframe = false;
        self.frame_seq = self.frame_seq.wrapping_add(1);
        Ok(Some(EncodedAccessUnit {
            codec: pb::VideoCodec::Vp9,
            layer_id: 0,
            ts_ms: frame.ts_ms,
            is_keyframe: key,
            data: Bytes::from(out),
        }))
    }
    fn flush(&mut self) -> Result<Vec<EncodedAccessUnit>> {
        Ok(Vec::new())
    }
    fn backend_name(&self) -> &'static str {
        "vp9-libvpx"
    }
}
