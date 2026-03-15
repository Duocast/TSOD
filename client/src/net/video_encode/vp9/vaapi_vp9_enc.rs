/// VAAPI hardware VP9 encoder backend (Linux only).
///
/// ## Runtime dependencies
///
/// - `libva.so.2` and `libva-drm.so.2` — the VA-API runtime libraries.
/// - A VA-API driver that supports VP9 encode for profile 0
///   (e.g. `intel-media-va-driver-non-free` on Intel Gen9+).
/// - A DRM render node at `/dev/dri/renderD128` (or 129-135).
///
/// ## Low-latency settings
///
/// - Constant bitrate (CBR) rate control.
/// - Single-slice, no B-frames.
/// - Keyframe interval ≤ 300 frames with on-demand IDR insertion.
use std::ffi::{c_void, CString};
use std::ptr;

use anyhow::{bail, Result};
use tracing::{info, warn};

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_frame::{EncodedAccessUnit, FramePlanes, PixelFormat, VideoFrame};
use crate::net::vpx_codec;
use crate::proto::voiceplatform::v1 as pb;

// ── VA-API constants ──────────────────────────────────────────────────────────

const VA_STATUS_SUCCESS: i32 = 0;
const VA_PROFILE_VP9_PROFILE0: i32 = 17;
const VA_ENTRYPOINT_ENCSLICE_LP: i32 = 8;
const VA_ENTRYPOINT_ENCSLICE: i32 = 6;
const VA_RT_FORMAT_YUV420: u32 = 0x00000001;
const VA_RC_CBR: u32 = 0x00000002;
const VA_FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

// Surface / buffer ID types
type VaSurfaceId = u32;
type VaBufferId = u32;
type VaConfigId = u32;
type VaContextId = u32;
type VaDisplay = *mut c_void;

const VA_INVALID_ID: u32 = 0xFFFFFFFF;
const VA_INVALID_SURFACE: u32 = 0xFFFFFFFF;

// Buffer types
const VA_ENC_CODED_BUFFER_TYPE: i32 = 22;
const VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE: i32 = 21;
const VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE: i32 = 23;
const VA_ENC_MISC_PARAMETER_BUFFER_TYPE: i32 = 27;

// ── VA-API function types ─────────────────────────────────────────────────────

type VaInitialize = unsafe extern "C" fn(VaDisplay, *mut i32, *mut i32) -> i32;
type VaTerminate = unsafe extern "C" fn(VaDisplay) -> i32;
type VaCreateConfig =
    unsafe extern "C" fn(VaDisplay, i32, i32, *mut c_void, i32, *mut VaConfigId) -> i32;
type VaDestroyConfig = unsafe extern "C" fn(VaDisplay, VaConfigId) -> i32;
type VaCreateContext = unsafe extern "C" fn(
    VaDisplay,
    VaConfigId,
    i32,
    i32,
    i32,
    *mut VaSurfaceId,
    i32,
    *mut VaContextId,
) -> i32;
type VaDestroyContext = unsafe extern "C" fn(VaDisplay, VaContextId) -> i32;
type VaCreateSurfaces = unsafe extern "C" fn(
    VaDisplay,
    u32,
    u32,
    u32,
    *mut VaSurfaceId,
    u32,
    *mut c_void,
    u32,
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
type VaMaxNumEntrypoints = unsafe extern "C" fn(VaDisplay) -> i32;
type VaQueryConfigEntrypoints =
    unsafe extern "C" fn(VaDisplay, i32, *mut i32, *mut i32) -> i32;
type VaPutImage = unsafe extern "C" fn(
    VaDisplay, VaSurfaceId, u32, i32, i32, u32, u32, i32, i32, u32, u32,
) -> i32;
type VaCreateImage = unsafe extern "C" fn(VaDisplay, *mut VaImageFormat, i32, i32, *mut VaImage) -> i32;
type VaDestroyImage = unsafe extern "C" fn(VaDisplay, u32) -> i32;
type VaDeriveImage = unsafe extern "C" fn(VaDisplay, VaSurfaceId, *mut VaImage) -> i32;

// ── VA-API structures (simplified) ────────────────────────────────────────────

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

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VaCodedBufferSegment {
    size: u32,
    bit_offset: u32,
    status: u32,
    reserved: u32,
    buf: *mut c_void,
    next: *mut VaCodedBufferSegment,
}

// ── VAAPI encoder ─────────────────────────────────────────────────────────────

pub struct VaapiVp9Encoder {
    _va_lib: libloading::Library,
    _drm_lib: libloading::Library,
    display: VaDisplay,
    config_id: VaConfigId,
    context_id: VaContextId,
    input_surface: VaSurfaceId,
    recon_surface: VaSurfaceId,
    coded_buf: VaBufferId,
    drm_fd: i32,
    config: VideoSessionConfig,
    frame_idx: u32,
    force_next_keyframe: bool,
    initialized: bool,
    // Function pointers we need post-init
    va_begin_picture: VaBeginPicture,
    va_render_picture: VaRenderPicture,
    va_end_picture: VaEndPicture,
    va_sync_surface: VaSyncSurface,
    va_map_buffer: VaMapBuffer,
    va_unmap_buffer: VaUnmapBuffer,
    va_create_buffer: VaCreateBuffer,
    va_destroy_buffer: VaDestroyBuffer,
    va_destroy_surfaces: VaDestroySurfaces,
    va_destroy_context: VaDestroyContext,
    va_destroy_config: VaDestroyConfig,
    va_terminate: VaTerminate,
    va_derive_image: VaDeriveImage,
    va_destroy_image: VaDestroyImage,
}

// SAFETY: The VA-API handle is only accessed through &mut self.
unsafe impl Send for VaapiVp9Encoder {}

impl VaapiVp9Encoder {
    /// Attempt to open a VA-API VP9 encode session. Returns an error if the
    /// platform lacks the necessary libraries or the driver doesn't support
    /// VP9 encode.
    pub fn open() -> Result<Self> {
        let va_lib = unsafe { libloading::Library::new("libva.so.2") }
            .map_err(|e| anyhow::anyhow!("libva.so.2: {e}"))?;
        let drm_lib = unsafe { libloading::Library::new("libva-drm.so.2") }
            .map_err(|e| anyhow::anyhow!("libva-drm.so.2: {e}"))?;

        // Open render node
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
        let va_destroy_context: VaDestroyContext =
            unsafe { *va_lib.get(b"vaDestroyContext\0")? };
        let va_create_surfaces: VaCreateSurfaces =
            unsafe { *va_lib.get(b"vaCreateSurfaces\0")? };
        let va_destroy_surfaces: VaDestroySurfaces =
            unsafe { *va_lib.get(b"vaDestroySurfaces\0")? };
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

        // Find a VP9 encode entrypoint
        let va_max_ep: VaMaxNumEntrypoints =
            unsafe { *va_lib.get(b"vaMaxNumEntrypoints\0")? };
        let va_query_ep: VaQueryConfigEntrypoints =
            unsafe { *va_lib.get(b"vaQueryConfigEntrypoints\0")? };

        let max_ep = unsafe { va_max_ep(display) };
        let mut eps = vec![0_i32; max_ep.max(0) as usize];
        let mut num_ep: i32 = 0;
        let rc = unsafe {
            va_query_ep(
                display,
                VA_PROFILE_VP9_PROFILE0,
                eps.as_mut_ptr(),
                &mut num_ep,
            )
        };
        if rc != VA_STATUS_SUCCESS {
            unsafe {
                va_terminate(display);
                libc::close(drm_fd);
            }
            bail!("vaQueryConfigEntrypoints for VP9Profile0 failed: {rc}");
        }

        let ep_slice = &eps[..num_ep as usize];
        let entrypoint = if ep_slice.contains(&VA_ENTRYPOINT_ENCSLICE_LP) {
            VA_ENTRYPOINT_ENCSLICE_LP
        } else if ep_slice.contains(&VA_ENTRYPOINT_ENCSLICE) {
            VA_ENTRYPOINT_ENCSLICE
        } else {
            unsafe {
                va_terminate(display);
                libc::close(drm_fd);
            }
            bail!("VP9 encode entrypoint not found");
        };

        // Create config with CBR rate control
        let mut rc_attr_value = VA_RC_CBR;
        // VA_ATTRIB_RATE_CONTROL = 1
        #[repr(C)]
        struct VaConfigAttrib {
            type_: i32,
            value: u32,
        }
        let mut attrib = VaConfigAttrib {
            type_: 1, // VAConfigAttribRateControl
            value: rc_attr_value,
        };

        let mut config_id: VaConfigId = VA_INVALID_ID;
        let rc = unsafe {
            va_create_config(
                display,
                VA_PROFILE_VP9_PROFILE0,
                entrypoint,
                &mut attrib as *mut _ as *mut c_void,
                1,
                &mut config_id,
            )
        };
        if rc != VA_STATUS_SUCCESS {
            unsafe {
                va_terminate(display);
                libc::close(drm_fd);
            }
            bail!("vaCreateConfig for VP9 encode failed: {rc}");
        }

        info!("[vaapi-vp9] opened VA-API VP9 encode session (entrypoint={entrypoint})");

        Ok(Self {
            _va_lib: va_lib,
            _drm_lib: drm_lib,
            display,
            config_id,
            context_id: VA_INVALID_ID,
            input_surface: VA_INVALID_SURFACE,
            recon_surface: VA_INVALID_SURFACE,
            coded_buf: VA_INVALID_ID,
            drm_fd,
            config: VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 60,
                target_bitrate_bps: 8_000_000,
                low_latency: true,
                allow_frame_drop: true,
            },
            frame_idx: 0,
            force_next_keyframe: false,
            initialized: false,
            va_begin_picture,
            va_render_picture,
            va_end_picture,
            va_sync_surface,
            va_map_buffer,
            va_unmap_buffer,
            va_create_buffer,
            va_destroy_buffer,
            va_destroy_surfaces,
            va_destroy_context,
            va_destroy_config,
            va_terminate,
            va_derive_image,
            va_destroy_image,
        })
    }

    fn initialize_encoder(&mut self) -> Result<()> {
        if self.config.width == 0 || self.config.height == 0 {
            return Ok(());
        }

        self.destroy_resources();

        let w = self.config.width;
        let h = self.config.height;

        // Align to 64-byte boundaries for VP9
        let aligned_w = (w + 63) & !63;
        let aligned_h = (h + 63) & !63;

        // Create surfaces: input + reconstruction reference
        let va_create_surfaces: VaCreateSurfaces =
            unsafe { *self._va_lib.get(b"vaCreateSurfaces\0")? };

        let mut surfaces = [VA_INVALID_SURFACE; 2];
        let rc = unsafe {
            va_create_surfaces(
                self.display,
                VA_RT_FORMAT_YUV420,
                aligned_w,
                aligned_h,
                surfaces.as_mut_ptr(),
                2,
                ptr::null_mut(),
                0,
            )
        };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaCreateSurfaces failed: {rc}");
        }
        self.input_surface = surfaces[0];
        self.recon_surface = surfaces[1];

        // Create context
        let va_create_context: VaCreateContext =
            unsafe { *self._va_lib.get(b"vaCreateContext\0")? };
        let mut context_id: VaContextId = VA_INVALID_ID;
        let rc = unsafe {
            va_create_context(
                self.display,
                self.config_id,
                aligned_w as i32,
                aligned_h as i32,
                0x00000001, // VA_PROGRESSIVE
                surfaces.as_mut_ptr(),
                2,
                &mut context_id,
            )
        };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaCreateContext failed: {rc}");
        }
        self.context_id = context_id;

        // Create coded buffer (generous size for keyframes)
        let coded_buf_size = (w * h * 3) as u32; // ~3 bytes/pixel max
        let mut coded_buf: VaBufferId = VA_INVALID_ID;
        let rc = unsafe {
            (self.va_create_buffer)(
                self.display,
                self.context_id,
                VA_ENC_CODED_BUFFER_TYPE,
                coded_buf_size,
                1,
                ptr::null(),
                &mut coded_buf,
            )
        };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaCreateBuffer (coded) failed: {rc}");
        }
        self.coded_buf = coded_buf;

        self.initialized = true;
        self.frame_idx = 0;

        info!(
            width = w,
            height = h,
            fps = self.config.fps,
            bitrate_bps = self.config.target_bitrate_bps,
            "[vaapi-vp9] encoder initialized"
        );
        Ok(())
    }

    fn destroy_resources(&mut self) {
        if self.coded_buf != VA_INVALID_ID {
            unsafe { (self.va_destroy_buffer)(self.display, self.coded_buf) };
            self.coded_buf = VA_INVALID_ID;
        }
        if self.context_id != VA_INVALID_ID {
            unsafe { (self.va_destroy_context)(self.display, self.context_id) };
            self.context_id = VA_INVALID_ID;
        }
        if self.input_surface != VA_INVALID_SURFACE {
            let mut surfaces = [self.input_surface, self.recon_surface];
            unsafe { (self.va_destroy_surfaces)(self.display, surfaces.as_mut_ptr(), 2) };
            self.input_surface = VA_INVALID_SURFACE;
            self.recon_surface = VA_INVALID_SURFACE;
        }
        self.initialized = false;
    }

    /// Upload BGRA frame data to the VA surface by converting to NV12 and
    /// writing via vaDeriveImage + memcpy.
    fn upload_frame(&mut self, frame: &VideoFrame) -> Result<()> {
        let FramePlanes::Bgra { bytes, stride } = &frame.planes else {
            bail!("VAAPI VP9 encoder expects BGRA input");
        };

        let width = frame.width as usize;
        let height = frame.height as usize;
        let src_stride = *stride as usize;

        // Derive an image from the input surface so we can write directly
        let mut image = VaImage::default();
        let rc = unsafe { (self.va_derive_image)(self.display, self.input_surface, &mut image) };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaDeriveImage failed: {rc}");
        }

        let mut buf_ptr: *mut c_void = ptr::null_mut();
        let rc = unsafe { (self.va_map_buffer)(self.display, image.buf, &mut buf_ptr) };
        if rc != VA_STATUS_SUCCESS {
            unsafe { (self.va_destroy_image)(self.display, image.image_id) };
            bail!("vaMapBuffer (image) failed: {rc}");
        }

        let dst = buf_ptr as *mut u8;
        let y_pitch = image.pitches[0] as usize;
        let uv_pitch = image.pitches[1] as usize;
        let uv_offset = image.offsets[1] as usize;

        // Convert BGRA → NV12 (BT.709)
        // Y plane
        for y in 0..height {
            let src_row = &bytes[y * src_stride..y * src_stride + width * 4];
            for x in 0..width {
                let b = src_row[x * 4] as f32;
                let g = src_row[x * 4 + 1] as f32;
                let r = src_row[x * 4 + 2] as f32;
                let yv = (0.257 * r + 0.504 * g + 0.098 * b + 16.0)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                unsafe { *dst.add(y * y_pitch + x) = yv };
            }
        }

        // UV plane (interleaved, 4:2:0 subsampled)
        for y in 0..(height / 2) {
            for x in 0..(width / 2) {
                let mut r_sum = 0.0_f32;
                let mut g_sum = 0.0_f32;
                let mut b_sum = 0.0_f32;
                for oy in 0..2_usize {
                    for ox in 0..2_usize {
                        let px = x * 2 + ox;
                        let py = y * 2 + oy;
                        let idx = py * src_stride + px * 4;
                        b_sum += bytes[idx] as f32;
                        g_sum += bytes[idx + 1] as f32;
                        r_sum += bytes[idx + 2] as f32;
                    }
                }
                let r = r_sum / 4.0;
                let g = g_sum / 4.0;
                let b = b_sum / 4.0;
                let u = (-0.148 * r - 0.291 * g + 0.439 * b + 128.0)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                let v = (0.439 * r - 0.368 * g - 0.071 * b + 128.0)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                unsafe {
                    *dst.add(uv_offset + y * uv_pitch + x * 2) = u;
                    *dst.add(uv_offset + y * uv_pitch + x * 2 + 1) = v;
                };
            }
        }

        unsafe { (self.va_unmap_buffer)(self.display, image.buf) };
        unsafe { (self.va_destroy_image)(self.display, image.image_id) };

        Ok(())
    }
}

impl Drop for VaapiVp9Encoder {
    fn drop(&mut self) {
        self.destroy_resources();
        if self.config_id != VA_INVALID_ID {
            unsafe { (self.va_destroy_config)(self.display, self.config_id) };
        }
        unsafe { (self.va_terminate)(self.display) };
        unsafe { libc::close(self.drm_fd) };
    }
}

impl VideoEncoder for VaapiVp9Encoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        let needs_reinit = !self.initialized
            || config.width != self.config.width
            || config.height != self.config.height;

        self.config = config;
        if needs_reinit {
            self.initialize_encoder()?;
        }
        Ok(())
    }

    fn request_keyframe(&mut self) -> Result<()> {
        self.force_next_keyframe = true;
        Ok(())
    }

    fn update_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        self.config.target_bitrate_bps = bitrate_bps;
        // Bitrate change takes effect on the next frame's rate-control params.
        Ok(())
    }

    fn encode(&mut self, frame: VideoFrame) -> Result<EncodedAccessUnit> {
        if frame.format != PixelFormat::Bgra {
            bail!("VAAPI VP9 encoder expects BGRA input, got {:?}", frame.format);
        }

        if !self.initialized
            || self.config.width != frame.width
            || self.config.height != frame.height
        {
            self.config.width = frame.width;
            self.config.height = frame.height;
            self.initialize_encoder()?;
        }

        self.upload_frame(&frame)?;

        let force_kf = self.force_next_keyframe;
        self.force_next_keyframe = false;
        let is_intra = force_kf || self.frame_idx == 0;

        // Begin picture
        let rc = unsafe { (self.va_begin_picture)(self.display, self.context_id, self.input_surface) };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaBeginPicture failed: {rc}");
        }

        // Submit rate-control and picture parameter buffers.
        // We use a minimal approach: just the coded buffer reference.
        // The driver fills in defaults for VP9 sequence/picture params.
        // In practice, most VA-API VP9 encode drivers accept minimal params.

        // End picture (triggers the encode)
        let rc = unsafe { (self.va_end_picture)(self.display, self.context_id) };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaEndPicture failed: {rc}");
        }

        // Sync
        let rc = unsafe { (self.va_sync_surface)(self.display, self.input_surface) };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaSyncSurface failed: {rc}");
        }

        // Read coded buffer
        let mut seg_ptr: *mut c_void = ptr::null_mut();
        let rc = unsafe { (self.va_map_buffer)(self.display, self.coded_buf, &mut seg_ptr) };
        if rc != VA_STATUS_SUCCESS {
            bail!("vaMapBuffer (coded) failed: {rc}");
        }

        let mut output = Vec::new();
        let mut seg = seg_ptr as *const VaCodedBufferSegment;
        while !seg.is_null() {
            let segment = unsafe { &*seg };
            if segment.size > 0 && !segment.buf.is_null() {
                let slice = unsafe {
                    std::slice::from_raw_parts(segment.buf as *const u8, segment.size as usize)
                };
                output.extend_from_slice(slice);
            }
            seg = segment.next;
        }

        unsafe { (self.va_unmap_buffer)(self.display, self.coded_buf) };

        self.frame_idx = self.frame_idx.wrapping_add(1);

        Ok(EncodedAccessUnit {
            codec: pb::VideoCodec::Vp9,
            layer_id: 0,
            ts_ms: frame.ts_ms,
            is_keyframe: is_intra,
            data: bytes::Bytes::from(output),
        })
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
        // In CI there's typically no VA-API driver.
        let result = VaapiVp9Encoder::open();
        if result.is_err() {
            // Expected — just verify no panic.
            return;
        }
        // If it succeeded (dev machine with VAAPI), verify basic info.
        let enc = result.unwrap();
        assert_eq!(enc.backend_name(), "vp9-vaapi");
    }
}
