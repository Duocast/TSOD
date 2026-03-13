use crate::proto::voiceplatform::v1 as pb;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SenderPolicy {
    AutoLowLatency,
    AutoPremiumAv1,
}

impl SenderPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AutoLowLatency => "auto_low_latency",
            Self::AutoPremiumAv1 => "auto_premium_av1",
        }
    }

    pub fn from_setting_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto_premium_av1" | "av1" => Self::AutoPremiumAv1,
            "auto_low_latency" | "auto" | "vp9" => Self::AutoLowLatency,
            _ => Self::AutoLowLatency,
        }
    }

    pub fn from_settings_or_env(settings_policy: Option<Self>) -> Self {
        if let Ok(value) = std::env::var("TSOD_VIDEO_CODEC_POLICY") {
            return Self::from_setting_value(&value);
        }

        settings_policy.unwrap_or(Self::AutoLowLatency)
    }

    pub fn preferred_codec_order(self) -> Vec<pb::VideoCodec> {
        match self {
            Self::AutoLowLatency => vec![pb::VideoCodec::Vp9, pb::VideoCodec::Av1],
            Self::AutoPremiumAv1 => vec![pb::VideoCodec::Av1, pb::VideoCodec::Vp9],
        }
    }
}

pub fn env_screen_capture_override() -> Option<String> {
    let value = std::env::var("TSOD_SCREEN_CAPTURE").ok()?;
    let normalized = value.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "auto" | "dxgi" | "pipewire" | "x11" | "scrap"
    )
    .then_some(normalized)
}

pub fn env_video_encoder_override() -> Option<String> {
    let value = std::env::var("TSOD_VIDEO_ENCODER").ok()?;
    let normalized = value.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "auto"
            | "vp9-libvpx"
            | "vp9-mf"
            | "vp9-vaapi"
            | "av1-rav1e"
            | "av1-svt"
            | "av1-mf"
            | "av1-vaapi"
    )
    .then_some(normalized)
}

pub fn env_video_decoder_override() -> Option<String> {
    let value = std::env::var("TSOD_VIDEO_DECODER").ok()?;
    let normalized = value.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "auto" | "vp9-libvpx" | "vp9-mf" | "vp9-vaapi" | "av1-dav1d" | "av1-mf" | "av1-vaapi"
    )
    .then_some(normalized)
}

pub fn env_system_audio_override() -> Option<String> {
    let value = std::env::var("TSOD_SYSTEM_AUDIO").ok()?;
    let normalized = value.trim().to_ascii_lowercase();
    matches!(normalized.as_str(), "auto" | "off" | "wasapi" | "pipewire").then_some(normalized)
}

pub fn env_disable_hw() -> bool {
    std::env::var("TSOD_DISABLE_HW")
        .ok()
        .as_deref()
        .map(str::trim)
        == Some("1")
}
