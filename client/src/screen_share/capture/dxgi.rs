use anyhow::{anyhow, Context};

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{PixelFormat, VideoFrame};

use super::scrap_fallback::ScrapCapture;

#[derive(Debug, Clone)]
pub struct DirtyRegion {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

pub struct DxgiCapture {
    inner: ScrapCapture,
    last_dirty_regions: Vec<DirtyRegion>,
}

impl DxgiCapture {
    pub fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
        #[cfg(not(target_os = "windows"))]
        {
            let _ = source;
            return Err(anyhow!(
                "DXGI desktop duplication is only available on Windows"
            ));
        }

        #[cfg(target_os = "windows")]
        {
            let inner = ScrapCapture::from_source(source)
                .context("initialize DXGI desktop duplication capture")?;
            Ok(Self {
                inner,
                last_dirty_regions: Vec::new(),
            })
        }
    }

    #[allow(dead_code)]
    pub fn take_dirty_regions(&mut self) -> Vec<DirtyRegion> {
        std::mem::take(&mut self.last_dirty_regions)
    }
}

impl CaptureBackend for DxgiCapture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        let frame = self.inner.next_frame()?;
        self.last_dirty_regions.clear();
        self.last_dirty_regions.push(DirtyRegion {
            left: 0,
            top: 0,
            right: frame.width,
            bottom: frame.height,
        });
        Ok(frame)
    }

    fn backend_name(&self) -> &'static str {
        "dxgi-duplication"
    }

    fn native_format(&self) -> PixelFormat {
        self.inner.native_format()
    }
}
