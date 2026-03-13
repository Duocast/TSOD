use anyhow::Result;

use crate::net::video_frame::{EncodedAccessUnit, VideoFrame};

#[derive(Debug, Clone)]
pub struct VideoSessionConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub target_bitrate_bps: u32,
    pub low_latency: bool,
    pub allow_frame_drop: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct DecodeMetadata {
    pub ts_ms: u32,
}

pub trait VideoEncoder: Send {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()>;
    fn request_keyframe(&mut self) -> Result<()>;
    fn update_bitrate(&mut self, bitrate_bps: u32) -> Result<()>;
    fn encode(&mut self, frame: VideoFrame) -> Result<EncodedAccessUnit>;
    fn backend_name(&self) -> &'static str;
}

#[derive(Clone)]
pub struct DecodedVideoFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub ts_ms: u32,
}

pub trait VideoDecoder: Send {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()>;
    fn decode(
        &mut self,
        encoded: &EncodedAccessUnit,
        metadata: DecodeMetadata,
    ) -> Result<DecodedVideoFrame>;
    fn reset(&mut self) -> Result<()>;
    fn backend_name(&self) -> &'static str;
}
