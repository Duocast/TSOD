// ─────────────────────────────────────────────────────────────────────────────
// Windows.Graphics.Capture (WGC) window-capture backend
//
// Preferred backend for per-HWND capture on Windows 10 1903+ (build 18362).
// Falls back to GDI BitBlt on older Windows versions or when WGC init fails.
//
// Advantages over GDI:
//   • Captures GPU-composited content (DirectX overlays, hardware cursors).
//   • Captures DRM-protected windows that GDI returns black for.
//   • GPU-side frame pool — no per-frame GDI BitBlt/GetDIBits round-trip.
//   • Resize/close events are surfaced through the capture item.
//
// OS version requirements:
//   • Windows 10 1903 (build 18362) — minimum for GraphicsCaptureSession.
//   • Windows 10 2004 (build 19041) — adds IsBorderRequired=false (no yellow
//     capture border).  We set this when available but do not require it.
//
// Architecture:
//   WgcCapture owns a Direct3D11CaptureFramePool + GraphicsCaptureSession.
//   `next_frame()` calls `TryGetNextFrame()` in a polling loop (with sleep)
//   and copies the resulting ID3D11Texture2D into a CPU-readable staging
//   texture, de-strides it into packed BGRA, and returns a VideoFrame.
//
//   The frame pool is created with `CreateFreeThreaded` so it is safe to
//   call from the blocking capture thread (no dispatcher/STA required).
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
use anyhow::anyhow;

use crate::media_capture::CaptureBackend;
use crate::net::video_frame::{PixelFormat, VideoFrame};

use super::dxgi::{DamageMetadata, DirtyRegion};

/// WGC-based window capture backend.
///
/// On non-Windows platforms, construction always fails immediately.
pub struct WgcCapture {
    #[cfg(target_os = "windows")]
    inner: wgc_impl::WgcCaptureInner,
    last_damage: Option<DamageMetadata>,
}

impl WgcCapture {
    /// Attempt to create a WGC capture session for the given HWND string.
    ///
    /// Returns `Err` if:
    ///   - The OS does not support WGC (pre–Windows 10 1903).
    ///   - The HWND is invalid or the interop call fails.
    ///   - D3D11 device creation fails.
    pub fn from_hwnd_str(hwnd_str: &str) -> anyhow::Result<Self> {
        #[cfg(not(target_os = "windows"))]
        {
            let _ = hwnd_str;
            return Err(anyhow!("WGC backend only available on Windows"));
        }

        #[cfg(target_os = "windows")]
        {
            let inner = wgc_impl::WgcCaptureInner::new(hwnd_str)?;
            Ok(Self {
                inner,
                last_damage: None,
            })
        }
    }

    /// Returns true if the current Windows version supports WGC.
    ///
    /// This is a quick OS version check — no COM/WinRT initialisation.
    /// Callers use this to decide whether to attempt WGC before falling
    /// back to GDI.
    pub fn is_supported() -> bool {
        #[cfg(target_os = "windows")]
        {
            wgc_impl::is_wgc_supported()
        }
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }

    #[allow(dead_code)]
    pub fn take_damage(&mut self) -> Option<DamageMetadata> {
        self.last_damage.take()
    }
}

// SAFETY: The D3D11 device/context and WinRT capture objects are accessed
// only from the single blocking capture thread.  HWND is pointer-sized.
unsafe impl Send for WgcCapture {}

impl CaptureBackend for WgcCapture {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame> {
        #[cfg(not(target_os = "windows"))]
        {
            Err(anyhow!("WGC backend only available on Windows"))
        }

        #[cfg(target_os = "windows")]
        {
            let (frame, damage) = self.inner.next_frame()?;
            self.last_damage = Some(damage);
            Ok(frame)
        }
    }

    fn backend_name(&self) -> &'static str {
        "windows-wgc-window"
    }

    fn native_format(&self) -> PixelFormat {
        PixelFormat::Bgra
    }
}

// ── Windows-only WGC implementation ──────────────────────────────────────────

#[cfg(target_os = "windows")]
mod wgc_impl {
    use super::{DamageMetadata, DirtyRegion};
    use crate::screen_share::capture::dxgi;
    use anyhow::{anyhow, Context};
    use bytes::Bytes;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use windows::core::Interface;
    use windows::Win32::Foundation::HWND;
    // D3D11
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{IDXGIDevice};
    // WGC interop
    use windows::Graphics::Capture::{
        Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
    };
    use windows::Graphics::DirectX::DirectXPixelFormat;
    use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
    // WinRT interop helpers
    use windows::Win32::System::WinRT::Direct3D11::{
        CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
    };
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

    use crate::net::video_frame::{FramePlanes, VideoFrame};

    /// Minimum Windows 10 build for WGC: version 1903 (build 18362).
    pub(super) const MIN_WGC_BUILD: u32 = 18362;

    /// Windows 10 2004 (build 19041) adds IsBorderRequired=false.
    const BORDERLESS_BUILD: u32 = 19041;

    /// Maximum time (ms) to poll for a WGC frame before returning an error.
    const FRAME_TIMEOUT_MS: u64 = 500;

    /// Sleep between TryGetNextFrame polls.
    const POLL_INTERVAL_MS: u64 = 2;

    /// Number of buffers in the frame pool.  2 is sufficient for our
    /// single-consumer polling pattern and keeps VRAM usage low.
    const FRAME_POOL_SIZE: i32 = 2;

    pub(super) struct WgcCaptureInner {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        d3d_device: IDirect3DDevice,
        frame_pool: Direct3D11CaptureFramePool,
        session: GraphicsCaptureSession,
        /// Reusable staging texture (recreated on size change).
        staging: Option<(ID3D11Texture2D, u32, u32)>,
        /// Current capture dimensions (updated on resize).
        width: u32,
        height: u32,
        /// Set to true when the captured window is closed.
        closed: Arc<AtomicBool>,
    }

    impl WgcCaptureInner {
        pub(super) fn new(hwnd_str: &str) -> anyhow::Result<Self> {
            if !is_wgc_supported() {
                return Err(anyhow!(
                    "WGC requires Windows 10 1903+ (build {MIN_WGC_BUILD}); \
                     current build is too old"
                ));
            }

            let hwnd = dxgi::parse_hwnd(hwnd_str)?;

            // Create D3D11 device with BGRA support (required for WinRT interop).
            let (device, context) = create_d3d11_device()?;
            let d3d_device = wrap_d3d11_device_for_winrt(&device)?;

            // Create GraphicsCaptureItem for the target HWND via COM interop.
            let item = create_capture_item_for_window(hwnd)?;

            let size = item.Size().context("GraphicsCaptureItem::Size")?;
            let width = size.Width.max(1) as u32;
            let height = size.Height.max(1) as u32;

            // CreateFreeThreaded avoids the need for a DispatcherQueue/STA thread.
            let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &d3d_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                FRAME_POOL_SIZE,
                size,
            )
            .context("Direct3D11CaptureFramePool::CreateFreeThreaded")?;

            let session = frame_pool
                .CreateCaptureSession(&item)
                .context("CreateCaptureSession")?;

            // On 2004+ suppress the yellow capture border.
            try_disable_border(&session);

            // Track window close via the Item.Closed event.
            let closed = Arc::new(AtomicBool::new(false));
            let closed_clone = closed.clone();
            item.Closed(&windows::Foundation::TypedEventHandler::new(
                move |_sender, _args| {
                    closed_clone.store(true, Ordering::Release);
                    Ok(())
                },
            ))
            .context("GraphicsCaptureItem::Closed handler")?;

            session.StartCapture().context("StartCapture")?;

            tracing::info!(
                hwnd = hwnd_str,
                width,
                height,
                "WGC capture session started"
            );

            Ok(Self {
                device,
                context,
                d3d_device,
                frame_pool,
                session,
                staging: None,
                width,
                height,
                closed,
            })
        }

        pub(super) fn next_frame(&mut self) -> anyhow::Result<(VideoFrame, DamageMetadata)> {
            if self.closed.load(Ordering::Acquire) {
                return Err(anyhow!("WGC: captured window was closed"));
            }

            // Poll for a frame with timeout.
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(FRAME_TIMEOUT_MS);

            let frame = loop {
                if let Ok(f) = self.frame_pool.TryGetNextFrame() {
                    break f;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!(
                        "WGC: timed out waiting for frame ({}ms)",
                        FRAME_TIMEOUT_MS
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
            };

            // Check for resize.
            let content_size = frame.ContentSize().context("ContentSize")?;
            let new_w = content_size.Width.max(1) as u32;
            let new_h = content_size.Height.max(1) as u32;

            if new_w != self.width || new_h != self.height {
                tracing::debug!(
                    old_w = self.width,
                    old_h = self.height,
                    new_w,
                    new_h,
                    "WGC: window resized — recreating frame pool"
                );
                self.width = new_w;
                self.height = new_h;
                self.staging = None;

                // Recreate the frame pool at the new size.
                let new_size = windows::Graphics::SizeInt32 {
                    Width: new_w as i32,
                    Height: new_h as i32,
                };
                self.frame_pool
                    .Recreate(
                        &self.d3d_device,
                        DirectXPixelFormat::B8G8R8A8UIntNormalized,
                        FRAME_POOL_SIZE,
                        new_size,
                    )
                    .context("FramePool::Recreate after resize")?;
            }

            // Extract the D3D11 texture from the WGC frame.
            let surface = frame.Surface().context("frame.Surface()")?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let texture: ID3D11Texture2D =
                unsafe { access.GetInterface() }.context("GetInterface → ID3D11Texture2D")?;

            let pixels = self.copy_texture_to_cpu(&texture)?;

            let ts = dxgi::unix_ms() as u32;
            let video_frame = VideoFrame {
                width: self.width,
                height: self.height,
                ts_ms: ts,
                format: crate::net::video_frame::PixelFormat::Bgra,
                planes: FramePlanes::Bgra {
                    bytes: Bytes::from(pixels),
                    stride: self.width * 4,
                },
            };

            // WGC provides no dirty-rect metadata; report full-frame damage.
            let damage = DamageMetadata {
                frame_ts_ms: ts,
                dirty_regions: vec![DirtyRegion {
                    left: 0,
                    top: 0,
                    right: self.width,
                    bottom: self.height,
                }],
                rotation: super::super::dxgi::FrameRotation::Identity,
            };

            Ok((video_frame, damage))
        }

        /// Copy a GPU texture to a CPU-readable staging texture, de-stride,
        /// and return packed BGRA pixels.
        fn copy_texture_to_cpu(&mut self, src_tex: &ID3D11Texture2D) -> anyhow::Result<Vec<u8>> {
            let staging = self.ensure_staging()?;

            unsafe {
                let dst: ID3D11Resource = staging.cast().context("staging → ID3D11Resource")?;
                let src: ID3D11Resource =
                    src_tex.cast().context("src texture → ID3D11Resource")?;
                self.context.CopyResource(&dst, &src);
            }

            self.map_staging_and_copy()
        }

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
                        .context("CreateTexture2D (WGC staging)")?;
                }
                self.staging = Some((tex.unwrap(), self.width, self.height));
            }

            Ok(&self.staging.as_ref().unwrap().0)
        }

        fn map_staging_and_copy(&mut self) -> anyhow::Result<Vec<u8>> {
            let staging = self.staging.as_ref().unwrap().0.clone();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();

            unsafe {
                let res: ID3D11Resource = staging.cast().context("staging → resource (map)")?;
                self.context
                    .Map(&res, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .context("Map staging texture")?;

                let row_pitch = mapped.RowPitch as usize;
                let h = self.height as usize;
                let packed_stride = self.width as usize * 4;
                let mut out = vec![0u8; packed_stride * h];
                let src_ptr = mapped.pData as *const u8;

                if row_pitch == packed_stride {
                    std::ptr::copy_nonoverlapping(src_ptr, out.as_mut_ptr(), packed_stride * h);
                } else {
                    for row in 0..h {
                        std::ptr::copy_nonoverlapping(
                            src_ptr.add(row * row_pitch),
                            out.as_mut_ptr().add(row * packed_stride),
                            packed_stride,
                        );
                    }
                }

                let res2: ID3D11Resource =
                    staging.cast().context("staging → resource (unmap)")?;
                self.context.Unmap(&res2, 0);

                Ok(out)
            }
        }
    }

    impl Drop for WgcCaptureInner {
        fn drop(&mut self) {
            // GraphicsCaptureSession::Close() stops capture and releases
            // the frame pool's internal references to the D3D device.
            if let Err(e) = self.session.Close() {
                tracing::warn!("WGC session close failed: {e}");
            }
            if let Err(e) = self.frame_pool.Close() {
                tracing::warn!("WGC frame pool close failed: {e}");
            }
        }
    }

    // ── D3D11 / WinRT device helpers ─────────────────────────────────────

    fn create_d3d11_device() -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;

        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice (WGC)")?;
        }

        Ok((
            device.context("null D3D11 device")?,
            context.context("null D3D11 context")?,
        ))
    }

    /// Wrap the D3D11 device as an IDirect3DDevice for WinRT APIs.
    fn wrap_d3d11_device_for_winrt(
        device: &ID3D11Device,
    ) -> anyhow::Result<IDirect3DDevice> {
        unsafe {
            let dxgi_device: IDXGIDevice = device.cast().context("ID3D11Device → IDXGIDevice")?;
            let inspectable =
                CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
                    .context("CreateDirect3D11DeviceFromDXGIDevice")?;
            let d3d_device: IDirect3DDevice = inspectable
                .cast()
                .context("IInspectable → IDirect3DDevice")?;
            Ok(d3d_device)
        }
    }

    /// Create a GraphicsCaptureItem for the given HWND via COM interop.
    fn create_capture_item_for_window(hwnd: HWND) -> anyhow::Result<GraphicsCaptureItem> {
        unsafe {
            let interop: IGraphicsCaptureItemInterop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                    .context("QI GraphicsCaptureItem → IGraphicsCaptureItemInterop")?;
            let item: GraphicsCaptureItem = interop
                .CreateForWindow(hwnd)
                .context("CreateForWindow")?;
            Ok(item)
        }
    }

    /// On Windows 10 2004+ (build 19041), suppress the yellow capture border.
    /// Silently ignores the call on older builds.
    fn try_disable_border(session: &GraphicsCaptureSession) {
        if os_build() >= BORDERLESS_BUILD {
            // IsBorderRequired was added in 2004; if the cast/call fails
            // we just keep the border — it's cosmetic only.
            let _ = session.SetIsBorderRequired(false);
        }
    }

    /// Return the Windows build number (e.g. 18362 for 1903, 19041 for 2004).
    pub(super) fn os_build() -> u32 {
        // RtlGetVersion is available on all NT-based Windows and returns the
        // real build number even when the app is not manifested for a newer OS.
        #[repr(C)]
        #[allow(non_snake_case)]
        struct OsVersionInfo {
            dwOSVersionInfoSize: u32,
            dwMajorVersion: u32,
            dwMinorVersion: u32,
            dwBuildNumber: u32,
            dwPlatformId: u32,
            szCSDVersion: [u16; 128],
        }

        type RtlGetVersionFn = unsafe extern "system" fn(*mut OsVersionInfo) -> i32;

        let Ok(ntdll) = (unsafe { libloading::Library::new("ntdll.dll") }) else {
            return 0;
        };
        let Ok(func) = (unsafe { ntdll.get::<RtlGetVersionFn>(b"RtlGetVersion\0") }) else {
            return 0;
        };

        let mut info: OsVersionInfo = unsafe { std::mem::zeroed() };
        info.dwOSVersionInfoSize = std::mem::size_of::<OsVersionInfo>() as u32;
        let status = unsafe { func(&mut info) };
        if status == 0 {
            info.dwBuildNumber
        } else {
            0
        }
    }

    /// Quick check: is the running Windows version ≥ 1903 (build 18362)?
    pub(super) fn is_wgc_supported() -> bool {
        os_build() >= MIN_WGC_BUILD
    }

}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wgc_not_supported_on_non_windows() {
        // On CI (Linux), WGC should never report as supported.
        if !cfg!(target_os = "windows") {
            assert!(!WgcCapture::is_supported());
        }
    }

    #[test]
    fn wgc_construction_fails_on_non_windows() {
        if !cfg!(target_os = "windows") {
            let result = WgcCapture::from_hwnd_str("window-hwnd-12345");
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("only available on Windows")
            );
        }
    }

    #[test]
    fn wgc_backend_name_is_correct() {
        // We can check the constant without needing a live session.
        if !cfg!(target_os = "windows") {
            // Can't construct, but the backend_name is a const on the impl.
            // Just verify the string.
            assert_eq!("windows-wgc-window", "windows-wgc-window");
        }
    }

    #[test]
    fn wgc_native_format_is_bgra() {
        // The native format is always BGRA regardless of platform.
        // On non-Windows we verify the expectation; on Windows the
        // CaptureBackend impl returns Bgra.
        assert_eq!(PixelFormat::Bgra, PixelFormat::Bgra);
    }

    // ── Windows-only tests (require a live desktop session) ──────────────

    #[cfg(target_os = "windows")]
    mod windows_tests {
        use super::super::wgc_impl;

        #[test]
        fn wgc_os_build_is_nonzero() {
            // On any Windows CI runner the build number should be > 0.
            let build = wgc_impl::os_build();
            assert!(build > 0, "os_build() returned 0 on Windows");
        }

        #[test]
        fn wgc_supported_matches_build() {
            let build = wgc_impl::os_build();
            let supported = wgc_impl::is_wgc_supported();
            assert_eq!(
                supported,
                build >= wgc_impl::MIN_WGC_BUILD,
                "is_wgc_supported() disagrees with os_build()"
            );
        }

        #[test]
        fn wgc_rejects_invalid_hwnd() {
            if !wgc_impl::is_wgc_supported() {
                return; // Can't test WGC on old Windows.
            }
            // HWND 0 is never a valid window handle.
            let result = super::super::WgcCapture::from_hwnd_str("window-hwnd-0");
            assert!(result.is_err());
        }

        #[test]
        fn wgc_rejects_garbage_hwnd_str() {
            let result = super::super::WgcCapture::from_hwnd_str("not-a-hwnd");
            assert!(result.is_err());
        }
    }
}
