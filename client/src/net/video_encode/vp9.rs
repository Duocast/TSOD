use anyhow::Result;

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_encode::av1::Av1RealtimeEncoder;
use crate::net::video_frame::{EncodedFrame, VideoFrame};

pub struct Vp9RealtimeEncoder {
    inner: Av1RealtimeEncoder,
}

impl Vp9RealtimeEncoder {
    pub fn new() -> Self {
        Self {
            inner: Av1RealtimeEncoder::new(),
        }
    }
}

impl VideoEncoder for Vp9RealtimeEncoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        self.inner.configure_session(config)
    }

    fn request_keyframe(&mut self) -> Result<()> {
        self.inner.request_keyframe()
    }

    fn update_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        self.inner.update_bitrate(bitrate_bps)
    }

    fn encode(&mut self, frame: VideoFrame) -> Result<EncodedFrame> {
        self.inner.encode(frame)
    }
}
