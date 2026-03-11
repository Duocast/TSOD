use anyhow::Result;

use crate::net::video_frame::VideoFrame;

pub trait CaptureBackend: Send {
    fn next_frame(&mut self) -> Result<VideoFrame>;
}
