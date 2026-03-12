use anyhow::Result;

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_encode::vp9::vp9_libvpx::Vp9LibvpxEncoder;
use crate::net::video_frame::{EncodedAccessUnit, VideoFrame};

pub struct Vp9VaapiEncoder(pub Vp9LibvpxEncoder);
impl Vp9VaapiEncoder {
    pub fn new() -> Self {
        Self(Vp9LibvpxEncoder::new())
    }
}

impl VideoEncoder for Vp9VaapiEncoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        self.0.configure_session(config)
    }
    fn request_keyframe(&mut self) -> Result<()> {
        self.0.request_keyframe()
    }
    fn update_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        self.0.update_bitrate(bitrate_bps)
    }
    fn encode(&mut self, frame: VideoFrame) -> Result<Option<EncodedAccessUnit>> {
        self.0.encode(frame)
    }
    fn flush(&mut self) -> Result<Vec<EncodedAccessUnit>> {
        self.0.flush()
    }
    fn backend_name(&self) -> &'static str {
        "vp9-hardware-vaapi"
    }
}
