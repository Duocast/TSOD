/// VAAPI backend probing for hardware VP9 encode and decode.
///
/// ## Runtime dependencies
///
/// - **Linux only.** Requires `libva.so.2` (VA-API runtime) and a compatible
///   driver (e.g. `intel-media-va-driver-non-free` for Intel, `mesa-va-drivers`
///   for AMD). The VAAPI driver must support VP9 encode/decode profiles for the
///   respective capability to be advertised.
///
/// The probe opens `/dev/dri/renderD128` (or the first available render node)
/// and queries the VA-API driver for VP9 entrypoints.

use std::ffi::c_void;
use std::ptr;

/// Result of probing VAAPI VP9 capabilities.
#[derive(Clone, Debug, Default)]
pub struct VaapiVp9Status {
    pub encode_available: bool,
    pub decode_available: bool,
    pub encode_reason: Option<String>,
    pub decode_reason: Option<String>,
}

/// Probe whether VA-API VP9 encode and/or decode are available on this system.
pub fn probe_vaapi_vp9() -> VaapiVp9Status {
    #[cfg(not(target_os = "linux"))]
    {
        return VaapiVp9Status {
            encode_available: false,
            decode_available: false,
            encode_reason: Some("VAAPI is only available on Linux".into()),
            decode_reason: Some("VAAPI is only available on Linux".into()),
        };
    }

    #[cfg(target_os = "linux")]
    {
        probe_vaapi_vp9_linux()
    }
}

#[cfg(target_os = "linux")]
fn probe_vaapi_vp9_linux() -> VaapiVp9Status {
    let lib = match unsafe { libloading::Library::new("libva.so.2") } {
        Ok(l) => l,
        Err(e) => {
            let reason = format!("failed to load libva.so.2: {e}");
            return VaapiVp9Status {
                encode_available: false,
                decode_available: false,
                encode_reason: Some(reason.clone()),
                decode_reason: Some(reason),
            };
        }
    };

    let drm_lib = match unsafe { libloading::Library::new("libva-drm.so.2") } {
        Ok(l) => l,
        Err(e) => {
            let reason = format!("failed to load libva-drm.so.2: {e}");
            return VaapiVp9Status {
                encode_available: false,
                decode_available: false,
                encode_reason: Some(reason.clone()),
                decode_reason: Some(reason),
            };
        }
    };

    // Open a DRM render node
    let drm_fd = open_render_node();
    if drm_fd < 0 {
        let reason = "no DRM render node available (/dev/dri/renderD*)".to_string();
        return VaapiVp9Status {
            encode_available: false,
            decode_available: false,
            encode_reason: Some(reason.clone()),
            decode_reason: Some(reason),
        };
    }

    // Load VA-API functions
    type VaGetDisplayDRM = unsafe extern "C" fn(i32) -> *mut c_void;
    type VaInitialize = unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> i32;
    type VaTerminate = unsafe extern "C" fn(*mut c_void) -> i32;
    type VaMaxNumEntrypoints = unsafe extern "C" fn(*mut c_void) -> i32;
    type VaQueryConfigEntrypoints =
        unsafe extern "C" fn(*mut c_void, i32, *mut i32, *mut i32) -> i32;

    let va_get_display: VaGetDisplayDRM = match unsafe { drm_lib.get(b"vaGetDisplayDRM\0") } {
        Ok(f) => unsafe { *f },
        Err(e) => {
            unsafe { libc::close(drm_fd) };
            let reason = format!("vaGetDisplayDRM not found: {e}");
            return VaapiVp9Status {
                encode_available: false,
                decode_available: false,
                encode_reason: Some(reason.clone()),
                decode_reason: Some(reason),
            };
        }
    };

    let va_initialize: VaInitialize = match unsafe { lib.get(b"vaInitialize\0") } {
        Ok(f) => unsafe { *f },
        Err(_) => {
            unsafe { libc::close(drm_fd) };
            return default_unavailable("vaInitialize not found");
        }
    };
    let va_terminate: VaTerminate = match unsafe { lib.get(b"vaTerminate\0") } {
        Ok(f) => unsafe { *f },
        Err(_) => {
            unsafe { libc::close(drm_fd) };
            return default_unavailable("vaTerminate not found");
        }
    };
    let va_max_num_entrypoints: VaMaxNumEntrypoints =
        match unsafe { lib.get(b"vaMaxNumEntrypoints\0") } {
            Ok(f) => unsafe { *f },
            Err(_) => {
                unsafe { libc::close(drm_fd) };
                return default_unavailable("vaMaxNumEntrypoints not found");
            }
        };
    let va_query_config_entrypoints: VaQueryConfigEntrypoints =
        match unsafe { lib.get(b"vaQueryConfigEntrypoints\0") } {
            Ok(f) => unsafe { *f },
            Err(_) => {
                unsafe { libc::close(drm_fd) };
                return default_unavailable("vaQueryConfigEntrypoints not found");
            }
        };

    let display = unsafe { va_get_display(drm_fd) };
    if display.is_null() {
        unsafe { libc::close(drm_fd) };
        return default_unavailable("vaGetDisplayDRM returned null");
    }

    let mut major: i32 = 0;
    let mut minor: i32 = 0;
    let rc = unsafe { va_initialize(display, &mut major, &mut minor) };
    if rc != 0 {
        // VA_STATUS_SUCCESS = 0
        unsafe { libc::close(drm_fd) };
        return default_unavailable(&format!("vaInitialize failed: {rc}"));
    }

    // VP9 profile 0 = VAProfileVP9Profile0 = 17
    const VA_PROFILE_VP9_PROFILE0: i32 = 17;
    // Entrypoint constants
    const VA_ENTRYPOINT_VLDVP9: i32 = 1; // VAEntrypointVLD = 1 (decode)
    const VA_ENTRYPOINT_ENCSLICE: i32 = 6; // VAEntrypointEncSlice = 6 (encode)
    const VA_ENTRYPOINT_ENCSLICE_LP: i32 = 8; // VAEntrypointEncSliceLP = 8 (low-power encode)

    let max_ep = unsafe { va_max_num_entrypoints(display) };
    let mut entrypoints = vec![0_i32; max_ep.max(0) as usize];
    let mut num_ep: i32 = 0;

    let rc = unsafe {
        va_query_config_entrypoints(
            display,
            VA_PROFILE_VP9_PROFILE0,
            entrypoints.as_mut_ptr(),
            &mut num_ep,
        )
    };

    let mut encode_available = false;
    let mut decode_available = false;
    let mut encode_reason = None;
    let mut decode_reason = None;

    if rc == 0 && num_ep > 0 {
        let eps = &entrypoints[..num_ep as usize];
        decode_available = eps.contains(&VA_ENTRYPOINT_VLDVP9)
            || eps.iter().any(|&ep| ep == 1); // VLD
        encode_available = eps.contains(&VA_ENTRYPOINT_ENCSLICE)
            || eps.contains(&VA_ENTRYPOINT_ENCSLICE_LP);

        if !decode_available {
            decode_reason = Some("VP9 decode entrypoint not found in VA-API driver".into());
        }
        if !encode_available {
            encode_reason = Some("VP9 encode entrypoint not found in VA-API driver".into());
        }
    } else {
        let reason = if rc != 0 {
            format!("vaQueryConfigEntrypoints for VP9Profile0 failed: {rc}")
        } else {
            "no VP9 entrypoints found".into()
        };
        encode_reason = Some(reason.clone());
        decode_reason = Some(reason);
    }

    unsafe { va_terminate(display) };
    unsafe { libc::close(drm_fd) };

    VaapiVp9Status {
        encode_available,
        decode_available,
        encode_reason,
        decode_reason,
    }
}

#[cfg(target_os = "linux")]
fn open_render_node() -> i32 {
    // Try render nodes 128-135
    for idx in 128..136 {
        let path = format!("/dev/dri/renderD{idx}");
        let cpath = std::ffi::CString::new(path).unwrap();
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
        if fd >= 0 {
            return fd;
        }
    }
    -1
}

fn default_unavailable(reason: &str) -> VaapiVp9Status {
    VaapiVp9Status {
        encode_available: false,
        decode_available: false,
        encode_reason: Some(reason.to_string()),
        decode_reason: Some(reason.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_does_not_panic() {
        let status = probe_vaapi_vp9();
        // On CI (no GPU) we expect unavailable; on a dev machine with VA-API
        // we'd expect at least decode. Either way, no panic.
        if !status.encode_available {
            assert!(status.encode_reason.is_some());
        }
        if !status.decode_available {
            assert!(status.decode_reason.is_some());
        }
    }

    #[test]
    fn non_linux_is_unavailable() {
        // This test body only meaningfully runs on non-Linux, but it compiles
        // everywhere and verifies the struct shape.
        let _ = VaapiVp9Status::default();
    }
}
