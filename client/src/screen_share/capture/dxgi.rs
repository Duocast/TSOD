use anyhow::anyhow;

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};

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
    #[cfg(target_os = "windows")]
    inner: windows_impl::WindowsCapture,
    last_damage: Option<DamageMetadata>,
}

impl DxgiCapture {
    pub fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
        #[cfg(not(target_os = "windows"))]
        {
            let _ = source;
            Err(anyhow!("DXGI backend only available on Windows"))
        }

        #[cfg(target_os = "windows")]
        {
            let inner = windows_impl::WindowsCapture::from_source(source)?;
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
        #[cfg(not(target_os = "windows"))]
        {
            Err(anyhow!("DXGI backend only available on Windows"))
        }

        #[cfg(target_os = "windows")]
        {
            let frame = self.inner.next_frame()?;
            self.last_damage = Some(DamageMetadata {
                frame_ts_ms: frame.ts_ms,
                dirty_regions: self.inner.last_damage_regions(frame.width, frame.height),
            });
            Ok(frame)
        }
    }

    fn backend_name(&self) -> &'static str {
        #[cfg(target_os = "windows")]
        {
            self.inner.backend_name()
        }
        #[cfg(not(target_os = "windows"))]
        {
            "dxgi-unavailable"
        }
    }

    fn native_format(&self) -> PixelFormat {
        PixelFormat::Bgra
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::DirtyRegion;
    use anyhow::{anyhow, Context};
    use bytes::Bytes;
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetClientRect,
        GetDC, GetDIBits, GetWindowDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
    };

    use crate::media_capture::CaptureBackend;
    use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};

    pub(super) enum WindowsCapture {
        Display { idx: usize },
        Window { hwnd: HWND },
    }

    impl WindowsCapture {
        pub(super) fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
            match source {
                crate::ShareSource::WindowsDisplay(id) => Ok(Self::Display {
                    idx: parse_screen_id(id)?,
                }),
                crate::ShareSource::WindowsWindow(id) => {
                    let hwnd = parse_hwnd(id)?;
                    Ok(Self::Window { hwnd })
                }
                _ => Err(anyhow!(
                    "windows capture backend only supports windows sources"
                )),
            }
        }

        pub(super) fn backend_name(&self) -> &'static str {
            match self {
                Self::Display { .. } => "windows-gdi-display",
                Self::Window { .. } => "windows-gdi-window",
            }
        }

        pub(super) fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
            match self {
                Self::Display { .. } => capture_display(),
                Self::Window { hwnd } => capture_window(*hwnd),
            }
        }

        pub(super) fn last_damage_regions(&self, width: u32, height: u32) -> Vec<DirtyRegion> {
            vec![DirtyRegion {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            }]
        }
    }

    fn parse_screen_id(id: &str) -> anyhow::Result<usize> {
        id.strip_prefix("screen-")
            .unwrap_or(id)
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| anyhow!("invalid windows display id: {id}"))
    }

    fn parse_hwnd(id: &str) -> anyhow::Result<HWND> {
        let raw = id
            .strip_prefix("window-hwnd-")
            .unwrap_or(id)
            .parse::<isize>()
            .map_err(|_| anyhow!("invalid window id: {id}"))?;
        Ok(HWND(raw))
    }

    fn capture_display() -> anyhow::Result<VideoFrame> {
        unsafe { capture_dc(GetDC(None), None) }
    }

    fn capture_window(hwnd: HWND) -> anyhow::Result<VideoFrame> {
        let mut rect = RECT::default();
        unsafe {
            GetClientRect(hwnd, &mut rect).ok()?;
        }
        let width = (rect.right - rect.left).max(1) as u32;
        let height = (rect.bottom - rect.top).max(1) as u32;
        unsafe { capture_dc(GetWindowDC(hwnd), Some((width, height))) }
    }

    unsafe fn capture_dc(src: HDC, force_size: Option<(u32, u32)>) -> anyhow::Result<VideoFrame> {
        if src.is_invalid() {
            return Err(anyhow!("failed to get source DC"));
        }
        let (width, height) = force_size.unwrap_or((1920, 1080));
        let mem_dc = CreateCompatibleDC(src);
        let bmp: HBITMAP = CreateCompatibleBitmap(src, width as i32, height as i32);
        let old: HGDIOBJ = SelectObject(mem_dc, bmp);
        BitBlt(
            mem_dc,
            0,
            0,
            width as i32,
            height as i32,
            src,
            0,
            0,
            SRCCOPY,
        )
        .ok()?;

        let mut bi = BITMAPINFO::default();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut pixels = vec![0_u8; (width * height * 4) as usize];
        let rows = GetDIBits(
            mem_dc,
            bmp,
            0,
            height as u32,
            Some(pixels.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );

        let _ = SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, src);

        if rows == 0 {
            return Err(anyhow!("GetDIBits failed")).context("capture frame");
        }

        Ok(VideoFrame {
            width,
            height,
            ts_ms: super::unix_ms() as u32,
            format: PixelFormat::Bgra,
            planes: FramePlanes::Bgra {
                bytes: Bytes::from(pixels),
                stride: width * 4,
            },
        })
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
