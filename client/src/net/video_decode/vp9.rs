use anyhow::Result;

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_frame::EncodedAccessUnit;

use super::av1::decode_realtime_payload;

pub struct Vp9LibvpxDecoder;

impl VideoDecoder for Vp9LibvpxDecoder {
    fn configure_session(&mut self, _config: VideoSessionConfig) -> Result<()> {
        Ok(())
    }

    fn decode(
        &mut self,
        encoded: &EncodedAccessUnit,
        metadata: DecodeMetadata,
    ) -> Result<DecodedVideoFrame> {
        decode_realtime_payload(encoded, metadata)
    }

    fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "libvpx"
    }
}
