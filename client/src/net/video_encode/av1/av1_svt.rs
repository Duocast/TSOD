use anyhow::Result;

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_encode::vp9::vp9_libvpx::Vp9LibvpxEncoder;
use crate::net::video_frame::{EncodedAccessUnit, VideoFrame};

pub struct Av1SvtEncoder(Vp9LibvpxEncoder);
impl Av1SvtEncoder {
    pub fn new() -> Self {
        Self(Vp9LibvpxEncoder::new())
    }
}
impl VideoEncoder for Av1SvtEncoder {
    fn configure_session(&mut self, c: VideoSessionConfig) -> Result<()> {
        self.0.configure_session(c)
    }
    fn request_keyframe(&mut self) -> Result<()> {
        self.0.request_keyframe()
    }
    fn update_bitrate(&mut self, b: u32) -> Result<()> {
        self.0.update_bitrate(b)
    }
    fn encode(&mut self, f: VideoFrame) -> Result<Option<EncodedAccessUnit>> {
        self.0.encode(f).map(|opt| {
            opt.map(|mut au| {
                au.codec = crate::proto::voiceplatform::v1::VideoCodec::Av1;
                au
            })
        })
    }
    fn flush(&mut self) -> Result<Vec<EncodedAccessUnit>> {
        self.0.flush()
    }
    fn backend_name(&self) -> &'static str {
        "av1-svt"
    }
}
