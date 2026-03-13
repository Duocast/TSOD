pub mod nvidia;

use crate::net::video_encode::av1::caps::probe_av1_caps;
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::config::{
    env_disable_hw, env_screen_capture_override, env_system_audio_override,
    env_video_decoder_override, env_video_encoder_override, SenderPolicy,
};
use std::collections::HashMap;

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
}

pub fn probe_media_caps(source: &crate::ShareSource) -> MediaRuntimeCaps {
    let mut capture_backends = preferred_capture_backends(source);
    let mut encode_backends = preferred_encode_backends();
    let mut decode_backends = preferred_decode_backends();
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

    MediaRuntimeCaps {
        capture_backends,
        encode_backends,
        decode_backends,
        audio_backends,
        supports_system_audio,
        max_simulcast_layers: 1,
        preferred_codec,
        supports_1440p60,
    }
}

fn estimate_encode_headroom_1440p60(
    encode_backends: &HashMap<pb::VideoCodec, Vec<EncodeBackendKind>>,
) -> bool {
    let cpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let has_hw_av1 = encode_backends
        .get(&pb::VideoCodec::Av1)
        .map(|backends| {
            backends.iter().any(|backend| {
                matches!(
                    backend,
                    EncodeBackendKind::NvencAv1
                        | EncodeBackendKind::MfHwVp9
                        | EncodeBackendKind::VaapiVp9
                )
            })
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

    has_hw_av1 || has_hw_vp9 || cpu >= 12
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
    let av1_caps = probe_av1_caps();
    let mut av1 = Vec::new();
    if !hw_disabled && av1_caps.nvenc_available {
        av1.push(EncodeBackendKind::NvencAv1);
    }
    if av1_caps.svt_available {
        av1.push(EncodeBackendKind::SvtAv1);
    }
    av1
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
    fn nvenc_not_shown_without_probe() {
        std::env::remove_var("TSOD_TEST_NVIDIA_VENDOR_ID");
        std::env::remove_var("TSOD_TEST_NVIDIA_DEVICE_ID");
        let av1 = av1_encode_backends(false);
        assert!(!av1.contains(&EncodeBackendKind::NvencAv1));
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
    fn auto_prefers_nvenc_over_svt() {
        std::env::set_var("TSOD_TEST_NVIDIA_VENDOR_ID", "10de");
        std::env::set_var("TSOD_TEST_NVIDIA_DEVICE_ID", "2684");
        let av1 = av1_encode_backends(false);
        if av1.contains(&EncodeBackendKind::SvtAv1) {
            assert_eq!(av1.first(), Some(&EncodeBackendKind::NvencAv1));
        }
    }
}
