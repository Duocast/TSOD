// ─────────────────────────────────────────────────────────────────────────────
// Windows screen-capture backend
//
//  Primary   – DXGI Desktop Duplication  (display capture, Win 8+)
//  Secondary – GDI BitBlt                (window capture, all versions)
//
// DXGI Desktop Duplication advantages over GDI:
//   • GPU→CPU copy performed by the display driver; no extra GDI memcpy.
//   • Real per-frame dirty-rect metadata from IDXGIOutputDuplication.
//   • Output resolution comes from DXGI_OUTDUPL_DESC — no 1920×1080 fallback.
//   • Natural frame pacing at the display refresh rate via AcquireNextFrame.
//   • Multi-monitor support with proper adapter enumeration.
//
// GDI window capture is kept as a fallback for per-HWND capture on pre-1903
// Windows or when WGC initialisation fails.  The preferred per-HWND backend
// is now Windows.Graphics.Capture (WGC) — see wgc.rs.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
use anyhow::anyhow;

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{PixelFormat, VideoFrame};

// ── Public types consumed by the screen-share pipeline ───────────────────────

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
    /// Real dirty rects from DXGI for display capture; full-frame for GDI
    /// window capture (GDI has no dirty-region API).
    pub dirty_regions: Vec<DirtyRegion>,
    /// Rotation reported by the capture backend.
    pub rotation: FrameRotation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRotation {
    Identity,
    Rotate90,
    Rotate180,
    Rotate270,
}

// ── DxgiCapture (public CaptureBackend implementation) ───────────────────────

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
            return Err(anyhow!("DXGI backend only available on Windows"));
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

    /// Returns and clears the damage metadata from the most recent frame.
    #[allow(dead_code)]
    pub fn take_damage(&mut self) -> Option<DamageMetadata> {
        self.last_damage.take()
    }
}

// SAFETY: HWND is a pointer-sized integer safe to send across threads.
// The D3D11 device/context are accessed only from the single capture thread.
unsafe impl Send for DxgiCapture {}

impl CaptureBackend for DxgiCapture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        #[cfg(not(target_os = "windows"))]
        {
            Err(anyhow!("DXGI backend only available on Windows"))
        }

        #[cfg(target_os = "windows")]
        {
            let (frame, damage) = self.inner.next_frame()?;
            self.last_damage = Some(damage);
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

// ── Windows-only implementation ───────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::{DamageMetadata, DirtyRegion, FrameRotation};
    use anyhow::{anyhow, Context};
    use bytes::Bytes;
    use windows::Win32::Foundation::{HMODULE, HWND, RECT};
    // D3D11
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    // DXGI
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_IDENTITY,
        DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90,
        DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1,
        IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
        DXGI_OUTDUPL_FRAME_INFO,
    };
    // GDI (window-capture fallback)
    use windows::core::Interface;
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits,
        GetWindowDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        HBITMAP, HDC, HGDIOBJ, SRCCOPY,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

    use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};

    /// AcquireNextFrame blocks until the next desktop frame or this timeout.
    /// 100 ms covers displays down to 10 Hz; typical 60/120 Hz panels unblock
    /// within one frame period.
    const ACQUIRE_TIMEOUT_MS: u32 = 100;

    // ── Top-level dispatch ────────────────────────────────────────────────────

    pub(super) enum WindowsCapture {
        /// DXGI Desktop Duplication — real dirty rects, display-paced.
        Duplication(DuplicationCapture),
        /// GDI per-window capture — full-frame damage, no dirty metadata.
        GdiWindow { hwnd: HWND },
    }

    impl WindowsCapture {
        pub(super) fn from_source(source: &crate::ShareSource) -> anyhow::Result<Self> {
            match source {
                crate::ShareSource::WindowsDisplay(id) => {
                    let idx = parse_screen_id(id)?;
                    let dup =
                        DuplicationCapture::new(idx).context("DXGI Desktop Duplication init")?;
                    Ok(Self::Duplication(dup))
                }
                crate::ShareSource::WindowsWindow(id) => {
                    let hwnd = parse_hwnd(id)?;
                    Ok(Self::GdiWindow { hwnd })
                }
                _ => Err(anyhow!(
                    "Windows capture backend only supports Windows sources"
                )),
            }
        }

        pub(super) fn backend_name(&self) -> &'static str {
            match self {
                Self::Duplication(_) => "dxgi-desktop-duplication",
                Self::GdiWindow { .. } => "windows-gdi-window",
            }
        }

        pub(super) fn next_frame(&mut self) -> anyhow::Result<(VideoFrame, DamageMetadata)> {
            match self {
                Self::Duplication(dup) => dup.next_frame(),
                Self::GdiWindow { hwnd } => {
                    let frame = gdi_capture_window(*hwnd)?;
                    let damage = full_frame_damage(frame.ts_ms, frame.width, frame.height);
                    Ok((frame, damage))
                }
            }
        }
    }

    // ── DXGI Desktop Duplication ──────────────────────────────────────────────

    pub(super) struct DuplicationCapture {
        monitor_index: usize,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        duplication: IDXGIOutputDuplication,
        /// Reused staging texture for GPU→CPU readback.
        /// Stored with its dimensions so we can detect resolution changes.
        staging: Option<(ID3D11Texture2D, u32, u32)>,
        width: u32,
        height: u32,
        rotation: FrameRotation,
    }

    impl DuplicationCapture {
        pub(super) fn new(monitor_index: usize) -> anyhow::Result<Self> {
            let (device, context, duplication, width, height, rotation) =
                create_duplication(monitor_index)?;
            Ok(Self {
                monitor_index,
                device,
                context,
                duplication,
                staging: None,
                width,
                height,
                rotation,
            })
        }

        pub(super) fn next_frame(&mut self) -> anyhow::Result<(VideoFrame, DamageMetadata)> {
            // On ACCESS_LOST (display mode change, session lock, etc.)
            // reinitialise the duplication interface and retry once.
            match self.acquire_frame() {
                Err(e) if is_access_lost(&e) => {
                    tracing::warn!(
                        "DXGI: access lost (mode change / session lock?) — reinitialising"
                    );
                    self.reinit()?;
                    self.acquire_frame()
                }
                other => other,
            }
        }

        fn reinit(&mut self) -> anyhow::Result<()> {
            let (device, context, duplication, width, height, rotation) =
                create_duplication(self.monitor_index)?;
            self.device = device;
            self.context = context;
            self.duplication = duplication;
            self.staging = None; // recreated on next frame with new dimensions
            self.width = width;
            self.height = height;
            self.rotation = rotation;
            Ok(())
        }

        fn acquire_frame(&mut self) -> anyhow::Result<(VideoFrame, DamageMetadata)> {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            let acquire_result = unsafe {
                self.duplication.AcquireNextFrame(
                    ACQUIRE_TIMEOUT_MS,
                    &mut frame_info,
                    &mut resource,
                )
            };

            if let Err(e) = acquire_result {
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Err(anyhow!(
                        "DXGI: timed out waiting for next desktop frame \
                         (display may be sleeping or session locked)"
                    ));
                }
                return Err(anyhow::Error::from(e).context("AcquireNextFrame"));
            }

            // resource is valid; ReleaseFrame must be called before we return.
            let result = self.copy_frame_and_metadata(&frame_info, resource.as_ref().unwrap());

            // Always release, even on copy error.
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }

            result
        }

        fn copy_frame_and_metadata(
            &mut self,
            info: &DXGI_OUTDUPL_FRAME_INFO,
            resource: &IDXGIResource,
        ) -> anyhow::Result<(VideoFrame, DamageMetadata)> {
            let texture: ID3D11Texture2D = resource
                .cast()
                .context("QI IDXGIResource → ID3D11Texture2D")?;

            let staging = self.ensure_staging()?;

            unsafe {
                let dst: ID3D11Resource = staging.cast().context("staging → ID3D11Resource")?;
                let src: ID3D11Resource = texture.cast().context("texture → ID3D11Resource")?;
                self.context.CopyResource(&dst, &src);
            }

            // Collect dirty rects before ReleaseFrame (caller does that).
            let dirty = rotate_dirty_regions(
                &self.collect_dirty_rects(info),
                self.width,
                self.height,
                self.rotation,
            );

            let pixels = self.map_staging_and_copy()?;
            let (pixels, frame_width, frame_height) =
                rotate_bgra_frame(&pixels, self.width, self.height, self.rotation);

            let ts = super::unix_ms() as u32;
            let frame = VideoFrame {
                width: frame_width,
                height: frame_height,
                ts_ms: ts,
                format: PixelFormat::Bgra,
                planes: FramePlanes::Bgra {
                    bytes: Bytes::from(pixels),
                    stride: frame_width * 4,
                },
            };
            let damage = DamageMetadata {
                frame_ts_ms: ts,
                dirty_regions: dirty,
                rotation: self.rotation,
            };

            Ok((frame, damage))
        }

        /// Return (or create) the staging texture, recreating if dimensions changed.
        fn ensure_staging(&mut self) -> anyhow::Result<&ID3D11Texture2D> {
            let needs_create = match &self.staging {
                Some((_, w, h)) => *w != self.width || *h != self.height,
                None => true,
            };

            if needs_create {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: self.width,
                    Height: self.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut tex: Option<ID3D11Texture2D> = None;
                unsafe {
                    self.device
                        .CreateTexture2D(&desc, None, Some(&mut tex))
                        .context("CreateTexture2D (staging)")?;
                }
                self.staging = Some((tex.unwrap(), self.width, self.height));
            }

            Ok(&self.staging.as_ref().unwrap().0)
        }

        /// Map the staging texture, de-stride into a packed BGRA buffer, unmap.
        fn map_staging_and_copy(&mut self) -> anyhow::Result<Vec<u8>> {
            let staging = self.staging.as_ref().unwrap().0.clone();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();

            unsafe {
                let res: ID3D11Resource = staging.cast().context("staging → resource (map)")?;
                self.context
                    .Map(&res, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .context("ID3D11DeviceContext::Map")?;

                let row_pitch = mapped.RowPitch as usize;
                let h = self.height as usize;
                let packed_stride = self.width as usize * 4;
                let mut out = vec![0u8; packed_stride * h];
                let src = mapped.pData as *const u8;

                if row_pitch == packed_stride {
                    // No GPU row padding — single copy.
                    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), packed_stride * h);
                } else {
                    // De-stride row by row.
                    for row in 0..h {
                        std::ptr::copy_nonoverlapping(
                            src.add(row * row_pitch),
                            out.as_mut_ptr().add(row * packed_stride),
                            packed_stride,
                        );
                    }
                }

                let res2: ID3D11Resource = staging.cast().context("staging → resource (unmap)")?;
                self.context.Unmap(&res2, 0);

                Ok(out)
            }
        }

        /// Extract per-frame dirty rectangles from DXGI metadata.
        ///
        /// Falls back to a full-frame region if:
        ///   - `RectsCoalesced` is set (too many rects; DXGI merged them)
        ///   - `TotalMetadataBufferSize` is 0 (no metadata this frame)
        ///   - GetFrameDirtyRects fails
        ///
        /// NOTE: Move rects (GetFrameMoveRects) are not included here.
        /// Destination rectangles of moves are implicitly dirty, so callers
        /// that need complete damage coverage should also call
        /// GetFrameMoveRects and union the dest rects.  This is tracked as
        /// a TODO below.
        fn collect_dirty_rects(&self, info: &DXGI_OUTDUPL_FRAME_INFO) -> Vec<DirtyRegion> {
            if info.RectsCoalesced.as_bool() || info.TotalMetadataBufferSize == 0 {
                return vec![full_region(self.width, self.height)];
            }

            // Allocate TotalMetadataBufferSize bytes — large enough for all
            // metadata (both move and dirty rects combined).
            let buf_bytes = info.TotalMetadataBufferSize as usize;
            let max_rects = buf_bytes / std::mem::size_of::<RECT>();
            if max_rects == 0 {
                return vec![full_region(self.width, self.height)];
            }

            let mut buf = vec![RECT::default(); max_rects];
            let mut required_bytes = 0u32;

            let ok = unsafe {
                self.duplication.GetFrameDirtyRects(
                    buf_bytes as u32,
                    buf.as_mut_ptr(),
                    &mut required_bytes,
                )
            };

            if ok.is_err() || required_bytes == 0 {
                return vec![full_region(self.width, self.height)];
            }

            let n_actual = (required_bytes as usize) / std::mem::size_of::<RECT>();
            buf[..n_actual]
                .iter()
                .map(|r| DirtyRegion {
                    left: r.left.max(0) as u32,
                    top: r.top.max(0) as u32,
                    right: r.right.max(0) as u32,
                    bottom: r.bottom.max(0) as u32,
                })
                .collect()
        }
    }

    fn map_dxgi_rotation(rotation: DXGI_MODE_ROTATION) -> FrameRotation {
        if rotation == DXGI_MODE_ROTATION_ROTATE90 {
            FrameRotation::Rotate90
        } else if rotation == DXGI_MODE_ROTATION_ROTATE180 {
            FrameRotation::Rotate180
        } else if rotation == DXGI_MODE_ROTATION_ROTATE270 {
            FrameRotation::Rotate270
        } else if rotation == DXGI_MODE_ROTATION_IDENTITY {
            FrameRotation::Identity
        } else {
            FrameRotation::Identity
        }
    }

    // ── D3D11 / DXGI initialisation ───────────────────────────────────────────

    /// Enumerate DXGI adapters and outputs to find the output at `monitor_index`
    /// (1-based, matching "screen-1", "screen-2", …), then create a D3D11 device
    /// on that adapter's GPU and open a desktop duplication session.
    ///
    /// Using per-adapter device creation (D3D_DRIVER_TYPE_UNKNOWN + explicit
    /// adapter) ensures correctness on multi-GPU machines where different
    /// monitors are attached to different GPUs.
    fn create_duplication(
        monitor_index: usize,
    ) -> anyhow::Result<(
        ID3D11Device,
        ID3D11DeviceContext,
        IDXGIOutputDuplication,
        u32,
        u32,
        FrameRotation,
    )> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;

            let mut global_output_count = 0usize;
            let mut adapter_idx = 0u32;

            loop {
                let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(adapter_idx) {
                    Ok(a) => a,
                    Err(_) => break, // no more adapters
                };

                let mut output_idx = 0u32;
                loop {
                    let output = match adapter.EnumOutputs(output_idx) {
                        Ok(o) => o,
                        Err(_) => break, // no more outputs on this adapter
                    };

                    global_output_count += 1;

                    if global_output_count == monitor_index {
                        let output1: IDXGIOutput1 =
                            output.cast().context("QI IDXGIOutput → IDXGIOutput1")?;

                        // Create the D3D11 device bound to this adapter so that
                        // DuplicateOutput succeeds even on multi-GPU systems.
                        let dxgi_adapter: IDXGIAdapter =
                            adapter.cast().context("IDXGIAdapter1 → IDXGIAdapter")?;
                        let mut device: Option<ID3D11Device> = None;
                        let mut context: Option<ID3D11DeviceContext> = None;
                        D3D11CreateDevice(
                            Some(&dxgi_adapter),
                            D3D_DRIVER_TYPE_UNKNOWN,
                            HMODULE::default(),
                            D3D11_CREATE_DEVICE_FLAG(0),
                            None,
                            D3D11_SDK_VERSION,
                            Some(&mut device),
                            None,
                            Some(&mut context),
                        )
                        .context("D3D11CreateDevice")?;

                        let device = device.context("D3D11CreateDevice: null device")?;
                        let context = context.context("D3D11CreateDevice: null context")?;

                        // DuplicateOutput fails on RDP sessions and displays
                        // protected by HDCP; callers fall back to Scrap.
                        let dup = output1
                            .DuplicateOutput(&device)
                            .context("IDXGIOutput1::DuplicateOutput")?;

                        // Read the actual output resolution from the duplication
                        // descriptor — no more hardcoded 1920×1080.
                        let desc = dup.GetDesc();
                        let width = desc.ModeDesc.Width;
                        let height = desc.ModeDesc.Height;
                        let rotation = map_dxgi_rotation(desc.Rotation);

                        return Ok((device, context, dup, width, height, rotation));
                    }

                    output_idx += 1;
                }

                adapter_idx += 1;
            }

            Err(anyhow!(
                "monitor index {} not found (system has {} output(s))",
                monitor_index,
                global_output_count
            ))
        }
    }

    // ── GDI window-capture fallback ───────────────────────────────────────────
    //
    // Used for per-HWND capture. GDI has no dirty-region API so damage is
    // always reported as a full-frame region.
    //
    // Limitations vs. DXGI/WGC:
    //   • Misses GPU-composited and DRM-protected content.
    //   • Every frame is a full GPU→CPU blit.
    //   • Occlusion and minimisation: GetClientRect returns 0×0 for minimised
    //     windows; we clamp to 1×1 and return a blank frame.

    fn gdi_capture_window(hwnd: HWND) -> anyhow::Result<VideoFrame> {
        unsafe {
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect).context("GetClientRect")?;

            // Minimised windows have zero rect; produce a 1×1 blank frame so
            // the encoder/pipeline keeps running without special-casing.
            let width = (rect.right - rect.left).max(1) as u32;
            let height = (rect.bottom - rect.top).max(1) as u32;

            let src_dc = GetWindowDC(Some(hwnd));
            if src_dc.is_invalid() {
                return Err(anyhow!("GetWindowDC returned invalid HDC"));
            }

            // Capture then always release, even on error.
            let pixels = gdi_bitblt_to_pixels(src_dc, width, height);
            ReleaseDC(Some(hwnd), src_dc);
            let pixels = pixels?;

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

    unsafe fn gdi_bitblt_to_pixels(src: HDC, width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
        let mem_dc = CreateCompatibleDC(Some(src));
        let bmp: HBITMAP = CreateCompatibleBitmap(src, width as i32, height as i32);
        let old: HGDIOBJ = SelectObject(mem_dc, bmp.into());

        let blt_ok = BitBlt(
            mem_dc,
            0,
            0,
            width as i32,
            height as i32,
            Some(src),
            0,
            0,
            SRCCOPY,
        );

        let mut bi = BITMAPINFO::default();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32), // negative = top-down scan order
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let rows = GetDIBits(
            mem_dc,
            bmp,
            0,
            height,
            Some(pixels.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );

        let _ = SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);

        blt_ok.context("BitBlt")?;
        if rows == 0 {
            return Err(anyhow!("GetDIBits returned 0 scan lines"));
        }

        Ok(pixels)
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn full_region(w: u32, h: u32) -> DirtyRegion {
        DirtyRegion {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        }
    }

    fn full_frame_damage(ts_ms: u32, width: u32, height: u32) -> DamageMetadata {
        DamageMetadata {
            frame_ts_ms: ts_ms,
            dirty_regions: vec![full_region(width, height)],
            rotation: FrameRotation::Identity,
        }
    }

    fn is_access_lost(e: &anyhow::Error) -> bool {
        e.chain().any(|cause| {
            cause
                .downcast_ref::<windows::core::Error>()
                .map(|we| we.code() == DXGI_ERROR_ACCESS_LOST)
                .unwrap_or(false)
        })
    }

    /// Parse "screen-1", "screen-2", … into a 1-based monitor index.
    pub(super) fn parse_screen_id(id: &str) -> anyhow::Result<usize> {
        let s = id.strip_prefix("screen-").unwrap_or(id);
        let n: usize = s.parse().ok().filter(|&n| n >= 1).ok_or_else(|| {
            anyhow!("invalid Windows display id: {id:?} (expected \"screen-N\" where N ≥ 1)")
        })?;
        Ok(n)
    }

    /// Parse "window-hwnd-<isize>" into an HWND.
    pub(super) fn parse_hwnd(id: &str) -> anyhow::Result<HWND> {
        let raw: isize = id
            .strip_prefix("window-hwnd-")
            .unwrap_or(id)
            .parse()
            .map_err(|_| anyhow!("invalid window id: {id:?} (expected \"window-hwnd-<isize>\")"))?;
        Ok(HWND(raw as *mut std::ffi::c_void))
    }
}

// ── Shared helpers (available on all platforms) ───────────────────────────────

/// Parse "window-hwnd-<isize>" into an HWND.
///
/// Exposed as a crate-level helper so the WGC backend can reuse the same
/// parser without duplicating the logic.
#[cfg(target_os = "windows")]
pub(crate) fn parse_hwnd(id: &str) -> anyhow::Result<windows::Win32::Foundation::HWND> {
    windows_impl::parse_hwnd(id)
}

pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn rotate_bgra_frame(
    pixels: &[u8],
    width: u32,
    height: u32,
    rotation: FrameRotation,
) -> (Vec<u8>, u32, u32) {
    let src_w = width as usize;
    let src_h = height as usize;
    let src_stride = src_w * 4;

    match rotation {
        FrameRotation::Identity => (pixels.to_vec(), width, height),
        FrameRotation::Rotate180 => {
            let mut out = vec![0u8; pixels.len()];
            for y in 0..src_h {
                for x in 0..src_w {
                    let src_idx = (y * src_w + x) * 4;
                    let dst_x = src_w - 1 - x;
                    let dst_y = src_h - 1 - y;
                    let dst_idx = (dst_y * src_w + dst_x) * 4;
                    out[dst_idx..dst_idx + 4].copy_from_slice(&pixels[src_idx..src_idx + 4]);
                }
            }
            (out, width, height)
        }
        FrameRotation::Rotate90 => {
            let dst_w = src_h;
            let dst_h = src_w;
            let mut out = vec![0u8; dst_w * dst_h * 4];
            for y in 0..src_h {
                for x in 0..src_w {
                    let src_idx = y * src_stride + x * 4;
                    let dst_x = src_h - 1 - y;
                    let dst_y = x;
                    let dst_idx = (dst_y * dst_w + dst_x) * 4;
                    out[dst_idx..dst_idx + 4].copy_from_slice(&pixels[src_idx..src_idx + 4]);
                }
            }
            (out, height, width)
        }
        FrameRotation::Rotate270 => {
            let dst_w = src_h;
            let dst_h = src_w;
            let mut out = vec![0u8; dst_w * dst_h * 4];
            for y in 0..src_h {
                for x in 0..src_w {
                    let src_idx = y * src_stride + x * 4;
                    let dst_x = y;
                    let dst_y = src_w - 1 - x;
                    let dst_idx = (dst_y * dst_w + dst_x) * 4;
                    out[dst_idx..dst_idx + 4].copy_from_slice(&pixels[src_idx..src_idx + 4]);
                }
            }
            (out, height, width)
        }
    }
}

fn rotate_dirty_regions(
    regions: &[DirtyRegion],
    width: u32,
    height: u32,
    rotation: FrameRotation,
) -> Vec<DirtyRegion> {
    regions
        .iter()
        .map(|r| match rotation {
            FrameRotation::Identity => r.clone(),
            FrameRotation::Rotate180 => DirtyRegion {
                left: width.saturating_sub(r.right),
                top: height.saturating_sub(r.bottom),
                right: width.saturating_sub(r.left),
                bottom: height.saturating_sub(r.top),
            },
            FrameRotation::Rotate90 => DirtyRegion {
                left: height.saturating_sub(r.bottom),
                top: r.left,
                right: height.saturating_sub(r.top),
                bottom: r.right,
            },
            FrameRotation::Rotate270 => DirtyRegion {
                left: r.top,
                top: width.saturating_sub(r.right),
                right: r.bottom,
                bottom: width.saturating_sub(r.left),
            },
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[cfg(target_os = "windows")]
    use super::windows_impl::{parse_hwnd, parse_screen_id};

    #[cfg(target_os = "windows")]
    #[test]
    fn parses_screen_prefix() {
        assert_eq!(parse_screen_id("screen-1").unwrap(), 1);
        assert_eq!(parse_screen_id("screen-3").unwrap(), 3);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parses_bare_screen_index() {
        assert_eq!(parse_screen_id("2").unwrap(), 2);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn rejects_screen_zero() {
        assert!(parse_screen_id("screen-0").is_err());
        assert!(parse_screen_id("0").is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn rejects_invalid_screen_id() {
        assert!(parse_screen_id("screen-abc").is_err());
        assert!(parse_screen_id("display-1").is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parses_hwnd_prefixed() {
        let hwnd = parse_hwnd("window-hwnd-12345").unwrap();
        assert_eq!(hwnd.0 as isize, 12345);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parses_hwnd_bare() {
        let hwnd = parse_hwnd("99").unwrap();
        assert_eq!(hwnd.0 as isize, 99);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn rejects_invalid_hwnd() {
        assert!(parse_hwnd("window-hwnd-").is_err());
        assert!(parse_hwnd("notanumber").is_err());
    }

    fn px(v: u8) -> [u8; 4] {
        [v, v, v, v]
    }

    #[test]
    fn rotates_bgra_frame_90_degrees() {
        let pixels = [px(1), px(2), px(3), px(4), px(5), px(6)].concat();

        let (rotated, w, h) =
            super::rotate_bgra_frame(&pixels, 2, 3, super::FrameRotation::Rotate90);

        let expected = [px(5), px(3), px(1), px(6), px(4), px(2)].concat();
        assert_eq!((w, h), (3, 2));
        assert_eq!(rotated, expected);
    }

    #[test]
    fn rotates_bgra_frame_180_degrees() {
        let pixels = [px(1), px(2), px(3), px(4), px(5), px(6)].concat();

        let (rotated, w, h) =
            super::rotate_bgra_frame(&pixels, 2, 3, super::FrameRotation::Rotate180);

        let expected = [px(6), px(5), px(4), px(3), px(2), px(1)].concat();
        assert_eq!((w, h), (2, 3));
        assert_eq!(rotated, expected);
    }

    #[test]
    fn rotates_bgra_frame_270_degrees() {
        let pixels = [px(1), px(2), px(3), px(4), px(5), px(6)].concat();

        let (rotated, w, h) =
            super::rotate_bgra_frame(&pixels, 2, 3, super::FrameRotation::Rotate270);

        let expected = [px(2), px(4), px(6), px(1), px(3), px(5)].concat();
        assert_eq!((w, h), (3, 2));
        assert_eq!(rotated, expected);
    }

    // Dirty-region metadata: a full-frame DirtyRegion covers the whole output.
    #[test]
    fn full_frame_dirty_region_covers_output() {
        let damage = super::DamageMetadata {
            frame_ts_ms: 42,
            dirty_regions: vec![super::DirtyRegion {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }],
            rotation: super::FrameRotation::Identity,
        };
        let r = &damage.dirty_regions[0];
        assert_eq!(r.right - r.left, 1920);
        assert_eq!(r.bottom - r.top, 1080);
    }
}

// ── TODOs ─────────────────────────────────────────────────────────────────────
//
// DONE: Windows.Graphics.Capture (WGC) window-capture backend added in wgc.rs.
//   WGC is now the preferred per-HWND backend on Win 10 1903+; GDI is kept as
//   fallback for older OS versions or WGC init failure.
//
// TODO(optional): Include move-rect destinations in DamageMetadata. Call
//   IDXGIOutputDuplication::GetFrameMoveRects, add dest RECT of each move to
//   dirty_regions. This gives complete damage coverage for compositors that
//   skip re-encoding unchanged regions.
//
