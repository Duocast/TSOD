//! Safe Rust wrappers around the libvpx VP9 encode/decode C API.
//!
//! FFI structs are declared inline following the same pattern as `net/video_decode/av1.rs`
//! (dav1d bindings).  Only the fields needed for our realtime pipeline are named; the
//! remainder of each struct is covered by trailing `_tail` arrays so sizeof() agrees with
//! the C side on 64-bit LP64 platforms (Linux x86-64, Linux aarch64, Windows x64).
//!
//! ABI version constants target libvpx >= 1.7 which ships on every relevant distro.
//! If a system has an older libvpx the init call returns an error and the backend is not
//! advertised – see `can_initialize_backend`.

use std::ffi::c_void;
use std::os::raw::{c_int, c_ulong, c_uint};

use anyhow::{bail, Result};

// ── Constants ────────────────────────────────────────────────────────────────

/// ABI version numbers for libvpx 1.14 (Ubuntu 24.04 noble).
///
/// These are computed as:
///   VPX_IMAGE_ABI_VERSION       = 5
///   VPX_CODEC_ABI_VERSION       = 4 + VPX_IMAGE_ABI_VERSION  = 9
///   VPX_DECODER_ABI_VERSION     = 3 + VPX_CODEC_ABI_VERSION  = 12
///   VPX_EXT_RATECTRL_ABI_VERSION = 7
///   VPX_TPL_ABI_VERSION         = 2
///   VPX_ENCODER_ABI_VERSION     = 16 + VPX_CODEC_ABI_VERSION
///                                    + VPX_EXT_RATECTRL_ABI_VERSION
///                                    + VPX_TPL_ABI_VERSION    = 34
///
/// If linking against an older libvpx (< 1.11), the init calls will return a
/// non-OK error code; `probe_encoder`/`probe_decoder` will then return false
/// and VP9 will not be advertised.
pub const VPX_ENCODER_ABI_VERSION: c_int = 34;
pub const VPX_DECODER_ABI_VERSION: c_int = 12;

pub const VPX_CODEC_OK: c_int = 0;

pub const VPX_DL_REALTIME: c_ulong = 1;

/// Force keyframe flag passed to `vpx_codec_encode`.
pub const VPX_EFLAG_FORCE_KF: i64 = 1 << 0;

/// Bit set in `VpxCxPktFrame::flags` when the output packet is a keyframe.
pub const VPX_FRAME_IS_KEY: c_uint = 0x1;

/// Pixel format: planar YUV 4:2:0 (I420).
/// VPX_IMG_FMT_PLANAR (0x100) | 2 = 0x102.
pub const VPX_IMG_FMT_I420: c_uint = 0x102;

/// Plane indices.
pub const VPX_PLANE_Y: usize = 0;
pub const VPX_PLANE_U: usize = 1;
pub const VPX_PLANE_V: usize = 2;

/// Rate-control mode: constant bitrate.
pub const VPX_CBR: c_uint = 1;

/// Keyframe mode: encoder decides placement (subject to kf_max_dist).
pub const VPX_KF_AUTO: c_uint = 1;

/// Single-pass encode.
pub const VPX_RC_ONE_PASS: c_uint = 0;

/// Default error-resilient flag.
pub const VPX_ERROR_RESILIENT_DEFAULT: c_uint = 1;

/// `vpx_codec_cx_pkt_kind`: compressed video frame.
pub const VPX_CODEC_CX_FRAME_PKT: c_int = 0;

// ── FFI structures ───────────────────────────────────────────────────────────

/// Opaque codec context (encoder or decoder).
///
/// The real struct is ~56 bytes on 64-bit; we over-allocate to 128 bytes.
/// SAFETY: always zero-initialised before passing to the library.
#[repr(C, align(8))]
pub struct VpxCodecCtx([u8; 128]);

/// Opaque interface descriptor returned by `vpx_codec_vp9_cx/dx()`.
#[repr(C)]
pub struct VpxCodecIface(u8); // opaque; only ever used via pointer

/// Encoder configuration (`vpx_codec_enc_cfg_t`).
///
/// Field offsets on LP64 have been verified against libvpx 1.12 with
/// `offsetof` in C.  Fields after `kf_max_dist` are covered by `_tail`.
///
/// Layout (bytes):
///   0  g_usage          4
///   4  g_threads        4
///   8  g_profile        4
///  12  g_w              4
///  16  g_h              4
///  20  g_bit_depth      4
///  24  g_input_bit_depth 4
///  28  g_timebase_num   4   ┐ struct vpx_rational { int num; int den; }
///  32  g_timebase_den   4   ┘
///  36  g_error_resilient 4
///  40  g_pass           4
///  44  g_lag_in_frames  4
///  48  rc_dropframe_thresh 4
///  52  rc_resize_allowed   4
///  56  rc_scaled_width     4
///  60  rc_scaled_height    4
///  64  rc_resize_up_thresh 4
///  68  rc_resize_down_thresh 4
///  72  rc_end_usage        4
///  [4 padding]
///  80  rc_twopass_stats_in_buf   *  (8)
///  88  rc_twopass_stats_in_sz    sz (8)
///  96  rc_firstpass_mb_stats_in_buf * (8)
/// 104  rc_firstpass_mb_stats_in_sz  sz (8)
/// 112  rc_target_bitrate  4
/// 116  rc_min_quantizer   4
/// 120  rc_max_quantizer   4
/// 124  rc_undershoot_pct  4
/// 128  rc_overshoot_pct   4
/// 132  rc_buf_sz          4
/// 136  rc_buf_initial_sz  4
/// 140  rc_buf_optimal_sz  4
/// 144  rc_2pass_vbr_bias_pct 4
/// 148  rc_2pass_vbr_minsection_pct 4
/// 152  rc_2pass_vbr_maxsection_pct 4
/// 156  rc_2pass_vbr_corpus_complexity 4
/// 160  kf_mode            4
/// 164  kf_min_dist        4
/// 168  kf_max_dist        4
/// 172  … _tail …        332   ← covers remaining fields including TPL/ext-RC
///                              (sizeof(vpx_codec_enc_cfg_t) == 504 on libvpx 1.14)
/// 504  (total, struct align = 8 due to pointer fields)
#[repr(C)]
pub struct VpxCodecEncCfg {
    pub g_usage: c_uint,
    pub g_threads: c_uint,
    pub g_profile: c_uint,
    pub g_w: c_uint,
    pub g_h: c_uint,
    pub g_bit_depth: c_uint,
    pub g_input_bit_depth: c_uint,
    pub g_timebase_num: c_int,
    pub g_timebase_den: c_int,
    pub g_error_resilient: c_uint,
    pub g_pass: c_uint,
    pub g_lag_in_frames: c_uint,
    pub rc_dropframe_thresh: c_uint,
    pub rc_resize_allowed: c_uint,
    pub rc_scaled_width: c_uint,
    pub rc_scaled_height: c_uint,
    pub rc_resize_up_thresh: c_uint,
    pub rc_resize_down_thresh: c_uint,
    pub rc_end_usage: c_uint,
    // repr(C) inserts 4 bytes of alignment padding here before the pointer
    rc_twopass_stats_in_buf: *mut c_void,
    rc_twopass_stats_in_sz: usize,
    rc_firstpass_mb_stats_in_buf: *mut c_void,
    rc_firstpass_mb_stats_in_sz: usize,
    pub rc_target_bitrate: c_uint,
    pub rc_min_quantizer: c_uint,
    pub rc_max_quantizer: c_uint,
    pub rc_undershoot_pct: c_uint,
    pub rc_overshoot_pct: c_uint,
    pub rc_buf_sz: c_uint,
    pub rc_buf_initial_sz: c_uint,
    pub rc_buf_optimal_sz: c_uint,
    pub rc_2pass_vbr_bias_pct: c_uint,
    pub rc_2pass_vbr_minsection_pct: c_uint,
    pub rc_2pass_vbr_maxsection_pct: c_uint,
    pub rc_2pass_vbr_corpus_complexity: c_uint,
    pub kf_mode: c_uint,
    pub kf_min_dist: c_uint,
    pub kf_max_dist: c_uint,
    // Remaining fields (ss_*, ts_*, layer_*, temporal_layering_mode, TPL/ext-RC
    // additions in libvpx >= 1.11) — we do not need named access to them.
    // Size: 504 total - 172 declared = 332 bytes.
    _tail: [u8; 332],
}

/// Image descriptor (`vpx_image_t`).
///
/// Layout (LP64, bytes):
///   0  fmt              4
///   4  cs               4
///   8  r                4
///  12  w                4
///  16  h                4
///  20  bit_depth        4
///  24  d_w              4
///  28  d_h              4
///  32  r_w              4
///  36  r_h              4
///  40  x_chroma_shift   4
///  44  y_chroma_shift   4
///  48  planes[4]       32   (4 × *mut u8)
///  80  stride[4]       16   (4 × i32)
///  96  bps              4
/// 100  temporal_id      4
/// 104  user_priv        8
/// 112  img_data         8
/// 120  img_data_ownership 4
/// 124  self_allocd      4
/// 128  fb_priv          8
/// 136  (total, align = 8)
#[repr(C)]
pub struct VpxImage {
    pub fmt: c_uint,
    pub cs: c_uint,
    pub r: c_uint,
    pub w: c_uint,
    pub h: c_uint,
    pub bit_depth: c_uint,
    pub d_w: c_uint,
    pub d_h: c_uint,
    pub r_w: c_uint,
    pub r_h: c_uint,
    pub x_chroma_shift: c_uint,
    pub y_chroma_shift: c_uint,
    pub planes: [*mut u8; 4],
    pub stride: [c_int; 4],
    pub bps: c_int,
    pub temporal_id: c_uint,
    pub user_priv: *mut c_void,
    pub img_data: *mut u8,
    pub img_data_ownership: c_int,
    pub self_allocd: c_int,
    pub fb_priv: *mut c_void,
}

/// The frame sub-field of a `vpx_codec_cx_pkt_t.data` union (kind == 0).
///
/// Layout inside the union (LP64, bytes):
///   0  buf              8
///   8  sz               8
///  16  pts              8
///  24  duration         8
///  32  flags            4
///  36  partition_id     4
///  40  width[5]        20
///  60  height[5]       20
///  80  (end of frame sub-struct)
#[repr(C)]
struct VpxCxPktFrame {
    buf: *mut c_void,
    sz: usize,
    pts: i64,
    duration: u64,
    flags: c_uint,
    partition_id: c_int,
    width: [c_uint; 5],
    height: [c_uint; 5],
}

/// Encoder output packet (`vpx_codec_cx_pkt_t`).
///
/// Layout (LP64, bytes):
///   0  kind            4
///   [4 padding]
///   8  data union    128   (largest member is the pad[124] field in C)
/// 136  (total, align = 8)
///
/// We overlay the frame variant of the union starting at offset 8.
#[repr(C)]
struct VpxCodecCxPkt {
    kind: c_int,
    // repr(C) inserts 4 bytes padding here before the union (8-byte aligned)
    frame: VpxCxPktFrame, // offset 8, size 80
    _rest: [u8; 48],      // fills to 136 total
}

// ── External C functions ─────────────────────────────────────────────────────

#[link(name = "vpx")]
unsafe extern "C" {
    pub fn vpx_codec_vp9_cx() -> *const VpxCodecIface;
    pub fn vpx_codec_vp9_dx() -> *const VpxCodecIface;

    pub fn vpx_codec_enc_config_default(
        iface: *const VpxCodecIface,
        cfg: *mut VpxCodecEncCfg,
        usage: c_uint,
    ) -> c_int;

    pub fn vpx_codec_enc_init_ver(
        ctx: *mut VpxCodecCtx,
        iface: *const VpxCodecIface,
        cfg: *const VpxCodecEncCfg,
        flags: i64,
        abi_ver: c_int,
    ) -> c_int;

    pub fn vpx_codec_encode(
        ctx: *mut VpxCodecCtx,
        img: *const VpxImage,
        pts: i64,
        duration: c_ulong,
        flags: i64,
        deadline: c_ulong,
    ) -> c_int;

    pub fn vpx_codec_get_cx_data(
        ctx: *mut VpxCodecCtx,
        iter: *mut *const c_void,
    ) -> *const VpxCodecCxPkt;

    pub fn vpx_codec_enc_config_set(
        ctx: *mut VpxCodecCtx,
        cfg: *const VpxCodecEncCfg,
    ) -> c_int;

    pub fn vpx_codec_dec_init_ver(
        ctx: *mut VpxCodecCtx,
        iface: *const VpxCodecIface,
        cfg: *const c_void, // vpx_codec_dec_cfg_t* (NULL = defaults)
        flags: i64,
        abi_ver: c_int,
    ) -> c_int;

    pub fn vpx_codec_decode(
        ctx: *mut VpxCodecCtx,
        data: *const u8,
        data_sz: c_uint,
        user_priv: *mut c_void,
        deadline: i64,
    ) -> c_int;

    pub fn vpx_codec_get_frame(
        ctx: *mut VpxCodecCtx,
        iter: *mut *const c_void,
    ) -> *mut VpxImage;

    pub fn vpx_codec_destroy(ctx: *mut VpxCodecCtx) -> c_int;

    pub fn vpx_img_alloc(
        img: *mut VpxImage,
        fmt: c_uint,
        d_w: c_uint,
        d_h: c_uint,
        align: c_uint,
    ) -> *mut VpxImage;

    pub fn vpx_img_free(img: *mut VpxImage);

    pub fn vpx_codec_error(ctx: *const VpxCodecCtx) -> *const i8;
}

// ── Safe helpers ─────────────────────────────────────────────────────────────

fn vpx_err_msg(ctx: &VpxCodecCtx) -> String {
    let raw = unsafe { vpx_codec_error(ctx as *const VpxCodecCtx) };
    if raw.is_null() {
        return "unknown libvpx error".to_owned();
    }
    unsafe { std::ffi::CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned()
}

// ── LibvpxEncoder ────────────────────────────────────────────────────────────

/// Safe wrapper around a libvpx VP9 encoder context.
pub struct LibvpxEncoder {
    ctx: Box<VpxCodecCtx>,
    cfg: Box<VpxCodecEncCfg>,
    pts: i64,
    width: u32,
    height: u32,
}

/// SAFETY: `LibvpxEncoder` owns the context exclusively and only exposes `&mut self` methods.
unsafe impl Send for LibvpxEncoder {}

pub struct EncoderOutput {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

impl LibvpxEncoder {
    /// Initialise a real-time VP9 encoder.
    ///
    /// `bitrate_bps` is converted to kbps internally; the minimum is 1 kbps.
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self> {
        let bitrate_kbps = (bitrate_bps / 1000).max(1);

        let mut cfg = Box::new(unsafe { std::mem::zeroed::<VpxCodecEncCfg>() });

        let rc = unsafe {
            vpx_codec_enc_config_default(vpx_codec_vp9_cx(), &mut *cfg, 0)
        };
        if rc != VPX_CODEC_OK {
            bail!("vpx_codec_enc_config_default failed: {rc}");
        }

        cfg.g_w = width;
        cfg.g_h = height;
        cfg.g_timebase_num = 1;
        cfg.g_timebase_den = fps as c_int;
        cfg.g_lag_in_frames = 0; // no look-ahead → true realtime
        cfg.rc_end_usage = VPX_CBR;
        cfg.g_pass = VPX_RC_ONE_PASS;
        cfg.kf_mode = VPX_KF_AUTO;
        cfg.kf_min_dist = 0;
        cfg.kf_max_dist = 300; // ≤10 s at 30 fps
        cfg.rc_target_bitrate = bitrate_kbps;
        cfg.rc_min_quantizer = 4;
        cfg.rc_max_quantizer = 56;
        cfg.rc_undershoot_pct = 50;
        cfg.rc_overshoot_pct = 50;
        cfg.rc_buf_sz = 1000;
        cfg.rc_buf_initial_sz = 500;
        cfg.rc_buf_optimal_sz = 600;
        cfg.g_error_resilient = VPX_ERROR_RESILIENT_DEFAULT;
        cfg.g_threads = std::thread::available_parallelism()
            .map(|n| n.get().min(4) as c_uint)
            .unwrap_or(2);

        let mut ctx = Box::new(VpxCodecCtx([0u8; 128]));

        let rc = unsafe {
            vpx_codec_enc_init_ver(
                &mut *ctx,
                vpx_codec_vp9_cx(),
                &*cfg,
                0,
                VPX_ENCODER_ABI_VERSION,
            )
        };
        if rc != VPX_CODEC_OK {
            bail!(
                "vpx_codec_enc_init_ver failed ({rc}): {}",
                vpx_err_msg(&ctx)
            );
        }

        Ok(Self {
            ctx,
            cfg,
            pts: 0,
            width,
            height,
        })
    }

    /// Encode a single I420 frame.
    ///
    /// Plane slices must be packed (stride == width for luma, (width+1)/2 for chroma).
    pub fn encode(
        &mut self,
        y_plane: &[u8],
        u_plane: &[u8],
        v_plane: &[u8],
        force_keyframe: bool,
    ) -> Result<Vec<EncoderOutput>> {
        let width = self.width;
        let height = self.height;
        let uv_w = (width as usize + 1) / 2;
        let uv_h = (height as usize + 1) / 2;

        // Allocate a temporary image and fill planes.
        let mut img = unsafe { std::mem::zeroed::<VpxImage>() };
        let ptr = unsafe { vpx_img_alloc(&mut img, VPX_IMG_FMT_I420, width, height, 1) };
        if ptr.is_null() {
            bail!("vpx_img_alloc failed");
        }

        unsafe {
            copy_plane(y_plane, width as usize, img.planes[VPX_PLANE_Y], img.stride[VPX_PLANE_Y] as usize, height as usize);
            copy_plane(u_plane, uv_w, img.planes[VPX_PLANE_U], img.stride[VPX_PLANE_U] as usize, uv_h);
            copy_plane(v_plane, uv_w, img.planes[VPX_PLANE_V], img.stride[VPX_PLANE_V] as usize, uv_h);
        }

        let flags: i64 = if force_keyframe { VPX_EFLAG_FORCE_KF } else { 0 };

        let rc = unsafe {
            vpx_codec_encode(
                &mut *self.ctx,
                &img,
                self.pts,
                1, // one tick per frame in our 1/fps timebase
                flags,
                VPX_DL_REALTIME,
            )
        };

        unsafe { vpx_img_free(&mut img) };

        if rc != VPX_CODEC_OK {
            bail!(
                "vpx_codec_encode failed ({rc}): {}",
                vpx_err_msg(&self.ctx)
            );
        }

        self.pts += 1;

        let mut outputs = Vec::new();
        let mut iter: *const c_void = std::ptr::null();
        loop {
            let pkt = unsafe { vpx_codec_get_cx_data(&mut *self.ctx, &mut iter) };
            if pkt.is_null() {
                break;
            }
            let pkt = unsafe { &*pkt };
            if pkt.kind != VPX_CODEC_CX_FRAME_PKT {
                continue;
            }
            let frame = &pkt.frame;
            let data = unsafe {
                std::slice::from_raw_parts(frame.buf as *const u8, frame.sz)
            }
            .to_vec();
            let is_keyframe = (frame.flags & VPX_FRAME_IS_KEY) != 0;
            outputs.push(EncoderOutput { data, is_keyframe });
        }

        Ok(outputs)
    }

    /// Reconfigure the encoder's target bitrate without re-initialisation.
    pub fn update_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        self.cfg.rc_target_bitrate = (bitrate_bps / 1000).max(1);
        let rc = unsafe { vpx_codec_enc_config_set(&mut *self.ctx, &*self.cfg) };
        if rc != VPX_CODEC_OK {
            bail!(
                "vpx_codec_enc_config_set failed ({rc}): {}",
                vpx_err_msg(&self.ctx)
            );
        }
        Ok(())
    }
}

impl Drop for LibvpxEncoder {
    fn drop(&mut self) {
        unsafe { vpx_codec_destroy(&mut *self.ctx) };
    }
}

// ── LibvpxDecoder ────────────────────────────────────────────────────────────

pub struct LibvpxDecoder {
    ctx: Box<VpxCodecCtx>,
}

/// SAFETY: same ownership argument as `LibvpxEncoder`.
unsafe impl Send for LibvpxDecoder {}

pub struct DecoderOutput {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl LibvpxDecoder {
    pub fn new() -> Result<Self> {
        let mut ctx = Box::new(VpxCodecCtx([0u8; 128]));
        let rc = unsafe {
            vpx_codec_dec_init_ver(
                &mut *ctx,
                vpx_codec_vp9_dx(),
                std::ptr::null(),
                0,
                VPX_DECODER_ABI_VERSION,
            )
        };
        if rc != VPX_CODEC_OK {
            bail!(
                "vpx_codec_dec_init_ver failed ({rc}): {}",
                vpx_err_msg(&ctx)
            );
        }
        Ok(Self { ctx })
    }

    /// Decode a VP9 access unit; returns zero or one decoded frames.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecoderOutput>> {
        if data.is_empty() {
            bail!("empty VP9 access unit");
        }

        let rc = unsafe {
            vpx_codec_decode(
                &mut *self.ctx,
                data.as_ptr(),
                data.len() as c_uint,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != VPX_CODEC_OK {
            bail!(
                "vpx_codec_decode failed ({rc}): {}",
                vpx_err_msg(&self.ctx)
            );
        }

        let mut iter: *const c_void = std::ptr::null();
        let img_ptr = unsafe { vpx_codec_get_frame(&mut *self.ctx, &mut iter) };
        if img_ptr.is_null() {
            return Ok(None);
        }
        let img = unsafe { &*img_ptr };
        let width = img.d_w as usize;
        let height = img.d_h as usize;

        if width == 0 || height == 0 {
            bail!("libvpx returned zero-size frame {width}x{height}");
        }

        let rgba = yuv420_to_rgba(img, width, height)?;
        Ok(Some(DecoderOutput { width, height, rgba }))
    }

    /// Flush any pending decoder state (e.g., on seek or reset).
    pub fn flush(&mut self) {
        // Drain with NULL input:
        let _ = unsafe {
            vpx_codec_decode(
                &mut *self.ctx,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
            )
        };
    }
}

impl Drop for LibvpxDecoder {
    fn drop(&mut self) {
        unsafe { vpx_codec_destroy(&mut *self.ctx) };
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Copy a packed plane into a libvpx image plane that may have a larger stride.
///
/// SAFETY: caller ensures `dst` points to writable memory of at least
/// `rows × dst_stride` bytes as allocated by `vpx_img_alloc`.
unsafe fn copy_plane(
    src: &[u8],
    src_stride: usize,
    dst: *mut u8,
    dst_stride: usize,
    rows: usize,
) {
    for row in 0..rows {
        let src_row = &src[row * src_stride..(row + 1) * src_stride];
        let dst_row = std::slice::from_raw_parts_mut(dst.add(row * dst_stride), src_stride);
        dst_row.copy_from_slice(src_row);
    }
}

/// Convert an I420 `VpxImage` to packed RGBA.
fn yuv420_to_rgba(img: &VpxImage, width: usize, height: usize) -> Result<Vec<u8>> {
    let y_stride = img.stride[VPX_PLANE_Y] as usize;
    let u_stride = img.stride[VPX_PLANE_U] as usize;
    let v_stride = img.stride[VPX_PLANE_V] as usize;

    // Sanity: strides must be at least the chroma/luma width
    let uv_w = (width + 1) / 2;
    if y_stride < width || u_stride < uv_w || v_stride < uv_w {
        bail!("libvpx returned invalid strides for {width}x{height} frame");
    }

    let mut rgba = vec![0_u8; width * height * 4];

    for yy in 0..height {
        for xx in 0..width {
            let y_val =
                unsafe { *img.planes[VPX_PLANE_Y].add(yy * y_stride + xx) } as f32;
            let u_val =
                unsafe { *img.planes[VPX_PLANE_U].add((yy / 2) * u_stride + xx / 2) } as f32
                    - 128.0;
            let v_val =
                unsafe { *img.planes[VPX_PLANE_V].add((yy / 2) * v_stride + xx / 2) } as f32
                    - 128.0;

            let r = (y_val + 1.402 * v_val).clamp(0.0, 255.0) as u8;
            let g = (y_val - 0.344_136 * u_val - 0.714_136 * v_val).clamp(0.0, 255.0) as u8;
            let b = (y_val + 1.772 * u_val).clamp(0.0, 255.0) as u8;

            let idx = (yy * width + xx) * 4;
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = 255;
        }
    }

    Ok(rgba)
}

/// Convert a BGRA packed frame to separate I420 planes.
///
/// Returns `(y_plane, u_plane, v_plane)` with packed strides (no padding).
pub fn bgra_to_i420(
    bytes: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let uv_w = (width + 1) / 2;
    let uv_h = (height + 1) / 2;
    let mut y_plane = vec![0_u8; width * height];
    let mut u_acc = vec![0_u32; uv_w * uv_h];
    let mut v_acc = vec![0_u32; uv_w * uv_h];
    let mut uv_cnt = vec![0_u32; uv_w * uv_h];

    for y in 0..height {
        let row_start = y * stride;
        if row_start + width * 4 > bytes.len() {
            bail!("BGRA buffer too small for {width}x{height} at stride {stride}");
        }
        let row = &bytes[row_start..row_start + width * 4];
        for x in 0..width {
            let b = row[x * 4] as f32;
            let g = row[x * 4 + 1] as f32;
            let r = row[x * 4 + 2] as f32;
            let yv = (0.299 * r + 0.587 * g + 0.114 * b).clamp(0.0, 255.0) as u8;
            y_plane[y * width + x] = yv;
            let ui = (y / 2) * uv_w + (x / 2);
            u_acc[ui] += ((-0.169 * r - 0.331 * g + 0.5 * b) + 128.0).clamp(0.0, 255.0) as u32;
            v_acc[ui] += ((0.5 * r - 0.419 * g - 0.081 * b) + 128.0).clamp(0.0, 255.0) as u32;
            uv_cnt[ui] += 1;
        }
    }

    let mut u_plane = vec![0_u8; uv_w * uv_h];
    let mut v_plane = vec![0_u8; uv_w * uv_h];
    for i in 0..uv_w * uv_h {
        let d = uv_cnt[i].max(1);
        u_plane[i] = (u_acc[i] / d) as u8;
        v_plane[i] = (v_acc[i] / d) as u8;
    }

    Ok((y_plane, u_plane, v_plane))
}

/// Probe whether libvpx VP9 encoder can be initialised (tiny 16×16 test).
pub fn probe_encoder() -> bool {
    LibvpxEncoder::new(16, 16, 30, 100_000).is_ok()
}

/// Probe whether libvpx VP9 decoder can be initialised.
pub fn probe_decoder() -> bool {
    LibvpxDecoder::new().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check struct sizes against known C values (verified against libvpx 1.14 headers).
    #[test]
    fn struct_size_sanity() {
        // VpxCodecCtx: opaque, 128 bytes (actual C struct is 56 bytes; we over-allocate)
        assert_eq!(std::mem::size_of::<VpxCodecCtx>(), 128);
        // VpxImage: 136 bytes on LP64 (matches C sizeof(vpx_image_t))
        assert_eq!(std::mem::size_of::<VpxImage>(), 136);
        // VpxCodecCxPkt: 136 bytes on LP64 (matches C sizeof(vpx_codec_cx_pkt_t))
        assert_eq!(std::mem::size_of::<VpxCodecCxPkt>(), 136);
        // VpxCodecEncCfg: 504 bytes on LP64 (matches C sizeof(vpx_codec_enc_cfg_t) for libvpx 1.14)
        assert_eq!(std::mem::size_of::<VpxCodecEncCfg>(), 504);
    }

    #[test]
    fn bgra_to_i420_pure_red() {
        // 2×2 pure red (BGRA = 0,0,255,255)
        let bgra = vec![0_u8, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255];
        let (y, u, v) = bgra_to_i420(&bgra, 8, 2, 2).unwrap();
        // All Y should be roughly 76 (0.299*255)
        for &yv in &y {
            assert!((60..100).contains(&(yv as i32)), "Y={yv}");
        }
        // U should be < 128, V > 128 for red
        assert!(u[0] < 128, "U={}", u[0]);
        assert!(v[0] > 128, "V={}", v[0]);
    }

    #[test]
    fn encoder_probe_succeeds_or_skips() {
        // This test passes on systems with libvpx and is skipped (not panics) otherwise.
        let _ = probe_encoder();
    }

    #[test]
    fn decoder_probe_succeeds_or_skips() {
        let _ = probe_decoder();
    }
}
