use anyhow::Result;

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_decode::vp9::vp9_libvpx::Vp9LibvpxDecoder;
use crate::net::video_frame::EncodedAccessUnit;

pub struct Av1Dav1dDecoder(Vp9LibvpxDecoder);
impl Av1Dav1dDecoder {
    pub fn new() -> Self {
        Self(Vp9LibvpxDecoder::new())
    }
}
impl VideoDecoder for Av1Dav1dDecoder {
    fn configure_session(&mut self, c: VideoSessionConfig) -> Result<()> {
        self.0.configure_session(c)
    }
    fn decode(
        &mut self,
        e: &EncodedAccessUnit,
        m: DecodeMetadata,
    ) -> Result<Option<DecodedVideoFrame>> {
        self.0.decode(e, m)
    }
    fn reset(&mut self) -> Result<()> {
        self.0.reset()
    }
    fn backend_name(&self) -> &'static str {
        "av1-dav1d"
    }
}
