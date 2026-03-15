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
    SvtAv1,
    VaapiVp9,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DecodeBackendKind {
    MfHwVp9,
    Libvpx,
    MfHwAv1,
    Dav1d,
    VaapiVp9,
    VaapiAv1,
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
    let preferred_codec = sender_policy
        .preferred_codec_order()
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
/// software encoders (libvpx, SVT-AV1) cannot reliably sustain 1440p60 on
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
    av1.push(EncodeBackendKind::SvtAv1);
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
        (pb::VideoCodec::Av1, EncodeBackendKind::NvencAv1 | EncodeBackendKind::SvtAv1) => {
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
        (pb::VideoCodec::Av1, DecodeBackendKind::MfHwAv1 | DecodeBackendKind::VaapiAv1) => false,
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

        let mut av1 = Vec::new();
        av1.push(DecodeBackendKind::Dav1d);
        if !hw_disabled {
            av1.push(DecodeBackendKind::MfHwAv1);
        }
        map.insert(pb::VideoCodec::Av1, av1);
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

        let mut av1 = Vec::new();
        av1.push(DecodeBackendKind::Dav1d);
        if !hw_disabled {
            av1.push(DecodeBackendKind::VaapiAv1);
        }
        map.insert(pb::VideoCodec::Av1, av1);
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
        "av1-svt" => {
            backends.insert(pb::VideoCodec::Av1, vec![EncodeBackendKind::SvtAv1]);
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
        "av1-mf" => {
            backends.insert(pb::VideoCodec::Av1, vec![DecodeBackendKind::MfHwAv1]);
        }
        "av1-vaapi" => {
            backends.insert(pb::VideoCodec::Av1, vec![DecodeBackendKind::VaapiAv1]);
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
    fn explicit_svt_selection_software_only() {
        let mut map = HashMap::new();
        apply_encoder_override("av1-svt", &mut map);
        assert_eq!(
            map.get(&pb::VideoCodec::Av1),
            Some(&vec![EncodeBackendKind::SvtAv1])
        );
    }

    #[test]
    fn profile_1440_hidden_when_software_only() {
        let mut map = HashMap::new();
        map.insert(pb::VideoCodec::Vp9, vec![EncodeBackendKind::Libvpx]);
        assert!(!estimate_encode_headroom_1440p60(&map));
    }

    #[test]
    fn profile_1440_hidden_when_svt_av1_only() {
        let mut map = HashMap::new();
        map.insert(pb::VideoCodec::Av1, vec![EncodeBackendKind::SvtAv1]);
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
        let enc = HashMap::from([(
            pb::VideoCodec::Vp9,
            vec![EncodeBackendKind::Libvpx],
        )]);
        let dec = HashMap::from([(
            pb::VideoCodec::Vp9,
            vec![DecodeBackendKind::Libvpx],
        )]);
        let caps = build_codec_capabilities(&enc, &dec);
        assert_eq!(caps.len(), 1);
        assert!(caps[0].is_fully_runnable());
        assert_eq!(caps[0].encode_level, CapabilityLevel::Initialized);
        assert_eq!(caps[0].decode_level, CapabilityLevel::Initialized);
    }

    #[test]
    fn codec_capability_encode_only_is_not_runnable() {
        let enc = HashMap::from([(
            pb::VideoCodec::Av1,
            vec![EncodeBackendKind::SvtAv1],
        )]);
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
        let dec = HashMap::from([(
            pb::VideoCodec::Vp9,
            vec![DecodeBackendKind::Libvpx],
        )]);
        let caps = build_codec_capabilities(&enc, &dec);
        assert_eq!(caps.len(), 1);
        assert!(!caps[0].is_fully_runnable());
    }

    #[test]
    fn runnable_codecs_filters_correctly() {
        let enc = HashMap::from([
            (pb::VideoCodec::Vp9, vec![EncodeBackendKind::Libvpx]),
            (pb::VideoCodec::Av1, vec![EncodeBackendKind::SvtAv1]),
        ]);
        // Only VP9 has decode backends.
        let dec = HashMap::from([(
            pb::VideoCodec::Vp9,
            vec![DecodeBackendKind::Libvpx],
        )]);
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
}
