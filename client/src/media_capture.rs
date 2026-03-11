use anyhow::Result;

use crate::CapturedFrame;

pub trait CaptureBackend: Send {
    fn next_frame(&mut self) -> Result<CapturedFrame>;
}
