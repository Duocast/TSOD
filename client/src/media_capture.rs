use anyhow::Result;

use crate::net::video_frame::{PixelFormat, VideoFrame};

pub trait CaptureBackend: Send {
    fn next_frame(&mut self) -> Result<VideoFrame>;
    fn backend_name(&self) -> &'static str;
    fn native_format(&self) -> PixelFormat;
}
