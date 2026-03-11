use anyhow::Result;

use crate::{CapturedFrame, EncodedFrame};

pub trait VideoEncoder: Send {
    fn encode(&mut self, frame: CapturedFrame) -> Result<EncodedFrame>;
}

pub struct DecodedVideoFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub trait VideoDecoder: Send {
    fn decode(&mut self, encoded: &[u8]) -> Result<DecodedVideoFrame>;
}
