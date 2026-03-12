use anyhow::{bail, Result};

use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};

pub mod bgra_to_i420;
pub mod bgra_to_nv12;

pub fn convert_frame(frame: VideoFrame, target: PixelFormat) -> Result<VideoFrame> {
    if frame.format == target {
        return Ok(frame);
    }
    match (frame.format, target) {
        (PixelFormat::Bgra, PixelFormat::Nv12) => bgra_to_nv12::convert(frame),
        (PixelFormat::Bgra, PixelFormat::I420) => bgra_to_i420::convert(frame),
        _ => bail!("unsupported conversion {:?} -> {:?}", frame.format, target),
    }
}

pub fn plane_summary(frame: &VideoFrame) -> &'static str {
    match &frame.planes {
        FramePlanes::Bgra { .. } => "bgra",
        FramePlanes::Nv12 { .. } => "nv12",
        FramePlanes::I420 { .. } => "i420",
    }
}
