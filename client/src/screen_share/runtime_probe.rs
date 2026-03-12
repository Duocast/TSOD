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
    HardwareVp9,
    HardwareAv1,
    Libvpx,
    SvtAv1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DecodeBackendKind {
    HardwareVp9,
    HardwareAv1,
    Libvpx,
    Dav1d,
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

    let supports_1440p60 = benchmark_supports_1440p60(&encode_backends, preferred_codec);

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

fn benchmark_supports_1440p60(
    encode_backends: &HashMap<pb::VideoCodec, Vec<EncodeBackendKind>>,
    preferred_codec: pb::VideoCodec,
) -> bool {
    // Startup benchmark hook. Replace env value with measured throughput harness once
    // native codec backends are fully integrated.
    if std::env::var("TSOD_FORCE_1440P60").ok().as_deref() == Some("1") {
        return true;
    }
    encode_backends
        .get(&preferred_codec)
        .map(|backends| {
            backends.iter().any(|b| {
                matches!(
                    b,
                    EncodeBackendKind::HardwareAv1 | EncodeBackendKind::HardwareVp9
                )
            })
        })
        .unwrap_or(false)
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

    map.insert(
        pb::VideoCodec::Vp9,
        if hw_disabled {
            vec![EncodeBackendKind::Libvpx]
        } else {
            vec![EncodeBackendKind::HardwareVp9, EncodeBackendKind::Libvpx]
        },
    );

    let mut av1 = Vec::new();
    if !hw_disabled {
        av1.push(EncodeBackendKind::HardwareAv1);
    }
    if cfg!(feature = "video-av1-software") {
        av1.push(EncodeBackendKind::SvtAv1);
    }
    if !av1.is_empty() {
        map.insert(pb::VideoCodec::Av1, av1);
    }
    map
}

fn preferred_decode_backends() -> HashMap<pb::VideoCodec, Vec<DecodeBackendKind>> {
    let mut map = HashMap::new();
    let hw_disabled = env_disable_hw();

    map.insert(
        pb::VideoCodec::Vp9,
        if hw_disabled {
            vec![DecodeBackendKind::Libvpx]
        } else {
            vec![DecodeBackendKind::HardwareVp9, DecodeBackendKind::Libvpx]
        },
    );

    let mut av1 = Vec::new();
    if !hw_disabled {
        av1.push(DecodeBackendKind::HardwareAv1);
    }
    av1.push(DecodeBackendKind::Dav1d);
    map.insert(pb::VideoCodec::Av1, av1);
    map
}

fn preferred_audio_backends() -> Vec<SystemAudioBackendKind> {
    /* unchanged */
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
        "vp9-mf" | "vp9-vaapi" => {
            backends.insert(pb::VideoCodec::Vp9, vec![EncodeBackendKind::HardwareVp9]);
        }
        "av1-svt" => {
            backends.insert(pb::VideoCodec::Av1, vec![EncodeBackendKind::SvtAv1]);
        }
        "av1-mf" | "av1-vaapi" => {
            backends.insert(pb::VideoCodec::Av1, vec![EncodeBackendKind::HardwareAv1]);
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
        "vp9-mf" | "vp9-vaapi" => {
            backends.insert(pb::VideoCodec::Vp9, vec![DecodeBackendKind::HardwareVp9]);
        }
        "av1-dav1d" => {
            backends.insert(pb::VideoCodec::Av1, vec![DecodeBackendKind::Dav1d]);
        }
        "av1-mf" | "av1-vaapi" => {
            backends.insert(pb::VideoCodec::Av1, vec![DecodeBackendKind::HardwareAv1]);
        }
        _ => {}
    }
}
