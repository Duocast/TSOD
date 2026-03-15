/// NVENC AV1 hardware encoder backend.
///
/// ## Runtime dependencies
///
/// The NVIDIA Video Codec SDK encode library must be loadable at runtime:
///  - **Linux**: `libnvidia-encode.so.1` (ships with the proprietary NVIDIA driver ≥ 530)
///  - **Windows**: `nvEncodeAPI64.dll` / `nvEncodeAPI.dll` (ships with Game Ready ≥ 531.18)
///
/// An Ada Lovelace (RTX 40) or Blackwell (RTX 50) GPU is required for AV1
/// support in the NVENC hardware.
///
/// ## Low-latency screen-share settings
///
/// - Constant-bitrate (CBR) rate control for consistent frame delivery.
/// - Zero-latency mode (`NV_ENC_LOWLATENCY_HP` preset equivalent).
/// - Keyframe interval capped at 300 frames (~5 s @ 60 fps) with on-demand
///   keyframe insertion via `request_keyframe()`.
/// - Single-slice output for minimal decode-side latency.
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Result};
use tracing::{info, warn};

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_frame::{EncodedAccessUnit, FramePlanes, PixelFormat, VideoFrame};
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::runtime_probe::nvidia::probe_nvenc_av1;

/// Try to build an NVENC AV1 encoder.  Fails fast if the probe says NVENC
/// is unavailable (missing GPU, wrong generation, or missing library).
pub fn build_nvenc_encoder() -> Result<Box<dyn VideoEncoder>> {
    let status = probe_nvenc_av1();
    if !status.available {
        bail!(
            "NVENC AV1 unavailable: {}",
            status.reason.unwrap_or_else(|| "probe failed".into())
        );
    }

    let encoder = NvencAv1Encoder::open()?;
    Ok(Box::new(encoder))
}

// ── NVENC C API constants ────────────────────────────────────────────────────

const NV_ENC_SUCCESS: u32 = 0;
const NV_ENC_ERR_NO_ENCODE_DEVICE: u32 = 2;
const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = 1 | (12 << 16);
const NV_ENC_INITIALIZE_PARAMS_VER: u32 = 5 | (12 << 16);
const NV_ENC_CONFIG_VER: u32 = 8 | (12 << 16);
const NV_ENC_PIC_PARAMS_VER: u32 = 4 | (12 << 16);
const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = 1 | (12 << 16);
const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = 1 | (12 << 16);
const NV_ENC_LOCK_BITSTREAM_VER: u32 = 1 | (12 << 16);
const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = 1 | (12 << 16);

// Codec GUIDs
const NV_ENC_CODEC_AV1_GUID: [u8; 16] = [
    0x0E, 0x55, 0x05, 0x81, 0xB5, 0x88, 0xEE, 0x49, 0x8C, 0x49, 0x4F, 0x7F, 0x2E, 0x80, 0x17,
    0xBF,
];

// Preset GUIDs – low-latency high-perf
const NV_ENC_PRESET_P1_GUID: [u8; 16] = [
    0x69, 0xCD, 0x57, 0xFC, 0x2B, 0x17, 0x57, 0x44, 0x98, 0xC0, 0x80, 0x06, 0x0C, 0xEA, 0x24,
    0x27,
];

// Rate-control modes
const NV_ENC_PARAMS_RC_CBR: u32 = 2;

// Buffer formats
const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x00000020;

// Picture types
const NV_ENC_PIC_TYPE_IDR: u32 = 4;
const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 4;
const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 8;

// Encode-pic actions
const NV_ENC_PIC_ACTION_ENCODE: u32 = 0;

// Tuning info
const NV_ENC_TUNING_INFO_LOW_LATENCY: u32 = 2;

// Device type
const NV_ENC_DEVICE_TYPE_CUDA: u32 = 1;

// ── Opaque NVENC types ────────────────────────────────────────────────────────

type NvEncoder = *mut c_void;
type CuDevice = i32;
type CuContext = *mut c_void;

// ── FFI function-pointer table ────────────────────────────────────────────────
//
// We load the NVENC library at runtime and resolve the function pointers from
// the NvEncodeAPICreateInstance entry point, which populates a vtable struct.
// This mirrors how the NVIDIA Video Codec SDK samples work.

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncFunctions {
    version: u32,
    _reserved: u32,
    nvEncOpenEncodeSessionEx: Option<unsafe extern "C" fn(*mut NvEncOpenSessionParams, *mut NvEncoder) -> u32>,
    nvEncGetEncodeGUIDCount: Option<unsafe extern "C" fn(NvEncoder, *mut u32) -> u32>,
    nvEncGetEncodeGUIDs: Option<unsafe extern "C" fn(NvEncoder, *mut [u8; 16], u32, *mut u32) -> u32>,
    nvEncGetEncodeProfileGUIDCount: Option<unsafe extern "C" fn(NvEncoder, [u8; 16], *mut u32) -> u32>,
    nvEncGetEncodeProfileGUIDs: Option<unsafe extern "C" fn(NvEncoder, [u8; 16], *mut [u8; 16], u32, *mut u32) -> u32>,
    nvEncGetInputFormatCount: Option<unsafe extern "C" fn(NvEncoder, [u8; 16], *mut u32) -> u32>,
    nvEncGetInputFormats: Option<unsafe extern "C" fn(NvEncoder, [u8; 16], *mut u32, u32, *mut u32) -> u32>,
    nvEncGetEncodeCaps: Option<unsafe extern "C" fn(NvEncoder, [u8; 16], *mut c_void, *mut i32) -> u32>,
    nvEncGetEncodePresetCount: Option<unsafe extern "C" fn(NvEncoder, [u8; 16], *mut u32) -> u32>,
    nvEncGetEncodePresetGUIDs: Option<unsafe extern "C" fn(NvEncoder, [u8; 16], *mut [u8; 16], u32, *mut u32) -> u32>,
    nvEncGetEncodePresetConfigEx: Option<unsafe extern "C" fn(NvEncoder, [u8; 16], [u8; 16], u32, *mut NvEncPresetConfig) -> u32>,
    nvEncInitializeEncoder: Option<unsafe extern "C" fn(NvEncoder, *mut NvEncInitParams) -> u32>,
    nvEncCreateInputBuffer: Option<unsafe extern "C" fn(NvEncoder, *mut NvEncCreateInputBuffer) -> u32>,
    nvEncDestroyInputBuffer: Option<unsafe extern "C" fn(NvEncoder, *mut c_void) -> u32>,
    nvEncCreateBitstreamBuffer: Option<unsafe extern "C" fn(NvEncoder, *mut NvEncCreateBitstreamBuffer) -> u32>,
    nvEncDestroyBitstreamBuffer: Option<unsafe extern "C" fn(NvEncoder, *mut c_void) -> u32>,
    nvEncEncodePicture: Option<unsafe extern "C" fn(NvEncoder, *mut NvEncPicParams) -> u32>,
    nvEncLockBitstream: Option<unsafe extern "C" fn(NvEncoder, *mut NvEncLockBitstream) -> u32>,
    nvEncUnlockBitstream: Option<unsafe extern "C" fn(NvEncoder, *mut c_void) -> u32>,
    nvEncLockInputBuffer: Option<unsafe extern "C" fn(NvEncoder, *mut NvEncLockInputBuffer) -> u32>,
    nvEncUnlockInputBuffer: Option<unsafe extern "C" fn(NvEncoder, *mut c_void) -> u32>,
    // There are more entry points but we don't need them.
    nvEncDestroyEncoder: Option<unsafe extern "C" fn(NvEncoder) -> u32>,
    nvEncReconfigureEncoder: Option<unsafe extern "C" fn(NvEncoder, *mut NvEncReconfigParams) -> u32>,
    _pad: [*mut c_void; 16],
}

// ── NVENC parameter structs (simplified) ──────────────────────────────────────

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncOpenSessionParams {
    version: u32,
    deviceType: u32,
    device: *mut c_void,
    reserved: *mut c_void,
    apiVersion: u32,
    reserved1: [u32; 253],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncConfig {
    version: u32,
    profileGUID: [u8; 16],
    gopLength: u32,
    frameIntervalP: i32,
    monoChromeEncoding: u32,
    frameFieldMode: u32,
    mvPrecision: u32,
    rcParams: NvEncRcParams,
    _pad: [u8; 4096], // Reserve space for the full struct
}

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncRcParams {
    version: u32,
    rateControlMode: u32,
    constQP_interP: u32,
    constQP_interB: u32,
    constQP_intra: u32,
    averageBitRate: u32,
    maxBitRate: u32,
    vbvBufferSize: u32,
    vbvInitialDelay: u32,
    _pad: [u8; 256],
}

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncInitParams {
    version: u32,
    encodeGUID: [u8; 16],
    presetGUID: [u8; 16],
    encodeWidth: u32,
    encodeHeight: u32,
    darWidth: u32,
    darHeight: u32,
    frameRateNum: u32,
    frameRateDen: u32,
    enableEncodeAsync: u32,
    enablePTD: u32,
    reportSliceOffsets: u32,
    enableSubFrameWrite: u32,
    enableExternalMEHints: u32,
    enableMEOnlyMode: u32,
    enableWeightedPrediction: u32,
    enableOutputInVidmem: u32,
    reservedBitFields: u32,
    privDataSize: u32,
    privData: *mut c_void,
    encodeConfig: *mut NvEncConfig,
    maxEncodeWidth: u32,
    maxEncodeHeight: u32,
    maxMEHintCountsPerBlock: [u32; 2],
    tuningInfo: u32,
    _pad: [u8; 512],
}

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncPresetConfig {
    version: u32,
    presetCfg: NvEncConfig,
}

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncCreateInputBuffer {
    version: u32,
    width: u32,
    height: u32,
    memoryHeap: u32,
    bufferFmt: u32,
    reserved: u32,
    inputBuffer: *mut c_void,
    pSysMemBuffer: *mut c_void,
    reserved1: [u32; 57],
    reserved2: [*mut c_void; 63],
}

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncCreateBitstreamBuffer {
    version: u32,
    size: u32,
    memoryHeap: u32,
    reserved: u32,
    bitstreamBuffer: *mut c_void,
    bitstreamBufferPtr: *mut c_void,
    reserved1: [u32; 58],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncPicParams {
    version: u32,
    inputWidth: u32,
    inputHeight: u32,
    inputPitch: u32,
    encodePicFlags: u32,
    frameIdx: u32,
    inputTimeStamp: u64,
    inputDuration: u64,
    inputBuffer: *mut c_void,
    outputBitstream: *mut c_void,
    completionEvent: *mut c_void,
    bufferFmt: u32,
    pictureStruct: u32, // 1 = frame
    pictureType: u32,
    codecPicParams: [u8; 256],
    _pad: [u8; 512],
}

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncLockBitstream {
    version: u32,
    doNotWait: u32,
    ltrFrame: u32,
    reservedBitFields: u32,
    outputBitstream: *mut c_void,
    sliceOffsets: *mut u32,
    frameIdx: u32,
    hwEncodeStatus: u32,
    numSlices: u32,
    bitstreamSizeInBytes: u32,
    outputTimeStamp: u64,
    outputDuration: u64,
    bitstreamBufferPtr: *mut c_void,
    pictureType: u32,
    pictureStruct: u32,
    frameAvgQP: u32,
    frameSatd: u32,
    ltrFrameIdx: u32,
    ltrFrameBitmap: u32,
    reserved: [u32; 13],
    intraMBCount: u32,
    interMBCount: u32,
    averageMVX: i32,
    averageMVY: i32,
    reserved1: [u32; 226],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncLockInputBuffer {
    version: u32,
    doNotWait: u32,
    reservedBitFields: u32,
    inputBuffer: *mut c_void,
    bufferDataPtr: *mut c_void,
    pitch: u32,
    reserved1: [u32; 251],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[allow(non_snake_case)]
struct NvEncReconfigParams {
    version: u32,
    reInitEncodeParams: NvEncInitParams,
    resetEncoder: u32,
    forceIDR: u32,
    _pad: [u8; 256],
}

// ── CUDA minimal FFI ──────────────────────────────────────────────────────────

type CuResult = i32;

// ── Library handle wrappers ───────────────────────────────────────────────────

struct NvencLibrary {
    _lib: libloading::Library,
    fns: Box<NvEncFunctions>,
}

struct CudaContext {
    _lib: libloading::Library,
    device: CuDevice,
    context: CuContext,
    cu_ctx_destroy: unsafe extern "C" fn(CuContext) -> CuResult,
}

impl Drop for CudaContext {
    fn drop(&mut self) {
        unsafe { (self.cu_ctx_destroy)(self.context) };
    }
}

/// Attempt to initialise a CUDA context on device 0.
fn init_cuda() -> Result<CudaContext> {
    #[cfg(target_os = "linux")]
    const CUDA_LIB: &str = "libcuda.so.1";
    #[cfg(target_os = "windows")]
    const CUDA_LIB: &str = "nvcuda.dll";
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    const CUDA_LIB: &str = "";

    if CUDA_LIB.is_empty() {
        bail!("CUDA not supported on this platform");
    }

    let lib = unsafe { libloading::Library::new(CUDA_LIB) }
        .map_err(|e| anyhow::anyhow!("failed to load CUDA library ({CUDA_LIB}): {e}"))?;

    type CuInit = unsafe extern "C" fn(u32) -> CuResult;
    type CuDeviceGet = unsafe extern "C" fn(*mut CuDevice, i32) -> CuResult;
    type CuCtxCreate = unsafe extern "C" fn(*mut CuContext, u32, CuDevice) -> CuResult;
    type CuCtxDestroy = unsafe extern "C" fn(CuContext) -> CuResult;

    let cu_init: CuInit = unsafe { *lib.get(b"cuInit\0")? };
    let cu_device_get: CuDeviceGet = unsafe { *lib.get(b"cuDeviceGet\0")? };
    let cu_ctx_create: CuCtxCreate = unsafe { *lib.get(b"cuCtxCreate_v2\0")? };
    let cu_ctx_destroy: CuCtxDestroy = unsafe { *lib.get(b"cuCtxDestroy_v2\0")? };

    let rc = unsafe { cu_init(0) };
    if rc != 0 {
        bail!("cuInit failed: {rc}");
    }

    let mut device: CuDevice = 0;
    let rc = unsafe { cu_device_get(&mut device, 0) };
    if rc != 0 {
        bail!("cuDeviceGet(0) failed: {rc}");
    }

    let mut context: CuContext = ptr::null_mut();
    let rc = unsafe { cu_ctx_create(&mut context, 0, device) };
    if rc != 0 {
        bail!("cuCtxCreate failed: {rc}");
    }

    Ok(CudaContext {
        _lib: lib,
        device,
        context,
        cu_ctx_destroy,
    })
}

/// Load the NVENC library and populate the function table.
fn load_nvenc_functions() -> Result<NvencLibrary> {
    #[cfg(target_os = "linux")]
    const CANDIDATES: &[&str] = &["libnvidia-encode.so.1", "libnvidia-encode.so"];
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[&str] = &["nvEncodeAPI64.dll", "nvEncodeAPI.dll"];
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    const CANDIDATES: &[&str] = &[];

    let lib = CANDIDATES
        .iter()
        .find_map(|name| unsafe { libloading::Library::new(name).ok() })
        .ok_or_else(|| anyhow::anyhow!("failed to load NVENC runtime library"))?;

    type CreateInstanceFn = unsafe extern "C" fn(*mut NvEncFunctions) -> u32;
    let create_instance: CreateInstanceFn =
        unsafe { *lib.get(b"NvEncodeAPICreateInstance\0")? };

    let mut fns = Box::new(unsafe { std::mem::zeroed::<NvEncFunctions>() });
    // The version field must be set to NVENCAPI_STRUCT_VERSION(NV_ENCODE_API_FUNCTION_LIST, 2).
    // For SDK 12.x this is (2 | (12 << 16) | (0x7 << 28)).
    fns.version = 2 | (12 << 16);
    let rc = unsafe { create_instance(&mut *fns) };
    if rc != NV_ENC_SUCCESS {
        bail!("NvEncodeAPICreateInstance failed: {rc}");
    }

    Ok(NvencLibrary { _lib: lib, fns })
}

// ── Encoder state ─────────────────────────────────────────────────────────────

pub struct NvencAv1Encoder {
    encoder: NvEncoder,
    nvenc: NvencLibrary,
    _cuda: CudaContext,
    input_buffer: *mut c_void,
    output_buffer: *mut c_void,
    config: VideoSessionConfig,
    enc_config: NvEncConfig,
    frame_idx: u32,
    force_next_keyframe: bool,
    initialized: bool,
}

// SAFETY: The NVENC encoder handle is only accessed via `&mut self` methods
// and all GPU resources are tied to the CUDA context which we own exclusively.
unsafe impl Send for NvencAv1Encoder {}

impl NvencAv1Encoder {
    /// Open a real NVENC AV1 session. This initialises CUDA, loads the NVENC
    /// library, creates an encode session, and verifies AV1 support.
    fn open() -> Result<Self> {
        let cuda = init_cuda()?;
        let nvenc = load_nvenc_functions()?;

        // Open encode session
        let mut session_params = unsafe { std::mem::zeroed::<NvEncOpenSessionParams>() };
        session_params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        session_params.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
        session_params.device = cuda.context;
        session_params.apiVersion = (12 << 4) | 2; // NVENCAPI_VERSION 12.2

        let mut encoder: NvEncoder = ptr::null_mut();
        let open_fn = nvenc
            .fns
            .nvEncOpenEncodeSessionEx
            .ok_or_else(|| anyhow::anyhow!("nvEncOpenEncodeSessionEx not available"))?;
        let rc = unsafe { open_fn(&mut session_params, &mut encoder) };
        if rc != NV_ENC_SUCCESS {
            bail!(
                "nvEncOpenEncodeSessionEx failed: {rc}{}",
                if rc == NV_ENC_ERR_NO_ENCODE_DEVICE {
                    " (no encode-capable device)"
                } else {
                    ""
                }
            );
        }

        info!("[nvenc-av1] opened NVENC encode session");

        Ok(Self {
            encoder,
            nvenc,
            _cuda: cuda,
            input_buffer: ptr::null_mut(),
            output_buffer: ptr::null_mut(),
            config: VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 60,
                target_bitrate_bps: 8_000_000,
                low_latency: true,
                allow_frame_drop: true,
            },
            enc_config: unsafe { std::mem::zeroed() },
            frame_idx: 0,
            force_next_keyframe: false,
            initialized: false,
        })
    }

    /// (Re-)initialise the NVENC encoder with current config dimensions.
    fn initialize_encoder(&mut self) -> Result<()> {
        if self.config.width == 0 || self.config.height == 0 {
            return Ok(());
        }

        // Destroy previous buffers if re-initialising.
        self.destroy_buffers();

        // Get preset config for low-latency
        let get_preset_fn = self
            .nvenc
            .fns
            .nvEncGetEncodePresetConfigEx
            .ok_or_else(|| anyhow::anyhow!("nvEncGetEncodePresetConfigEx missing"))?;

        let mut preset_config = unsafe { std::mem::zeroed::<NvEncPresetConfig>() };
        preset_config.version = NV_ENC_CONFIG_VER; // preset config version
        preset_config.presetCfg.version = NV_ENC_CONFIG_VER;
        let rc = unsafe {
            get_preset_fn(
                self.encoder,
                NV_ENC_CODEC_AV1_GUID,
                NV_ENC_PRESET_P1_GUID,
                NV_ENC_TUNING_INFO_LOW_LATENCY,
                &mut preset_config,
            )
        };
        if rc != NV_ENC_SUCCESS {
            bail!("nvEncGetEncodePresetConfigEx failed: {rc}");
        }

        let mut enc_config = preset_config.presetCfg;
        enc_config.version = NV_ENC_CONFIG_VER;
        enc_config.gopLength = 300; // ~5s at 60fps
        enc_config.frameIntervalP = 1; // No B-frames for low latency
        enc_config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
        enc_config.rcParams.averageBitRate = self.config.target_bitrate_bps;
        enc_config.rcParams.maxBitRate = self.config.target_bitrate_bps;
        enc_config.rcParams.vbvBufferSize =
            self.config.target_bitrate_bps / self.config.fps.max(1);
        enc_config.rcParams.vbvInitialDelay =
            enc_config.rcParams.vbvBufferSize;

        let mut init_params = unsafe { std::mem::zeroed::<NvEncInitParams>() };
        init_params.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init_params.encodeGUID = NV_ENC_CODEC_AV1_GUID;
        init_params.presetGUID = NV_ENC_PRESET_P1_GUID;
        init_params.encodeWidth = self.config.width;
        init_params.encodeHeight = self.config.height;
        init_params.darWidth = self.config.width;
        init_params.darHeight = self.config.height;
        init_params.frameRateNum = self.config.fps;
        init_params.frameRateDen = 1;
        init_params.enablePTD = 1; // picture type decision by encoder
        init_params.encodeConfig = &mut enc_config;
        init_params.maxEncodeWidth = self.config.width;
        init_params.maxEncodeHeight = self.config.height;
        init_params.tuningInfo = NV_ENC_TUNING_INFO_LOW_LATENCY;

        let init_fn = self
            .nvenc
            .fns
            .nvEncInitializeEncoder
            .ok_or_else(|| anyhow::anyhow!("nvEncInitializeEncoder missing"))?;
        let rc = unsafe { init_fn(self.encoder, &mut init_params) };
        if rc != NV_ENC_SUCCESS {
            bail!("nvEncInitializeEncoder failed: {rc}");
        }

        self.enc_config = enc_config;

        // Allocate input buffer
        let create_input_fn = self
            .nvenc
            .fns
            .nvEncCreateInputBuffer
            .ok_or_else(|| anyhow::anyhow!("nvEncCreateInputBuffer missing"))?;
        let mut input_buf = unsafe { std::mem::zeroed::<NvEncCreateInputBuffer>() };
        input_buf.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
        input_buf.width = self.config.width;
        input_buf.height = self.config.height;
        input_buf.bufferFmt = NV_ENC_BUFFER_FORMAT_ARGB;
        let rc = unsafe { create_input_fn(self.encoder, &mut input_buf) };
        if rc != NV_ENC_SUCCESS {
            bail!("nvEncCreateInputBuffer failed: {rc}");
        }
        self.input_buffer = input_buf.inputBuffer;

        // Allocate output bitstream buffer
        let create_output_fn = self
            .nvenc
            .fns
            .nvEncCreateBitstreamBuffer
            .ok_or_else(|| anyhow::anyhow!("nvEncCreateBitstreamBuffer missing"))?;
        let mut output_buf = unsafe { std::mem::zeroed::<NvEncCreateBitstreamBuffer>() };
        output_buf.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        let rc = unsafe { create_output_fn(self.encoder, &mut output_buf) };
        if rc != NV_ENC_SUCCESS {
            bail!("nvEncCreateBitstreamBuffer failed: {rc}");
        }
        self.output_buffer = output_buf.bitstreamBuffer;

        self.initialized = true;
        self.frame_idx = 0;
        info!(
            width = self.config.width,
            height = self.config.height,
            fps = self.config.fps,
            bitrate_bps = self.config.target_bitrate_bps,
            "[nvenc-av1] encoder initialized"
        );
        Ok(())
    }

    fn destroy_buffers(&mut self) {
        if !self.input_buffer.is_null() {
            if let Some(destroy) = self.nvenc.fns.nvEncDestroyInputBuffer {
                unsafe { destroy(self.encoder, self.input_buffer) };
            }
            self.input_buffer = ptr::null_mut();
        }
        if !self.output_buffer.is_null() {
            if let Some(destroy) = self.nvenc.fns.nvEncDestroyBitstreamBuffer {
                unsafe { destroy(self.encoder, self.output_buffer) };
            }
            self.output_buffer = ptr::null_mut();
        }
        self.initialized = false;
    }

    /// Copy BGRA frame data into the NVENC input buffer.
    fn upload_frame(&mut self, frame: &VideoFrame) -> Result<u32> {
        let lock_fn = self
            .nvenc
            .fns
            .nvEncLockInputBuffer
            .ok_or_else(|| anyhow::anyhow!("nvEncLockInputBuffer missing"))?;
        let unlock_fn = self
            .nvenc
            .fns
            .nvEncUnlockInputBuffer
            .ok_or_else(|| anyhow::anyhow!("nvEncUnlockInputBuffer missing"))?;

        let mut lock_params = unsafe { std::mem::zeroed::<NvEncLockInputBuffer>() };
        lock_params.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
        lock_params.inputBuffer = self.input_buffer;

        let rc = unsafe { lock_fn(self.encoder, &mut lock_params) };
        if rc != NV_ENC_SUCCESS {
            bail!("nvEncLockInputBuffer failed: {rc}");
        }

        let pitch = lock_params.pitch;
        let dst_ptr = lock_params.bufferDataPtr as *mut u8;

        let FramePlanes::Bgra { bytes, stride } = &frame.planes else {
            unsafe { unlock_fn(self.encoder, self.input_buffer) };
            bail!("NVENC AV1 encoder expects BGRA input");
        };

        let src_stride = *stride as usize;
        let width = frame.width as usize;
        let height = frame.height as usize;
        let row_bytes = width * 4;

        // BGRA → ARGB swizzle while copying row-by-row.
        // NVENC ARGB format is A[31:24] R[23:16] G[15:8] B[7:0] in memory
        // which is the same byte order as BGRA when read as little-endian u32.
        // So BGRA src maps directly to NV_ENC_BUFFER_FORMAT_ARGB on LE.
        for y in 0..height {
            let src_row = &bytes[y * src_stride..y * src_stride + row_bytes];
            let dst_row =
                unsafe { std::slice::from_raw_parts_mut(dst_ptr.add(y * pitch as usize), row_bytes) };
            dst_row.copy_from_slice(src_row);
        }

        let rc = unsafe { unlock_fn(self.encoder, self.input_buffer) };
        if rc != NV_ENC_SUCCESS {
            warn!("[nvenc-av1] nvEncUnlockInputBuffer returned {rc}");
        }

        Ok(pitch)
    }
}

impl Drop for NvencAv1Encoder {
    fn drop(&mut self) {
        self.destroy_buffers();
        if let Some(destroy) = self.nvenc.fns.nvEncDestroyEncoder {
            unsafe { destroy(self.encoder) };
        }
    }
}

impl VideoEncoder for NvencAv1Encoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        let needs_reinit = !self.initialized
            || config.width != self.config.width
            || config.height != self.config.height
            || config.fps != self.config.fps;

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

        if !self.initialized {
            return Ok(());
        }

        // Reconfigure encoder with new bitrate without full reinit
        self.enc_config.rcParams.averageBitRate = bitrate_bps;
        self.enc_config.rcParams.maxBitRate = bitrate_bps;
        self.enc_config.rcParams.vbvBufferSize = bitrate_bps / self.config.fps.max(1);
        self.enc_config.rcParams.vbvInitialDelay =
            self.enc_config.rcParams.vbvBufferSize;

        if let Some(reconfig_fn) = self.nvenc.fns.nvEncReconfigureEncoder {
            let mut reconfig = unsafe { std::mem::zeroed::<NvEncReconfigParams>() };
            // Build init params referencing updated enc_config
            reconfig.reInitEncodeParams.version = NV_ENC_INITIALIZE_PARAMS_VER;
            reconfig.reInitEncodeParams.encodeGUID = NV_ENC_CODEC_AV1_GUID;
            reconfig.reInitEncodeParams.presetGUID = NV_ENC_PRESET_P1_GUID;
            reconfig.reInitEncodeParams.encodeWidth = self.config.width;
            reconfig.reInitEncodeParams.encodeHeight = self.config.height;
            reconfig.reInitEncodeParams.darWidth = self.config.width;
            reconfig.reInitEncodeParams.darHeight = self.config.height;
            reconfig.reInitEncodeParams.frameRateNum = self.config.fps;
            reconfig.reInitEncodeParams.frameRateDen = 1;
            reconfig.reInitEncodeParams.enablePTD = 1;
            reconfig.reInitEncodeParams.encodeConfig = &mut self.enc_config;
            reconfig.reInitEncodeParams.tuningInfo = NV_ENC_TUNING_INFO_LOW_LATENCY;

            let rc = unsafe { reconfig_fn(self.encoder, &mut reconfig) };
            if rc != NV_ENC_SUCCESS {
                warn!("[nvenc-av1] nvEncReconfigureEncoder returned {rc}, falling back to cached bitrate");
            }
        }

        Ok(())
    }

    fn encode(&mut self, frame: VideoFrame) -> Result<EncodedAccessUnit> {
        if frame.format != PixelFormat::Bgra {
            bail!("NVENC AV1 encoder expects BGRA input, got {:?}", frame.format);
        }

        // Auto-init if configure_session wasn't called with real dimensions
        if !self.initialized
            || self.config.width != frame.width
            || self.config.height != frame.height
        {
            self.config.width = frame.width;
            self.config.height = frame.height;
            self.initialize_encoder()?;
        }

        let pitch = self.upload_frame(&frame)?;

        let force_kf = self.force_next_keyframe;
        self.force_next_keyframe = false;

        // Encode
        let encode_fn = self
            .nvenc
            .fns
            .nvEncEncodePicture
            .ok_or_else(|| anyhow::anyhow!("nvEncEncodePicture missing"))?;

        let mut pic_params = unsafe { std::mem::zeroed::<NvEncPicParams>() };
        pic_params.version = NV_ENC_PIC_PARAMS_VER;
        pic_params.inputWidth = frame.width;
        pic_params.inputHeight = frame.height;
        pic_params.inputPitch = pitch;
        pic_params.inputBuffer = self.input_buffer;
        pic_params.outputBitstream = self.output_buffer;
        pic_params.bufferFmt = NV_ENC_BUFFER_FORMAT_ARGB;
        pic_params.pictureStruct = 1; // frame
        pic_params.inputTimeStamp = frame.ts_ms as u64;
        pic_params.frameIdx = self.frame_idx;

        if force_kf {
            pic_params.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
            pic_params.pictureType = NV_ENC_PIC_TYPE_IDR;
        }

        let rc = unsafe { encode_fn(self.encoder, &mut pic_params) };
        if rc != NV_ENC_SUCCESS {
            bail!("nvEncEncodePicture failed: {rc}");
        }

        // Lock and read bitstream output
        let lock_fn = self
            .nvenc
            .fns
            .nvEncLockBitstream
            .ok_or_else(|| anyhow::anyhow!("nvEncLockBitstream missing"))?;
        let unlock_fn = self
            .nvenc
            .fns
            .nvEncUnlockBitstream
            .ok_or_else(|| anyhow::anyhow!("nvEncUnlockBitstream missing"))?;

        let mut lock_bs = unsafe { std::mem::zeroed::<NvEncLockBitstream>() };
        lock_bs.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock_bs.outputBitstream = self.output_buffer;

        let rc = unsafe { lock_fn(self.encoder, &mut lock_bs) };
        if rc != NV_ENC_SUCCESS {
            bail!("nvEncLockBitstream failed: {rc}");
        }

        let size = lock_bs.bitstreamSizeInBytes as usize;
        let is_keyframe = force_kf || lock_bs.pictureType == NV_ENC_PIC_TYPE_IDR;

        let data = if size > 0 && !lock_bs.bitstreamBufferPtr.is_null() {
            let slice =
                unsafe { std::slice::from_raw_parts(lock_bs.bitstreamBufferPtr as *const u8, size) };
            bytes::Bytes::copy_from_slice(slice)
        } else {
            bytes::Bytes::new()
        };

        let rc = unsafe { unlock_fn(self.encoder, self.output_buffer) };
        if rc != NV_ENC_SUCCESS {
            warn!("[nvenc-av1] nvEncUnlockBitstream returned {rc}");
        }

        self.frame_idx = self.frame_idx.wrapping_add(1);

        Ok(EncodedAccessUnit {
            codec: pb::VideoCodec::Av1,
            layer_id: 0,
            ts_ms: frame.ts_ms,
            is_keyframe,
            data,
        })
    }

    fn backend_name(&self) -> &'static str {
        "av1-nvenc"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen_share::runtime_probe::nvidia::probe_nvenc_av1;

    /// Verify that when NVENC is not available (typical CI), build_nvenc_encoder
    /// fails cleanly rather than panicking.
    #[test]
    fn build_fails_cleanly_without_nvenc() {
        let status = probe_nvenc_av1();
        if status.available {
            return; // NVENC is present; skip this test.
        }
        let result = build_nvenc_encoder();
        assert!(result.is_err());
    }

    /// If NVENC *is* available (e.g., dev machine with RTX 40), verify
    /// the full open → configure → encode → keyframe cycle.
    #[test]
    fn nvenc_roundtrip_if_available() {
        let status = probe_nvenc_av1();
        if !status.available {
            return;
        }
        let mut enc = build_nvenc_encoder().expect("should open NVENC session");
        enc.configure_session(VideoSessionConfig {
            width: 64,
            height: 64,
            fps: 30,
            target_bitrate_bps: 2_000_000,
            low_latency: true,
            allow_frame_drop: true,
        })
        .expect("configure should succeed");

        let frame = crate::net::video_frame::VideoFrame {
            width: 64,
            height: 64,
            ts_ms: 1,
            format: PixelFormat::Bgra,
            planes: FramePlanes::Bgra {
                bytes: bytes::Bytes::from(vec![128u8; 64 * 64 * 4]),
                stride: 64 * 4,
            },
        };

        enc.request_keyframe().unwrap();
        let au = enc.encode(frame).expect("encode should succeed");
        assert!(au.is_keyframe);
        assert!(!au.data.is_empty());
        assert_eq!(au.codec, pb::VideoCodec::Av1);

        // Bitrate update
        enc.update_bitrate(500_000).unwrap();

        let frame2 = crate::net::video_frame::VideoFrame {
            width: 64,
            height: 64,
            ts_ms: 2,
            format: PixelFormat::Bgra,
            planes: FramePlanes::Bgra {
                bytes: bytes::Bytes::from(vec![64u8; 64 * 64 * 4]),
                stride: 64 * 4,
            },
        };
        let au2 = enc.encode(frame2).expect("post-bitrate-update encode");
        assert!(!au2.data.is_empty());
    }
}
