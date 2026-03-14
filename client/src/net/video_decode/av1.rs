use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr;

use anyhow::{anyhow, bail, Result};

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_frame::EncodedAccessUnit;
use crate::screen_share::runtime_probe::DecodeBackendKind;

const DAV1D_EAGAIN: i32 = 11;

pub fn build_av1_decoder(backends: &[DecodeBackendKind]) -> Result<Box<dyn VideoDecoder>> {
    for backend in backends {
        if matches!(backend, DecodeBackendKind::Dav1d) {
            return Ok(Box::new(Av1RealtimeDecoder::new()?));
        }
    }

    Err(anyhow!("no AV1 decoder backend available"))
}

pub struct Av1RealtimeDecoder {
    config: VideoSessionConfig,
    dav1d: Dav1dDecoder,
    last_output: Option<DecodedVideoFrame>,
}

impl Av1RealtimeDecoder {
    fn new() -> Result<Self> {
        Ok(Self {
            config: VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 30,
                target_bitrate_bps: 0,
                low_latency: true,
                allow_frame_drop: true,
            },
            dav1d: Dav1dDecoder::new()?,
            last_output: None,
        })
    }
}

impl VideoDecoder for Av1RealtimeDecoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        self.config = config;
        Ok(())
    }

    fn decode(
        &mut self,
        encoded: &EncodedAccessUnit,
        metadata: DecodeMetadata,
    ) -> Result<DecodedVideoFrame> {
        match self.dav1d.decode(encoded.data.as_ref(), metadata.ts_ms)? {
            Some(frame) => {
                self.last_output = Some(frame.clone());
                Ok(frame)
            }
            None => {
                if let Some(mut previous) = self.last_output.clone() {
                    previous.ts_ms = metadata.ts_ms;
                    Ok(previous)
                } else {
                    bail!("dav1d has no picture ready yet")
                }
            }
        }
    }

    fn reset(&mut self) -> Result<()> {
        self.dav1d.flush();
        self.last_output = None;
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "av1-dav1d"
    }
}

struct Dav1dDecoder {
    ctx: *mut Dav1dContext,
}

// SAFETY: `Dav1dDecoder` owns the raw decoder handle and only exposes
// `&mut self` methods for interaction, so the context cannot be used
// concurrently from multiple threads.
unsafe impl Send for Dav1dDecoder {}

impl Dav1dDecoder {
    fn new() -> Result<Self> {
        let mut settings = MaybeUninit::<Dav1dSettings>::zeroed();
        unsafe { dav1d_default_settings(settings.as_mut_ptr()) };
        let mut settings = unsafe { settings.assume_init() };
        settings.max_frame_delay = 1;
        settings.n_threads = 1;

        let mut ctx: *mut Dav1dContext = ptr::null_mut();
        let rc = unsafe { dav1d_open(&mut ctx, &settings) };
        if rc < 0 || ctx.is_null() {
            bail!("failed to initialize dav1d decoder: {}", -rc);
        }

        Ok(Self { ctx })
    }

    fn decode(&mut self, data: &[u8], ts_ms: u32) -> Result<Option<DecodedVideoFrame>> {
        let mut packet = MaybeUninit::<Dav1dData>::zeroed();
        let packet_ptr = packet.as_mut_ptr();

        let dst = unsafe { dav1d_data_create(packet_ptr, data.len()) };
        if dst.is_null() {
            bail!("dav1d failed to allocate packet buffer");
        }
        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
            (*packet_ptr).m.timestamp = ts_ms as i64;
        }

        loop {
            let send_rc = unsafe { dav1d_send_data(self.ctx, packet_ptr) };
            if send_rc == 0 {
                break;
            }

            if -send_rc == DAV1D_EAGAIN {
                if self.receive_picture()?.is_some() {
                    continue;
                }
                unsafe { dav1d_data_unref(packet_ptr) };
                return Ok(None);
            }

            unsafe { dav1d_data_unref(packet_ptr) };
            bail!("dav1d_send_data failed: {}", -send_rc);
        }

        self.receive_picture()
    }

    fn receive_picture(&mut self) -> Result<Option<DecodedVideoFrame>> {
        let mut pic = MaybeUninit::<Dav1dPicture>::zeroed();
        let recv_rc = unsafe { dav1d_get_picture(self.ctx, pic.as_mut_ptr()) };

        if recv_rc == 0 {
            let pic = unsafe { pic.assume_init() };
            let ts_ms = if pic.m.timestamp >= 0 {
                pic.m.timestamp as u32
            } else {
                0
            };
            let out = picture_to_rgba(&pic, ts_ms)?;
            let mut pic = pic;
            unsafe { dav1d_picture_unref(&mut pic) };
            return Ok(Some(out));
        }

        if -recv_rc == DAV1D_EAGAIN {
            return Ok(None);
        }

        bail!("dav1d_get_picture failed: {}", -recv_rc);
    }

    fn flush(&mut self) {
        unsafe { dav1d_flush(self.ctx) }
    }
}

impl Drop for Dav1dDecoder {
    fn drop(&mut self) {
        unsafe { dav1d_close(&mut self.ctx) }
    }
}

fn picture_to_rgba(pic: &Dav1dPicture, ts_ms: u32) -> Result<DecodedVideoFrame> {
    let width = pic.p.w as usize;
    let height = pic.p.h as usize;
    if width == 0 || height == 0 {
        bail!("invalid decoded AV1 frame size {width}x{height}");
    }

    let mut rgba = vec![0_u8; width * height * 4];
    let bitdepth = pic.p.bpc;

    for y in 0..height {
        for x in 0..width {
            let luma = sample_plane(pic.data[0], pic.stride[0], x, y, bitdepth);
            let (uv_x, uv_y) = match pic.p.layout {
                DAV1D_PIXEL_LAYOUT_I400 => (0, 0),
                DAV1D_PIXEL_LAYOUT_I420 => (x / 2, y / 2),
                DAV1D_PIXEL_LAYOUT_I422 => (x / 2, y),
                DAV1D_PIXEL_LAYOUT_I444 => (x, y),
                _ => bail!("unsupported dav1d pixel layout"),
            };
            let (u, v) = if pic.p.layout == DAV1D_PIXEL_LAYOUT_I400 {
                (128.0, 128.0)
            } else {
                (
                    sample_plane(pic.data[1], pic.stride[1], uv_x, uv_y, bitdepth),
                    sample_plane(pic.data[2], pic.stride[1], uv_x, uv_y, bitdepth),
                )
            };

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

    Ok(DecodedVideoFrame {
        width,
        height,
        rgba,
        ts_ms,
    })
}

fn sample_plane(base: *mut c_void, stride: isize, x: usize, y: usize, bitdepth: i32) -> f32 {
    unsafe {
        if bitdepth > 8 {
            let row = (base as *const u8).offset(y as isize * stride) as *const u16;
            let sample = *row.add(x) as f32;
            sample * (255.0 / ((1 << bitdepth) as f32 - 1.0))
        } else {
            let row = (base as *const u8).offset(y as isize * stride);
            *row.add(x) as f32
        }
    }
}

#[repr(C)]
struct Dav1dContext;
#[repr(C)]
struct Dav1dRef;

#[repr(C)]
struct Dav1dUserData {
    data: *const u8,
    r#ref: *mut Dav1dRef,
}

#[repr(C)]
struct Dav1dDataProps {
    timestamp: i64,
    duration: i64,
    offset: i64,
    size: usize,
    user_data: Dav1dUserData,
}

#[repr(C)]
struct Dav1dData {
    data: *const u8,
    sz: usize,
    r#ref: *mut Dav1dRef,
    m: Dav1dDataProps,
}

#[repr(C)]
struct Dav1dPicAllocator {
    cookie: *mut c_void,
    alloc_picture_callback: Option<unsafe extern "C" fn(*mut Dav1dPicture, *mut c_void) -> i32>,
    release_picture_callback: Option<unsafe extern "C" fn(*mut Dav1dPicture, *mut c_void)>,
}

#[repr(C)]
struct Dav1dLogger {
    cookie: *mut c_void,
    callback: Option<unsafe extern "C" fn(*mut c_void, *const i8, *mut c_void)>,
}

#[repr(C)]
struct Dav1dSettings {
    n_threads: i32,
    max_frame_delay: i32,
    apply_grain: i32,
    operating_point: i32,
    all_layers: i32,
    frame_size_limit: u32,
    allocator: Dav1dPicAllocator,
    logger: Dav1dLogger,
    strict_std_compliance: i32,
    output_invisible_frames: i32,
    inloop_filters: i32,
    decode_frame_type: i32,
    reserved: [u8; 16],
}

#[repr(C)]
struct Dav1dPictureParameters {
    w: i32,
    h: i32,
    layout: i32,
    bpc: i32,
}

#[repr(C)]
struct Dav1dPicture {
    seq_hdr: *mut c_void,
    frame_hdr: *mut c_void,
    data: [*mut c_void; 3],
    stride: [isize; 2],
    p: Dav1dPictureParameters,
    m: Dav1dDataProps,
    content_light: *mut c_void,
    mastering_display: *mut c_void,
    itut_t35: *mut c_void,
    n_itut_t35: usize,
    reserved: [usize; 4],
    frame_hdr_ref: *mut Dav1dRef,
    seq_hdr_ref: *mut Dav1dRef,
    content_light_ref: *mut Dav1dRef,
    mastering_display_ref: *mut Dav1dRef,
    itut_t35_ref: *mut Dav1dRef,
    reserved_ref: [usize; 4],
    r#ref: *mut Dav1dRef,
    allocator_data: *mut c_void,
}

const DAV1D_PIXEL_LAYOUT_I400: i32 = 0;
const DAV1D_PIXEL_LAYOUT_I420: i32 = 1;
const DAV1D_PIXEL_LAYOUT_I422: i32 = 2;
const DAV1D_PIXEL_LAYOUT_I444: i32 = 3;

// Native library linkage is provided by build.rs so Windows can force
// static linking for a single-file client binary.
unsafe extern "C" {
    fn dav1d_default_settings(s: *mut Dav1dSettings);
    fn dav1d_open(c_out: *mut *mut Dav1dContext, s: *const Dav1dSettings) -> i32;
    fn dav1d_send_data(c: *mut Dav1dContext, input: *mut Dav1dData) -> i32;
    fn dav1d_get_picture(c: *mut Dav1dContext, out: *mut Dav1dPicture) -> i32;
    fn dav1d_flush(c: *mut Dav1dContext);
    fn dav1d_close(c_out: *mut *mut Dav1dContext);
    fn dav1d_data_create(data: *mut Dav1dData, sz: usize) -> *mut u8;
    fn dav1d_data_unref(data: *mut Dav1dData);
    fn dav1d_picture_unref(pic: *mut Dav1dPicture);
}

pub(crate) fn can_initialize_backend(backend: DecodeBackendKind) -> bool {
    matches!(backend, DecodeBackendKind::Dav1d) && build_av1_decoder(&[backend]).is_ok()
}
