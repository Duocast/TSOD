use anyhow::anyhow;
#[cfg(target_os = "windows")]
use anyhow::Context;

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

#[derive(Debug, Clone)]
pub struct DamageMetadata {
    pub frame_ts_ms: u32,
    pub dirty_regions: Vec<DirtyRegion>,
}

pub struct DxgiCapture {
    inner: ScrapCapture,
    last_damage: Option<DamageMetadata>,
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
            if !matches!(
                source,
                crate::ShareSource::WindowsDisplay(_) | crate::ShareSource::WindowsWindow(_)
            ) {
                return Err(anyhow!(
                    "DXGI backend only supports Windows display/window share sources"
                ));
            }

            // TODO(backend): Replace the migration fallback with explicit Desktop Duplication
            // output duplication and texture copy path once the dedicated DXGI implementation
            // lands. The trait contract and metadata plumbing are already in place.
            let inner = ScrapCapture::from_source(source)
                .context("initialize DXGI desktop duplication capture path")?;
            Ok(Self {
                inner,
                last_damage: None,
            })
        }
    }

    #[allow(dead_code)]
    pub fn take_damage(&mut self) -> Option<DamageMetadata> {
        self.last_damage.take()
    }
}

impl CaptureBackend for DxgiCapture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        let frame = self.inner.next_frame()?;
        self.last_damage = Some(DamageMetadata {
            frame_ts_ms: frame.ts_ms,
            dirty_regions: vec![DirtyRegion {
                left: 0,
                top: 0,
                right: frame.width,
                bottom: frame.height,
            }],
        });
        Ok(frame)
    }

    fn backend_name(&self) -> &'static str {
        "dxgi-duplication"
    }

    fn native_format(&self) -> PixelFormat {
        // DXGI duplication surfaces are typically BGRA8 in the migration path.
        PixelFormat::Bgra
    }
}
