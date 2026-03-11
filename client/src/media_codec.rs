use anyhow::Result;

use crate::net::video_frame::{EncodedFrame, VideoFrame};

#[derive(Debug, Clone)]
pub struct VideoSessionConfig {
    pub width: u32,
    pub height: u32,
    pub target_bitrate_bps: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct DecodeMetadata {
    pub ts_ms: u32,
}

pub trait VideoEncoder: Send {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()>;
    fn request_keyframe(&mut self) -> Result<()>;
    fn update_bitrate(&mut self, bitrate_bps: u32) -> Result<()>;
    fn encode(&mut self, frame: VideoFrame) -> Result<EncodedFrame>;
}

pub struct DecodedVideoFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub ts_ms: u32,
}

pub trait VideoDecoder: Send {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()>;
    fn decode(&mut self, encoded: &[u8], metadata: DecodeMetadata) -> Result<DecodedVideoFrame>;
}
