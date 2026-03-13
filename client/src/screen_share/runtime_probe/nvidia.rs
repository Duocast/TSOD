#[derive(Clone, Debug, Default)]
pub struct NvencAv1Status {
    pub available: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NvidiaAdapterInfo {
    pub vendor_id: u32,
    pub device_id: u32,
    pub name: String,
}

pub fn probe_nvenc_av1() -> NvencAv1Status {
    let adapter = match detect_nvidia_adapter() {
        Some(adapter) => adapter,
        None => {
            return NvencAv1Status {
                available: false,
                reason: Some("no NVIDIA adapter detected".into()),
            };
        }
    };

    if !is_rtx_40_or_50_series(&adapter) {
        return NvencAv1Status {
            available: false,
            reason: Some(format!(
                "NVIDIA adapter is not GeForce RTX 40/50 series (device_id=0x{:04x})",
                adapter.device_id
            )),
        };
    }

    match probe_nvenc_library_loaded() {
        Ok(()) => NvencAv1Status {
            available: true,
            reason: None,
        },
        Err(reason) => NvencAv1Status {
            available: false,
            reason: Some(reason),
        },
    }
}

fn detect_nvidia_adapter() -> Option<NvidiaAdapterInfo> {
    if let Ok(vendor) = std::env::var("TSOD_TEST_NVIDIA_VENDOR_ID") {
        let vendor_id = u32::from_str_radix(vendor.trim_start_matches("0x"), 16).ok()?;
        let device_id = std::env::var("TSOD_TEST_NVIDIA_DEVICE_ID")
            .ok()
            .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            .unwrap_or_default();
        let name = std::env::var("TSOD_TEST_NVIDIA_NAME").unwrap_or_default();
        return Some(NvidiaAdapterInfo {
            vendor_id,
            device_id,
            name,
        });
    }
    None
}

pub fn is_rtx_40_or_50_series(adapter: &NvidiaAdapterInfo) -> bool {
    if adapter.vendor_id != 0x10DE {
        return false;
    }

    matches!(adapter.device_id, 0x2684..=0x28FF | 0x2900..=0x2CFF)
}

fn probe_nvenc_library_loaded() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[&str] = &["nvEncodeAPI64.dll", "nvEncodeAPI.dll"];
    #[cfg(target_os = "linux")]
    const CANDIDATES: &[&str] = &["libnvidia-encode.so.1", "libnvidia-encode.so"];
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    const CANDIDATES: &[&str] = &[];

    if CANDIDATES.is_empty() {
        return Err("NVENC probing unsupported on this platform".into());
    }

    for name in CANDIDATES {
        let loaded = unsafe { libloading::Library::new(name) };
        if loaded.is_ok() {
            return Ok(());
        }
    }

    Err("unable to load NVENC runtime library".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtx_40_50_series_detection_gate() {
        let supported = NvidiaAdapterInfo {
            vendor_id: 0x10DE,
            device_id: 0x2684,
            name: "RTX 4090".into(),
        };
        let unsupported_gen = NvidiaAdapterInfo {
            vendor_id: 0x10DE,
            device_id: 0x2204,
            name: "RTX 3080".into(),
        };
        let not_nvidia = NvidiaAdapterInfo {
            vendor_id: 0x1002,
            device_id: 0x744C,
            name: "RX 7900".into(),
        };

        assert!(is_rtx_40_or_50_series(&supported));
        assert!(!is_rtx_40_or_50_series(&unsupported_gen));
        assert!(!is_rtx_40_or_50_series(&not_nvidia));
    }
}
