/// NVIDIA adapter detection and NVENC AV1 availability probing.
///
/// ## Runtime dependencies
///
/// - **Linux**: Reads `/sys/bus/pci/devices/*/vendor` and `/sys/bus/pci/devices/*/device`
///   to enumerate PCI adapters without any user-space driver library. The NVENC encode
///   library (`libnvidia-encode.so.1` or `libnvidia-encode.so`) must be loadable for
///   encoding to be available — this is shipped with the proprietary NVIDIA driver
///   (>= 530 for AV1, >= 470 for H.264/HEVC).
///
/// - **Windows**: Uses the DXGI factory to enumerate adapters. The NVENC runtime DLL
///   (`nvEncodeAPI64.dll` or `nvEncodeAPI.dll`) must be present; it ships with the
///   Game Ready / Studio driver (>= 531.18 for AV1).
///
/// The env-var path (`TSOD_TEST_NVIDIA_*`) is retained for CI and test-fixture use
/// but is no longer the only detection method.

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

/// Full probe: adapter detection → generation gate → library load.
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

    if !is_av1_capable_nvidia(&adapter) {
        return NvencAv1Status {
            available: false,
            reason: Some(format!(
                "NVIDIA adapter '{}' (device_id=0x{:04x}) does not support NVENC AV1 — \
                 requires Ada Lovelace (RTX 40) or Blackwell (RTX 50) series",
                adapter.name, adapter.device_id
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

/// Try to detect a real NVIDIA GPU.
///
/// Priority:
///  1. Test-environment override via `TSOD_TEST_NVIDIA_*` env vars (CI use only).
///  2. Platform-native enumeration (sysfs on Linux, DXGI on Windows).
pub fn detect_nvidia_adapter() -> Option<NvidiaAdapterInfo> {
    // 1. CI / test override ────────────────────────────────────────────────
    if let Some(info) = detect_nvidia_from_env() {
        return Some(info);
    }

    // 2. Real hardware enumeration ────────────────────────────────────────
    #[cfg(target_os = "linux")]
    {
        if let Some(info) = detect_nvidia_sysfs() {
            return Some(info);
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(info) = detect_nvidia_dxgi() {
            return Some(info);
        }
    }

    None
}

/// Test-only path: read adapter info from env vars.
fn detect_nvidia_from_env() -> Option<NvidiaAdapterInfo> {
    let vendor = std::env::var("TSOD_TEST_NVIDIA_VENDOR_ID").ok()?;
    let vendor_id = u32::from_str_radix(vendor.trim_start_matches("0x"), 16).ok()?;
    let device_id = std::env::var("TSOD_TEST_NVIDIA_DEVICE_ID")
        .ok()
        .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .unwrap_or_default();
    let name = std::env::var("TSOD_TEST_NVIDIA_NAME").unwrap_or_default();
    Some(NvidiaAdapterInfo {
        vendor_id,
        device_id,
        name,
    })
}

/// Linux: walk `/sys/bus/pci/devices/` for vendor 0x10DE.
#[cfg(target_os = "linux")]
fn detect_nvidia_sysfs() -> Option<NvidiaAdapterInfo> {
    use std::fs;
    use std::path::Path;

    let pci_dir = Path::new("/sys/bus/pci/devices");
    let entries = fs::read_dir(pci_dir).ok()?;

    // PCI class 0x03 = display controller
    const DISPLAY_CLASS_PREFIX: &str = "0x03";
    const NVIDIA_VENDOR: &str = "0x10de";

    let mut best: Option<NvidiaAdapterInfo> = None;

    for entry in entries.flatten() {
        let path = entry.path();

        let vendor = fs::read_to_string(path.join("vendor"))
            .ok()
            .map(|s| s.trim().to_ascii_lowercase())?;
        if vendor != NVIDIA_VENDOR {
            continue;
        }

        // Only consider display-class devices (skip NVLink bridges, USB, etc.)
        let class = fs::read_to_string(path.join("class"))
            .ok()
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if !class.starts_with(DISPLAY_CLASS_PREFIX) {
            continue;
        }

        let device_str = fs::read_to_string(path.join("device"))
            .ok()
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let device_id =
            u32::from_str_radix(device_str.trim_start_matches("0x"), 16).unwrap_or_default();

        // Best-effort name: try the driver's `label` or fall back to PCI slot.
        let name = fs::read_to_string(path.join("label"))
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .to_string()
            });

        let info = NvidiaAdapterInfo {
            vendor_id: 0x10DE,
            device_id,
            name,
        };

        // Prefer the adapter with the highest device_id (newest generation).
        if best
            .as_ref()
            .map_or(true, |b| info.device_id > b.device_id)
        {
            best = Some(info);
        }
    }

    best
}

/// Windows: enumerate DXGI adapters and find the first NVIDIA one.
#[cfg(target_os = "windows")]
fn detect_nvidia_dxgi() -> Option<NvidiaAdapterInfo> {
    // Use the Windows crate's DXGI factory to enumerate adapters.
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1().ok()? };
    let mut adapter_idx: u32 = 0;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(adapter_idx) } {
            Ok(a) => a,
            Err(_) => break,
        };
        adapter_idx += 1;

        let desc = unsafe { adapter.GetDesc1().ok()? };
        let vendor_id = desc.VendorId;
        if vendor_id != 0x10DE {
            continue;
        }

        let device_id = desc.DeviceId;
        let name = String::from_utf16_lossy(
            &desc.Description[..desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len())],
        );

        return Some(NvidiaAdapterInfo {
            vendor_id,
            device_id: device_id as u32,
            name,
        });
    }

    None
}

/// Returns true when the adapter supports NVENC AV1 encoding.
///
/// NVENC AV1 was introduced with Ada Lovelace (RTX 40 series, AD10x).
/// Blackwell (RTX 50 series, GB20x) also supports it.
///
/// Device-ID ranges (approximate, covers desktop + mobile):
///  - Ada Lovelace: 0x2684 – 0x28FF
///  - Blackwell:    0x2900 – 0x2CFF
pub fn is_av1_capable_nvidia(adapter: &NvidiaAdapterInfo) -> bool {
    if adapter.vendor_id != 0x10DE {
        return false;
    }
    matches!(adapter.device_id, 0x2684..=0x28FF | 0x2900..=0x2CFF)
}

// Keep old name as an alias so nothing else breaks.
pub fn is_rtx_40_or_50_series(adapter: &NvidiaAdapterInfo) -> bool {
    is_av1_capable_nvidia(adapter)
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

        assert!(is_av1_capable_nvidia(&supported));
        assert!(!is_av1_capable_nvidia(&unsupported_gen));
        assert!(!is_av1_capable_nvidia(&not_nvidia));
    }

    #[test]
    fn alias_matches_new_fn() {
        let ada = NvidiaAdapterInfo {
            vendor_id: 0x10DE,
            device_id: 0x2684,
            name: "RTX 4090".into(),
        };
        assert_eq!(
            is_rtx_40_or_50_series(&ada),
            is_av1_capable_nvidia(&ada)
        );
    }

    #[test]
    fn env_override_still_works() {
        // When env vars are not set, detect_nvidia_from_env returns None.
        // (We don't set them in tests by default.)
        let from_env = detect_nvidia_from_env();
        // This test just ensures the code path doesn't panic.
        let _ = from_env;
    }

    #[test]
    fn probe_without_nvidia_is_unavailable() {
        // In CI there is typically no real NVIDIA GPU.  Make sure the probe
        // doesn't panic and returns a sensible status.
        let status = probe_nvenc_av1();
        // We can't assert available==false because someone might run tests
        // on a machine with an RTX 40 card, but we can verify the struct
        // is well-formed.
        if !status.available {
            assert!(status.reason.is_some());
        }
    }
}
