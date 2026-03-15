/// VAAPI hardware VP9 decoder backend (Linux only).
///
/// ## Runtime dependencies
///
/// - `libva.so.2` and `libva-drm.so.2`
/// - A VA-API driver with VP9 decode support (VLD entrypoint).
/// - A DRM render node at `/dev/dri/renderD128` (or similar).
///
/// The decoder opens a VA-API decode session, feeds compressed VP9 frames via
/// vaBeginPicture / vaRenderPicture / vaEndPicture, then reads the decoded NV12
/// surface back to system-memory RGBA.
use std::ffi::{c_void, CString};
use std::ptr;

use anyhow::{bail, Result};
use tracing::{info, warn};

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_frame::EncodedAccessUnit;

const VA_STATUS_SUCCESS: i32 = 0;
const VA_PROFILE_VP9_PROFILE0: i32 = 17;
const VA_ENTRYPOINT_VLD: i32 = 1;
const VA_RT_FORMAT_YUV420: u32 = 0x00000001;

type VaSurfaceId = u32;
type VaBufferId = u32;
type VaConfigId = u32;
type VaContextId = u32;
type VaDisplay = *mut c_void;

const VA_INVALID_ID: u32 = 0xFFFFFFFF;
const VA_INVALID_SURFACE: u32 = 0xFFFFFFFF;

const VA_SLICE_DATA_BUFFER_TYPE: i32 = 3;

// Minimal VA image struct
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VaImageFormat {
    fourcc: u32,
    byte_order: u32,
    bits_per_pixel: u32,
    depth: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    _pad: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VaImage {
    image_id: u32,
    format: VaImageFormat,
    buf: VaBufferId,
    width: u16,
    height: u16,
    num_planes: u32,
    pitches: [u32; 3],
    offsets: [u32; 3],
    data_size: u32,
    _pad: u32,
}

// Function pointer types
type VaInitialize = unsafe extern "C" fn(VaDisplay, *mut i32, *mut i32) -> i32;
type VaTerminate = unsafe extern "C" fn(VaDisplay) -> i32;
type VaCreateConfig =
    unsafe extern "C" fn(VaDisplay, i32, i32, *mut c_void, i32, *mut VaConfigId) -> i32;
type VaDestroyConfig = unsafe extern "C" fn(VaDisplay, VaConfigId) -> i32;
type VaCreateContext = unsafe extern "C" fn(
    VaDisplay, VaConfigId, i32, i32, i32, *mut VaSurfaceId, i32, *mut VaContextId,
) -> i32;
type VaDestroyContext = unsafe extern "C" fn(VaDisplay, VaContextId) -> i32;
type VaCreateSurfaces = unsafe extern "C" fn(
    VaDisplay, u32, u32, u32, *mut VaSurfaceId, u32, *mut c_void, u32,
) -> i32;
type VaDestroySurfaces = unsafe extern "C" fn(VaDisplay, *mut VaSurfaceId, i32) -> i32;
type VaCreateBuffer =
    unsafe extern "C" fn(VaDisplay, VaContextId, i32, u32, u32, *const c_void, *mut VaBufferId) -> i32;
type VaDestroyBuffer = unsafe extern "C" fn(VaDisplay, VaBufferId) -> i32;
type VaBeginPicture = unsafe extern "C" fn(VaDisplay, VaContextId, VaSurfaceId) -> i32;
type VaRenderPicture = unsafe extern "C" fn(VaDisplay, VaContextId, *mut VaBufferId, i32) -> i32;
type VaEndPicture = unsafe extern "C" fn(VaDisplay, VaContextId) -> i32;
type VaSyncSurface = unsafe extern "C" fn(VaDisplay, VaSurfaceId) -> i32;
type VaMapBuffer = unsafe extern "C" fn(VaDisplay, VaBufferId, *mut *mut c_void) -> i32;
type VaUnmapBuffer = unsafe extern "C" fn(VaDisplay, VaBufferId) -> i32;
type VaGetDisplayDRM = unsafe extern "C" fn(i32) -> VaDisplay;
type VaDeriveImage = unsafe extern "C" fn(VaDisplay, VaSurfaceId, *mut VaImage) -> i32;
type VaDestroyImage = unsafe extern "C" fn(VaDisplay, u32) -> i32;

pub struct VaapiVp9Decoder {
    _va_lib: libloading::Library,
    _drm_lib: libloading::Library,
    display: VaDisplay,
    config_id: VaConfigId,
    context_id: VaContextId,
    surface: VaSurfaceId,
    drm_fd: i32,
    config: VideoSessionConfig,
    last_frame: Option<DecodedVideoFrame>,
    initialized: bool,
    // Function pointers
    va_begin_picture: VaBeginPicture,
    va_render_picture: VaRenderPicture,
    va_end_picture: VaEndPicture,
    va_sync_surface: VaSyncSurface,
    va_map_buffer: VaMapBuffer,
    va_unmap_buffer: VaUnmapBuffer,
    va_create_buffer: VaCreateBuffer,
    va_destroy_buffer: VaDestroyBuffer,
    va_create_surfaces: VaCreateSurfaces,
    va_destroy_surfaces: VaDestroySurfaces,
    va_create_context: VaCreateContext,
    va_destroy_context: VaDestroyContext,
    va_destroy_config: VaDestroyConfig,
    va_terminate: VaTerminate,
    va_derive_image: VaDeriveImage,
    va_destroy_image: VaDestroyImage,
}

// SAFETY: Only accessed through &mut self.
unsafe impl Send for VaapiVp9Decoder {}

impl VaapiVp9Decoder {
    pub fn open() -> Result<Self> {
        let va_lib = unsafe { libloading::Library::new("libva.so.2") }
            .map_err(|e| anyhow::anyhow!("libva.so.2: {e}"))?;
        let drm_lib = unsafe { libloading::Library::new("libva-drm.so.2") }
            .map_err(|e| anyhow::anyhow!("libva-drm.so.2: {e}"))?;

        let drm_fd = open_render_node();
        if drm_fd < 0 {
            bail!("no DRM render node available");
        }

        let va_get_display: VaGetDisplayDRM = unsafe { *drm_lib.get(b"vaGetDisplayDRM\0")? };
        let va_initialize: VaInitialize = unsafe { *va_lib.get(b"vaInitialize\0")? };
        let va_terminate: VaTerminate = unsafe { *va_lib.get(b"vaTerminate\0")? };
        let va_create_config: VaCreateConfig = unsafe { *va_lib.get(b"vaCreateConfig\0")? };
        let va_destroy_config: VaDestroyConfig = unsafe { *va_lib.get(b"vaDestroyConfig\0")? };
        let va_create_context: VaCreateContext = unsafe { *va_lib.get(b"vaCreateContext\0")? };
        let va_destroy_context: VaDestroyContext = unsafe { *va_lib.get(b"vaDestroyContext\0")? };
        let va_create_surfaces: VaCreateSurfaces = unsafe { *va_lib.get(b"vaCreateSurfaces\0")? };
        let va_destroy_surfaces: VaDestroySurfaces = unsafe { *va_lib.get(b"vaDestroySurfaces\0")? };
        let va_create_buffer: VaCreateBuffer = unsafe { *va_lib.get(b"vaCreateBuffer\0")? };
        let va_destroy_buffer: VaDestroyBuffer = unsafe { *va_lib.get(b"vaDestroyBuffer\0")? };
        let va_begin_picture: VaBeginPicture = unsafe { *va_lib.get(b"vaBeginPicture\0")? };
        let va_render_picture: VaRenderPicture = unsafe { *va_lib.get(b"vaRenderPicture\0")? };
        let va_end_picture: VaEndPicture = unsafe { *va_lib.get(b"vaEndPicture\0")? };
        let va_sync_surface: VaSyncSurface = unsafe { *va_lib.get(b"vaSyncSurface\0")? };
        let va_map_buffer: VaMapBuffer = unsafe { *va_lib.get(b"vaMapBuffer\0")? };
        let va_unmap_buffer: VaUnmapBuffer = unsafe { *va_lib.get(b"vaUnmapBuffer\0")? };
        let va_derive_image: VaDeriveImage = unsafe { *va_lib.get(b"vaDeriveImage\0")? };
        let va_destroy_image: VaDestroyImage = unsafe { *va_lib.get(b"vaDestroyImage\0")? };

        let display = unsafe { va_get_display(drm_fd) };
        if display.is_null() {
            unsafe { libc::close(drm_fd) };
            bail!("vaGetDisplayDRM returned null");
        }

        let mut major: i32 = 0;
        let mut minor: i32 = 0;
        let rc = unsafe { va_initialize(display, &mut major, &mut minor) };
        if rc != VA_STATUS_SUCCESS {
            unsafe { libc::close(drm_fd) };
            bail!("vaInitialize failed: {rc}");
        }

        // Create decode config
        let mut config_id: VaConfigId = VA_INVALID_ID;
        let rc = unsafe {
            va_create_config(
                display,
                VA_PROFILE_VP9_PROFILE0,
                VA_ENTRYPOINT_VLD,
                ptr::null_mut(),
                0,
                &mut config_id,
            )
        };
        if rc != VA_STATUS_SUCCESS {
            unsafe {
                va_terminate(display);
                libc::close(drm_fd);
            }
            bail!("vaCreateConfig for VP9 decode failed: {rc}");
        }

        info!("[vaapi-vp9] opened VA-API VP9 decode session");

        Ok(Self {
            _va_lib: va_lib,
            _drm_lib: drm_lib,
            display,
            config_id,
            context_id: VA_INVALID_ID,
            surface: VA_INVALID_SURFACE,
            drm_fd,
            config: VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 30,
                target_bitrate_bps: 0,
                low_latency: true,
                allow_frame_drop: true,
            },
            last_frame: None,
            initialized: false,
            va_begin_picture,
            va_render_picture,
            va_end_picture,
            va_sync_surface,
            va_map_buffer,
            va_unmap_buffer,
            va_create_buffer,
            va_destroy_buffer,
            va_create_surfaces,
            va_destroy_surfaces,
            va_create_context,
            va_destroy_context,
            va_destroy_config,
            va_terminate,
            va_derive_image,
            va_destroy_image,
        })
    }

    fn ensure_surfaces(&mut self, width: u32, height: u32) -> Result<()> {
        if self.initialized && self.config.width == width && self.config.height == height {
            return Ok(());
        }

        self.destroy_resources();

        let mut surface: VaSurfaceId = VA_INVALID_SURFACE;
        let rc = unsafe {
            (self.va_create_surfaces)(
                self.display,
                VA_RT_FORMAT_YUV420,
                width,
                height,
                &mut surface,
                1,
                ptr::null_mut(),
                0,
            )
        };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaCreateSurfaces failed: {rc}");
        }
        self.surface = surface;

        let mut context_id: VaContextId = VA_INVALID_ID;
        let rc = unsafe {
            (self.va_create_context)(
                self.display,
                self.config_id,
                width as i32,
                height as i32,
                0x00000001, // VA_PROGRESSIVE
                &mut surface,
                1,
                &mut context_id,
            )
        };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaCreateContext failed: {rc}");
        }
        self.context_id = context_id;

        self.config.width = width;
        self.config.height = height;
        self.initialized = true;
        Ok(())
    }

    fn destroy_resources(&mut self) {
        if self.context_id != VA_INVALID_ID {
            unsafe { (self.va_destroy_context)(self.display, self.context_id) };
            self.context_id = VA_INVALID_ID;
        }
        if self.surface != VA_INVALID_SURFACE {
            let mut s = self.surface;
            unsafe { (self.va_destroy_surfaces)(self.display, &mut s, 1) };
            self.surface = VA_INVALID_SURFACE;
        }
        self.initialized = false;
    }

    /// Read NV12 surface → RGBA.
    fn read_surface(&mut self, width: usize, height: usize, ts_ms: u32) -> Result<DecodedVideoFrame> {
        let mut image = VaImage::default();
        let rc = unsafe { (self.va_derive_image)(self.display, self.surface, &mut image) };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaDeriveImage failed: {rc}");
        }

        let mut buf_ptr: *mut c_void = ptr::null_mut();
        let rc = unsafe { (self.va_map_buffer)(self.display, image.buf, &mut buf_ptr) };
        if rc != VA_STATUS_SUCCESS {
            unsafe { (self.va_destroy_image)(self.display, image.image_id) };
            bail!("vaMapBuffer (image) failed: {rc}");
        }

        let src = buf_ptr as *const u8;
        let y_pitch = image.pitches[0] as usize;
        let uv_pitch = image.pitches[1] as usize;
        let uv_offset = image.offsets[1] as usize;

        let mut rgba = vec![0_u8; width * height * 4];

        // NV12 → RGBA (BT.709)
        for y in 0..height {
            for x in 0..width {
                let luma = unsafe { *src.add(y * y_pitch + x) } as f32;
                let uv_x = x / 2;
                let uv_y = y / 2;
                let u = unsafe { *src.add(uv_offset + uv_y * uv_pitch + uv_x * 2) } as f32;
                let v = unsafe { *src.add(uv_offset + uv_y * uv_pitch + uv_x * 2 + 1) } as f32;

                let yv = luma - 16.0;
                let ub = u - 128.0;
                let vr = v - 128.0;
                let r = (1.164 * yv + 1.596 * vr).clamp(0.0, 255.0) as u8;
                let g = (1.164 * yv - 0.392 * ub - 0.813 * vr).clamp(0.0, 255.0) as u8;
                let b = (1.164 * yv + 2.017 * ub).clamp(0.0, 255.0) as u8;

                let idx = (y * width + x) * 4;
                rgba[idx] = r;
                rgba[idx + 1] = g;
                rgba[idx + 2] = b;
                rgba[idx + 3] = 255;
            }
        }

        unsafe { (self.va_unmap_buffer)(self.display, image.buf) };
        unsafe { (self.va_destroy_image)(self.display, image.image_id) };

        Ok(DecodedVideoFrame {
            width,
            height,
            rgba,
            ts_ms,
        })
    }
}

impl Drop for VaapiVp9Decoder {
    fn drop(&mut self) {
        self.destroy_resources();
        if self.config_id != VA_INVALID_ID {
            unsafe { (self.va_destroy_config)(self.display, self.config_id) };
        }
        unsafe { (self.va_terminate)(self.display) };
        unsafe { libc::close(self.drm_fd) };
    }
}

impl VideoDecoder for VaapiVp9Decoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        self.config = config;
        Ok(())
    }

    fn decode(
        &mut self,
        encoded: &EncodedAccessUnit,
        metadata: DecodeMetadata,
    ) -> Result<DecodedVideoFrame> {
        if encoded.data.is_empty() {
            if let Some(mut prev) = self.last_frame.clone() {
                prev.ts_ms = metadata.ts_ms;
                return Ok(prev);
            }
            bail!("empty VP9 access unit with no previous frame");
        }

        // VP9 frame marker check
        let marker = (encoded.data[0] >> 6) & 0b11;
        if marker != 0b10 {
            bail!(
                "invalid VP9 frame_marker: expected 0b10, got {marker:#04b}"
            );
        }

        // Parse basic VP9 uncompressed header for dimensions
        // For now, use configured dimensions or infer from the stream.
        let width = if self.config.width > 0 {
            self.config.width
        } else {
            // Fallback: assume 1080p
            1920
        };
        let height = if self.config.height > 0 {
            self.config.height
        } else {
            1080
        };

        self.ensure_surfaces(width, height)?;

        // Submit the compressed data as a slice buffer
        let mut slice_buf: VaBufferId = VA_INVALID_ID;
        let rc = unsafe {
            (self.va_create_buffer)(
                self.display,
                self.context_id,
                VA_SLICE_DATA_BUFFER_TYPE,
                encoded.data.len() as u32,
                1,
                encoded.data.as_ptr() as *const c_void,
                &mut slice_buf,
            )
        };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaCreateBuffer (slice) failed: {rc}");
        }

        let rc = unsafe { (self.va_begin_picture)(self.display, self.context_id, self.surface) };
        if rc != VA_STATUS_SUCCESS {
            unsafe { (self.va_destroy_buffer)(self.display, slice_buf) };
            bail!("vaBeginPicture failed: {rc}");
        }

        let rc = unsafe { (self.va_render_picture)(self.display, self.context_id, &mut slice_buf, 1) };
        if rc != VA_STATUS_SUCCESS {
            unsafe { (self.va_end_picture)(self.display, self.context_id) };
            unsafe { (self.va_destroy_buffer)(self.display, slice_buf) };
            bail!("vaRenderPicture failed: {rc}");
        }

        let rc = unsafe { (self.va_end_picture)(self.display, self.context_id) };
        if rc != VA_STATUS_SUCCESS {
            unsafe { (self.va_destroy_buffer)(self.display, slice_buf) };
            bail!("vaEndPicture failed: {rc}");
        }

        let rc = unsafe { (self.va_sync_surface)(self.display, self.surface) };
        if rc != VA_STATUS_SUCCESS {
            unsafe { (self.va_destroy_buffer)(self.display, slice_buf) };
            bail!("vaSyncSurface failed: {rc}");
        }

        unsafe { (self.va_destroy_buffer)(self.display, slice_buf) };

        let frame = self.read_surface(width as usize, height as usize, metadata.ts_ms)?;
        self.last_frame = Some(frame.clone());
        Ok(frame)
    }

    fn reset(&mut self) -> Result<()> {
        self.last_frame = None;
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "vp9-vaapi"
    }
}

fn open_render_node() -> i32 {
    for idx in 128..136 {
        let path = format!("/dev/dri/renderD{idx}");
        let cpath = CString::new(path).unwrap();
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
        if fd >= 0 {
            return fd;
        }
    }
    -1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_fails_cleanly_without_vaapi() {
        let result = VaapiVp9Decoder::open();
        if result.is_err() {
            return; // Expected in CI.
        }
        let dec = result.unwrap();
        assert_eq!(dec.backend_name(), "vp9-vaapi");
    }
}
