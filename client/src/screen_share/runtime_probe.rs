pub mod nvidia;
pub mod vaapi;

use crate::net::{
    video_decode::{av1 as av1_decode, vp9 as vp9_decode},
    video_encode::{av1 as av1_encode, vp9 as vp9_encode},
};
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::config::{
    env_disable_hw, env_screen_capture_override, env_system_audio_override,
    env_video_decoder_override, env_video_encoder_override, SenderPolicy,
};
use std::collections::HashMap;

/// Capability readiness level for a codec.
///
/// Each level is a strict superset of the previous one:
/// - **Compiled** — the backend was compiled into this binary.
/// - **Detected** — the OS/driver reports that the hardware or library is
///   present (e.g. VAAPI device node exists, NVENC driver loads).
/// - **Initialized** — a minimal encode/decode session was opened and torn
///   down successfully.  Only codecs at this level are advertised to the
///   server and shown in the UI.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum CapabilityLevel {
    /// Backend code is compiled in but not yet probed.
    Compiled,
    /// Driver/library detected on this machine.
    Detected,
    /// A test init succeeded — ready to encode/decode.
    Initialized,
}

/// Per-codec capability summary after probing.
#[derive(Clone, Debug)]
pub struct CodecCapability {
    pub codec: pb::VideoCodec,
    pub encode_level: CapabilityLevel,
    pub decode_level: CapabilityLevel,
    pub encode_backends: Vec<EncodeBackendKind>,
    pub decode_backends: Vec<DecodeBackendKind>,
}

impl CodecCapability {
    /// True only when both encode and decode reached `Initialized`.
    pub fn is_fully_runnable(&self) -> bool {
        self.encode_level == CapabilityLevel::Initialized
            && self.decode_level == CapabilityLevel::Initialized
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CaptureBackendKind {
    /// Windows.Graphics.Capture — preferred for per-HWND capture on Win 10 1903+.
    /// Falls back to GDI (via Dxgi backend) on older Windows or init failure.
    Wgc,
    Dxgi,
    PipewirePortal,
    X11,
    Scrap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum EncodeBackendKind {
    MfHwVp9,
    Libvpx,
    NvencAv1,
    Rav1eAv1,
    VaapiVp9,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DecodeBackendKind {
    MfHwVp9,
    Libvpx,
    Dav1d,
    VaapiVp9,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SystemAudioBackendKind {
    WasapiLoopback,
    PipewireMonitor,
    Off,
}

#[derive(Clone, Debug)]
pub struct MediaRuntimeCaps {
    pub capture_backends: Vec<CaptureBackendKind>,
    pub encode_backends: HashMap<pb::VideoCodec, Vec<EncodeBackendKind>>,
    pub decode_backends: HashMap<pb::VideoCodec, Vec<DecodeBackendKind>>,
    pub audio_backends: Vec<SystemAudioBackendKind>,
    pub supports_system_audio: bool,
    pub max_simulcast_layers: u8,
    pub preferred_codec: pb::VideoCodec,
    pub supports_1440p60: bool,
    /// Per-codec capability breakdown (only codecs that reached `Initialized`).
    pub codec_capabilities: Vec<CodecCapability>,
}

pub fn probe_media_caps(source: &crate::ShareSource) -> MediaRuntimeCaps {
    let mut capture_backends = preferred_capture_backends(source);
    let mut encode_backends = verified_encode_backends(preferred_encode_backends());
    let mut decode_backends = verified_decode_backends(preferred_decode_backends());
    let audio_backends = preferred_audio_backends();

    if let Some(override_capture) = env_screen_capture_override() {
        capture_backends = match override_capture.as_str() {
            "wgc" => vec![CaptureBackendKind::Wgc, CaptureBackendKind::Scrap],
            "dxgi" => vec![CaptureBackendKind::Dxgi, CaptureBackendKind::Scrap],
            "pipewire" => vec![
                CaptureBackendKind::PipewirePortal,
                CaptureBackendKind::Scrap,
            ],
            "x11" => vec![CaptureBackendKind::X11, CaptureBackendKind::Scrap],
            "scrap" => vec![CaptureBackendKind::Scrap],
            _ => capture_backends,
        };
    }

    if let Some(encoder_override) = env_video_encoder_override() {
        apply_encoder_override(&encoder_override, &mut encode_backends);
    }

    if let Some(decoder_override) = env_video_decoder_override() {
        apply_decoder_override(&decoder_override, &mut decode_backends);
    }

    encode_backends = verified_encode_backends(encode_backends);
    decode_backends = verified_decode_backends(decode_backends);

    let sender_policy = SenderPolicy::from_settings_or_env(None);
    let preferred_codec = preferred_codec_order(
        sender_policy,
        &encode_backends,
        prefers_av1_low_latency_nvidia(),
    )
    .into_iter()
    .find(|codec| encode_backends.contains_key(codec))
    .unwrap_or(pb::VideoCodec::Vp9);

    let supports_system_audio = audio_backends
        .iter()
        .any(|backend| !matches!(backend, SystemAudioBackendKind::Off));

    let supports_1440p60 = estimate_encode_headroom_1440p60(&encode_backends);
    let max_simulcast_layers = estimate_max_simulcast_layers(&encode_backends, supports_1440p60);

    let codec_capabilities = build_codec_capabilities(&encode_backends, &decode_backends);

    MediaRuntimeCaps {
        capture_backends,
        encode_backends,
        decode_backends,
        audio_backends,
        supports_system_audio,
        max_simulcast_layers,
        preferred_codec,
        supports_1440p60,
        codec_capabilities,
    }
}

fn preferred_codec_order(
    sender_policy: SenderPolicy,
    encode_backends: &HashMap<pb::VideoCodec, Vec<EncodeBackendKind>>,
    prefer_av1_low_latency_nvidia: bool,
) -> Vec<pb::VideoCodec> {
    let mut order = sender_policy.preferred_codec_order();

    if sender_policy == SenderPolicy::AutoLowLatency
        && encode_backends.contains_key(&pb::VideoCodec::Av1)
        && prefer_av1_low_latency_nvidia
    {
        order = vec![pb::VideoCodec::Av1, pb::VideoCodec::Vp9];
    }

    order
}

fn prefers_av1_low_latency_nvidia() -> bool {
    prefers_av1_low_latency_nvidia_adapter(nvidia::detect_nvidia_adapter().as_ref())
}

fn prefers_av1_low_latency_nvidia_adapter(adapter: Option<&nvidia::NvidiaAdapterInfo>) -> bool {
    adapter.is_some_and(nvidia::is_rtx_40_or_50_series)
}

/// Build per-codec capability summaries from already-verified backend maps.
///
/// Because `verified_encode_backends` / `verified_decode_backends` only keep
/// backends that passed `can_init_*_backend`, any codec present in both maps
/// is at `Initialized` level.  Codecs present in only one map get `Detected`.
fn build_codec_capabilities(
    encode_backends: &HashMap<pb::VideoCodec, Vec<EncodeBackendKind>>,
    decode_backends: &HashMap<pb::VideoCodec, Vec<DecodeBackendKind>>,
) -> Vec<CodecCapability> {
    let mut codecs: Vec<pb::VideoCodec> = Vec::new();
    for codec in encode_backends.keys().chain(decode_backends.keys()) {
        if !codecs.contains(codec) {
            codecs.push(*codec);
        }
    }

    codecs
        .into_iter()
        .map(|codec| {
            let enc = encode_backends.get(&codec);
            let dec = decode_backends.get(&codec);

            let encode_level = if enc.is_some() {
                CapabilityLevel::Initialized
            } else {
                CapabilityLevel::Compiled
            };
            let decode_level = if dec.is_some() {
                CapabilityLevel::Initialized
            } else {
                CapabilityLevel::Compiled
            };

            CodecCapability {
                codec,
                encode_level,
                decode_level,
                encode_backends: enc.cloned().unwrap_or_default(),
                decode_backends: dec.cloned().unwrap_or_default(),
            }
        })
        .collect()
}

/// Returns only codecs where both encode and decode are `Initialized`.
pub fn runnable_codecs(caps: &MediaRuntimeCaps) -> Vec<pb::VideoCodec> {
    caps.codec_capabilities
        .iter()
        .filter(|c| c.is_fully_runnable())
        .map(|c| c.codec)
        .collect()
}

fn estimate_max_simulcast_layers(
    encode_backends: &HashMap<pb::VideoCodec, Vec<EncodeBackendKind>>,
    supports_1440p60: bool,
) -> u8 {
    let cpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let has_hw_encode = encode_backends.values().flatten().any(|backend| {
        matches!(
            backend,
            EncodeBackendKind::MfHwVp9 | EncodeBackendKind::NvencAv1 | EncodeBackendKind::VaapiVp9
        )
    });

    if supports_1440p60 && (has_hw_encode || cpu >= 12) {
        3
    } else if has_hw_encode || cpu >= 8 {
        2
    } else {
        1
    }
}

/// Conservative 1440p60 gating.
///
/// 1440p60 is **only** advertised when a verified hardware encoder backend is
/// present in the *already-verified* `encode_backends` map.  By the time this
/// function is called, every backend in the map has passed its
/// `can_initialize_backend` probe — so this is no longer optimistic inference.
///
/// The CPU-core heuristic (≥ 12 cores → true) is intentionally removed:
/// software encoders (libvpx, rav1e) cannot reliably sustain 1440p60 on
/// commodity hardware without frame drops, so advertising the capability
/// without a hardware backend leads to a degraded experience.
fn estimate_encode_headroom_1440p60(
    encode_backends: &HashMap<pb::VideoCodec, Vec<EncodeBackendKind>>,
) -> bool {
    let has_hw_av1 = encode_backends
        .get(&pb::VideoCodec::Av1)
        .map(|backends| {
            backends
                .iter()
                .any(|backend| matches!(backend, EncodeBackendKind::NvencAv1))
        })
        .unwrap_or(false);

    let has_hw_vp9 = encode_backends
        .get(&pb::VideoCodec::Vp9)
        .map(|backends| {
            backends.iter().any(|backend| {
                matches!(
                    backend,
                    EncodeBackendKind::MfHwVp9 | EncodeBackendKind::VaapiVp9
                )
            })
        })
        .unwrap_or(false);

    // Only advertise 1440p60 when a real, verified hardware encoder is present.
    has_hw_av1 || has_hw_vp9
}

fn preferred_capture_backends(_source: &crate::ShareSource) -> Vec<CaptureBackendKind> {
    #[cfg(target_os = "windows")]
    {
        // For window capture, prefer WGC (GPU-composited + DRM content) then
        // fall back to DXGI/GDI.  For display capture, DXGI Desktop Duplication
        // is still the best backend (real dirty-rect metadata, display-paced).
        if matches!(_source, crate::ShareSource::WindowsWindow(_)) {
            return vec![
                CaptureBackendKind::Wgc,
                CaptureBackendKind::Dxgi,
                CaptureBackendKind::Scrap,
            ];
        }
        return vec![CaptureBackendKind::Dxgi, CaptureBackendKind::Scrap];
    }

    #[cfg(target_os = "linux")]
    {
        if matches!(_source, crate::ShareSource::X11Window(_)) {
            return vec![CaptureBackendKind::X11, CaptureBackendKind::Scrap];
        }
        return vec![
            CaptureBackendKind::PipewirePortal,
            CaptureBackendKind::Scrap,
        ];
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        vec![CaptureBackendKind::Scrap]
    }
}

fn preferred_encode_backends() -> HashMap<pb::VideoCodec, Vec<EncodeBackendKind>> {
    let mut map = HashMap::new();
    let hw_disabled = env_disable_hw();

    #[cfg(target_os = "windows")]
    {
        map.insert(
            pb::VideoCodec::Vp9,
            if hw_disabled {
                vec![EncodeBackendKind::Libvpx]
            } else {
                vec![EncodeBackendKind::MfHwVp9, EncodeBackendKind::Libvpx]
            },
        );

        let av1 = av1_encode_backends(hw_disabled);
        if !av1.is_empty() {
            map.insert(pb::VideoCodec::Av1, av1);
        }

        return map;
    }

    #[cfg(target_os = "linux")]
    {
        map.insert(
            pb::VideoCodec::Vp9,
            if hw_disabled {
                vec![EncodeBackendKind::Libvpx]
            } else {
                vec![EncodeBackendKind::VaapiVp9, EncodeBackendKind::Libvpx]
            },
        );

        let av1 = av1_encode_backends(hw_disabled);
        if !av1.is_empty() {
            map.insert(pb::VideoCodec::Av1, av1);
        }

        return map;
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        map.insert(pb::VideoCodec::Vp9, vec![EncodeBackendKind::Libvpx]);
        map
    }
}

fn av1_encode_backends(hw_disabled: bool) -> Vec<EncodeBackendKind> {
    let mut av1 = Vec::new();
    if !hw_disabled {
        av1.push(EncodeBackendKind::NvencAv1);
    }
    av1.push(EncodeBackendKind::Rav1eAv1);
    av1
}

fn verified_encode_backends(
    backends: HashMap<pb::VideoCodec, Vec<EncodeBackendKind>>,
) -> HashMap<pb::VideoCodec, Vec<EncodeBackendKind>> {
    backends
        .into_iter()
        .filter_map(|(codec, list)| {
            let verified = list
                .into_iter()
                .filter(|backend| can_init_encode_backend(codec, *backend))
                .collect::<Vec<_>>();
            (!verified.is_empty()).then_some((codec, verified))
        })
        .collect()
}

fn can_init_encode_backend(codec: pb::VideoCodec, backend: EncodeBackendKind) -> bool {
    match (codec, backend) {
        (
            pb::VideoCodec::Vp9,
            EncodeBackendKind::MfHwVp9 | EncodeBackendKind::VaapiVp9 | EncodeBackendKind::Libvpx,
        ) => vp9_encode::can_initialize_backend(backend),
        (pb::VideoCodec::Av1, EncodeBackendKind::NvencAv1 | EncodeBackendKind::Rav1eAv1) => {
            av1_encode::can_initialize_backend(backend)
        }
        _ => false,
    }
}

fn verified_decode_backends(
    backends: HashMap<pb::VideoCodec, Vec<DecodeBackendKind>>,
) -> HashMap<pb::VideoCodec, Vec<DecodeBackendKind>> {
    backends
        .into_iter()
        .filter_map(|(codec, list)| {
            let verified = list
                .into_iter()
                .filter(|backend| can_init_decode_backend(codec, *backend))
                .collect::<Vec<_>>();
            (!verified.is_empty()).then_some((codec, verified))
        })
        .collect()
}

fn can_init_decode_backend(codec: pb::VideoCodec, backend: DecodeBackendKind) -> bool {
    match (codec, backend) {
        (
            pb::VideoCodec::Vp9,
            DecodeBackendKind::MfHwVp9 | DecodeBackendKind::VaapiVp9 | DecodeBackendKind::Libvpx,
        ) => vp9_decode::can_initialize_backend(backend),
        (pb::VideoCodec::Av1, DecodeBackendKind::Dav1d) => {
            av1_decode::can_initialize_backend(backend)
        }
        _ => false,
    }
}

fn preferred_decode_backends() -> HashMap<pb::VideoCodec, Vec<DecodeBackendKind>> {
    let mut map = HashMap::new();
    let hw_disabled = env_disable_hw();

    #[cfg(target_os = "windows")]
    {
        map.insert(
            pb::VideoCodec::Vp9,
            if hw_disabled {
                vec![DecodeBackendKind::Libvpx]
            } else {
                vec![DecodeBackendKind::MfHwVp9, DecodeBackendKind::Libvpx]
            },
        );

        map.insert(pb::VideoCodec::Av1, vec![DecodeBackendKind::Dav1d]);
        return map;
    }

    #[cfg(target_os = "linux")]
    {
        map.insert(
            pb::VideoCodec::Vp9,
            if hw_disabled {
                vec![DecodeBackendKind::Libvpx]
            } else {
                vec![DecodeBackendKind::VaapiVp9, DecodeBackendKind::Libvpx]
            },
        );

        map.insert(pb::VideoCodec::Av1, vec![DecodeBackendKind::Dav1d]);
        return map;
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        map.insert(pb::VideoCodec::Vp9, vec![DecodeBackendKind::Libvpx]);
        map
    }
}

fn preferred_audio_backends() -> Vec<SystemAudioBackendKind> {
    if let Some(override_value) = env_system_audio_override() {
        return match override_value.as_str() {
            "off" => vec![SystemAudioBackendKind::Off],
            "wasapi" => vec![
                SystemAudioBackendKind::WasapiLoopback,
                SystemAudioBackendKind::Off,
            ],
            "pipewire" => vec![
                SystemAudioBackendKind::PipewireMonitor,
                SystemAudioBackendKind::Off,
            ],
            _ => default_audio_backends(),
        };
    }

    default_audio_backends()
}

fn default_audio_backends() -> Vec<SystemAudioBackendKind> {
    #[cfg(target_os = "windows")]
    {
        return vec![
            SystemAudioBackendKind::WasapiLoopback,
            SystemAudioBackendKind::Off,
        ];
    }

    #[cfg(target_os = "linux")]
    {
        return vec![
            SystemAudioBackendKind::PipewireMonitor,
            SystemAudioBackendKind::Off,
        ];
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        vec![SystemAudioBackendKind::Off]
    }
}

fn apply_encoder_override(
    override_value: &str,
    backends: &mut HashMap<pb::VideoCodec, Vec<EncodeBackendKind>>,
) {
    match override_value {
        "vp9-libvpx" => {
            backends.insert(pb::VideoCodec::Vp9, vec![EncodeBackendKind::Libvpx]);
        }
        "vp9-svt" => {
            backends.insert(pb::VideoCodec::Vp9, vec![EncodeBackendKind::MfHwVp9]);
        }
        "av1-rav1e" => {
            backends.insert(pb::VideoCodec::Av1, vec![EncodeBackendKind::Rav1eAv1]);
        }
        "av1-nvenc" => {
            backends.insert(pb::VideoCodec::Av1, vec![EncodeBackendKind::NvencAv1]);
        }
        _ => {}
    }
}

fn apply_decoder_override(
    override_value: &str,
    backends: &mut HashMap<pb::VideoCodec, Vec<DecodeBackendKind>>,
) {
    match override_value {
        "vp9-libvpx" => {
            backends.insert(pb::VideoCodec::Vp9, vec![DecodeBackendKind::Libvpx]);
        }
        "vp9-ffvp9" => {
            backends.insert(pb::VideoCodec::Vp9, vec![DecodeBackendKind::MfHwVp9]);
        }
        "av1-dav1d" => {
            backends.insert(pb::VideoCodec::Av1, vec![DecodeBackendKind::Dav1d]);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impossible_codec_not_advertised() {
        let caps = verified_encode_backends(HashMap::from([(
            pb::VideoCodec::Av1,
            vec![EncodeBackendKind::NvencAv1],
        )]));
        assert!(caps.get(&pb::VideoCodec::Av1).is_none());
    }

    #[test]
    fn explicit_nvenc_selection_no_fallback() {
        let mut map = HashMap::new();
        apply_encoder_override("av1-nvenc", &mut map);
        assert_eq!(
            map.get(&pb::VideoCodec::Av1),
            Some(&vec![EncodeBackendKind::NvencAv1])
        );
    }

    #[test]
    fn explicit_rav1e_selection_software_only() {
        let mut map = HashMap::new();
        apply_encoder_override("av1-rav1e", &mut map);
        assert_eq!(
            map.get(&pb::VideoCodec::Av1),
            Some(&vec![EncodeBackendKind::Rav1eAv1])
        );
    }

    #[test]
    fn profile_1440_hidden_when_software_only() {
        let mut map = HashMap::new();
        map.insert(pb::VideoCodec::Vp9, vec![EncodeBackendKind::Libvpx]);
        assert!(!estimate_encode_headroom_1440p60(&map));
    }

    #[test]
    fn profile_1440_hidden_when_rav1e_av1_only() {
        let mut map = HashMap::new();
        map.insert(pb::VideoCodec::Av1, vec![EncodeBackendKind::Rav1eAv1]);
        assert!(
            !estimate_encode_headroom_1440p60(&map),
            "software AV1 must not enable 1440p60"
        );
    }

    #[test]
    fn profile_1440_advertised_with_nvenc_av1() {
        let mut map = HashMap::new();
        map.insert(pb::VideoCodec::Av1, vec![EncodeBackendKind::NvencAv1]);
        assert!(
            estimate_encode_headroom_1440p60(&map),
            "NVENC AV1 should enable 1440p60"
        );
    }

    #[test]
    fn profile_1440_advertised_with_vaapi_vp9() {
        let mut map = HashMap::new();
        map.insert(pb::VideoCodec::Vp9, vec![EncodeBackendKind::VaapiVp9]);
        assert!(
            estimate_encode_headroom_1440p60(&map),
            "VAAPI VP9 should enable 1440p60"
        );
    }

    #[test]
    fn profile_1440_advertised_with_mf_hw_vp9() {
        let mut map = HashMap::new();
        map.insert(pb::VideoCodec::Vp9, vec![EncodeBackendKind::MfHwVp9]);
        assert!(
            estimate_encode_headroom_1440p60(&map),
            "MF HW VP9 should enable 1440p60"
        );
    }

    #[test]
    fn profile_1440_not_advertised_when_empty() {
        let map = HashMap::new();
        assert!(!estimate_encode_headroom_1440p60(&map));
    }

    // ── Capability-layer tests ──────────────────────────────────────────

    #[test]
    fn codec_capability_fully_runnable_requires_both_encode_and_decode() {
        let enc = HashMap::from([(pb::VideoCodec::Vp9, vec![EncodeBackendKind::Libvpx])]);
        let dec = HashMap::from([(pb::VideoCodec::Vp9, vec![DecodeBackendKind::Libvpx])]);
        let caps = build_codec_capabilities(&enc, &dec);
        assert_eq!(caps.len(), 1);
        assert!(caps[0].is_fully_runnable());
        assert_eq!(caps[0].encode_level, CapabilityLevel::Initialized);
        assert_eq!(caps[0].decode_level, CapabilityLevel::Initialized);
    }

    #[test]
    fn codec_capability_encode_only_is_not_runnable() {
        let enc = HashMap::from([(pb::VideoCodec::Av1, vec![EncodeBackendKind::Rav1eAv1])]);
        let dec = HashMap::new();
        let caps = build_codec_capabilities(&enc, &dec);
        assert_eq!(caps.len(), 1);
        assert!(!caps[0].is_fully_runnable());
        assert_eq!(caps[0].encode_level, CapabilityLevel::Initialized);
        assert_eq!(caps[0].decode_level, CapabilityLevel::Compiled);
    }

    #[test]
    fn codec_capability_decode_only_is_not_runnable() {
        let enc = HashMap::new();
        let dec = HashMap::from([(pb::VideoCodec::Vp9, vec![DecodeBackendKind::Libvpx])]);
        let caps = build_codec_capabilities(&enc, &dec);
        assert_eq!(caps.len(), 1);
        assert!(!caps[0].is_fully_runnable());
    }

    #[test]
    fn runnable_codecs_filters_correctly() {
        let enc = HashMap::from([
            (pb::VideoCodec::Vp9, vec![EncodeBackendKind::Libvpx]),
            (pb::VideoCodec::Av1, vec![EncodeBackendKind::Rav1eAv1]),
        ]);
        // Only VP9 has decode backends.
        let dec = HashMap::from([(pb::VideoCodec::Vp9, vec![DecodeBackendKind::Libvpx])]);
        let caps = build_codec_capabilities(&enc, &dec);
        let media = MediaRuntimeCaps {
            capture_backends: vec![],
            encode_backends: enc,
            decode_backends: dec,
            audio_backends: vec![],
            supports_system_audio: false,
            max_simulcast_layers: 1,
            preferred_codec: pb::VideoCodec::Vp9,
            supports_1440p60: false,
            codec_capabilities: caps,
        };
        let runnable = runnable_codecs(&media);
        assert_eq!(runnable, vec![pb::VideoCodec::Vp9]);
    }

    #[test]
    fn empty_backends_produce_no_capabilities() {
        let caps = build_codec_capabilities(&HashMap::new(), &HashMap::new());
        assert!(caps.is_empty());
    }

    #[test]
    fn low_latency_policy_prefers_av1_on_rtx_4090_or_5090() {
        let rtx_4090 = nvidia::NvidiaAdapterInfo {
            vendor_id: 0x10DE,
            device_id: 0x2704,
            name: "NVIDIA GeForce RTX 4090".into(),
        };
        let rtx_5090 = nvidia::NvidiaAdapterInfo {
            vendor_id: 0x10DE,
            device_id: 0x2B80,
            name: "NVIDIA GeForce RTX 5090".into(),
        };

        assert!(prefers_av1_low_latency_nvidia_adapter(Some(&rtx_4090)));
        assert!(prefers_av1_low_latency_nvidia_adapter(Some(&rtx_5090)));
    }

    #[test]
    fn low_latency_policy_does_not_force_av1_for_non_ada_blackwell() {
        let rtx_3090 = nvidia::NvidiaAdapterInfo {
            vendor_id: 0x10DE,
            device_id: 0x2204,
            name: "NVIDIA GeForce RTX 3090".into(),
        };

        assert!(!prefers_av1_low_latency_nvidia_adapter(Some(&rtx_3090)));
        assert!(!prefers_av1_low_latency_nvidia_adapter(None));
    }

    #[test]
    fn low_latency_policy_reorders_codec_preference_on_av1_capable_nvidia() {
        let mut enc = HashMap::new();
        enc.insert(pb::VideoCodec::Vp9, vec![EncodeBackendKind::Libvpx]);
        enc.insert(pb::VideoCodec::Av1, vec![EncodeBackendKind::NvencAv1]);

        let order = preferred_codec_order(SenderPolicy::AutoLowLatency, &enc, true);
        assert_eq!(order, vec![pb::VideoCodec::Av1, pb::VideoCodec::Vp9]);
    }

    #[test]
    fn every_verified_decode_backend_is_initializable() {
        let verified = verified_decode_backends(preferred_decode_backends());
        for (codec, backends) in verified {
            for backend in backends {
                assert!(
                    can_init_decode_backend(codec, backend),
                    "verified backend {:?} for {:?} must be constructible",
                    backend,
                    codec
                );
            }
        }
    }

    #[test]
    fn every_verified_encode_backend_is_initializable() {
        let verified = verified_encode_backends(preferred_encode_backends());
        for (codec, backends) in verified {
            for backend in backends {
                assert!(
                    can_init_encode_backend(codec, backend),
                    "verified backend {:?} for {:?} must be constructible",
                    backend,
                    codec
                );
            }
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_av1_decode_selection_prefers_dav1d_only() {
        let backends = preferred_decode_backends()
            .remove(&pb::VideoCodec::Av1)
            .unwrap_or_default();
        assert_eq!(backends, vec![DecodeBackendKind::Dav1d]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_av1_decode_selection_prefers_dav1d_only() {
        let backends = preferred_decode_backends()
            .remove(&pb::VideoCodec::Av1)
            .unwrap_or_default();
        assert_eq!(backends, vec![DecodeBackendKind::Dav1d]);
    }
}
