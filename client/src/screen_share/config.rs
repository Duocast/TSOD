use crate::proto::voiceplatform::v1 as pb;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoEncoderOverride {
    Auto,
    Vp9Libvpx,
    Av1Nvenc,
    Av1Svt,
}

impl VideoEncoderOverride {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Vp9Libvpx => "vp9-libvpx",
            Self::Av1Nvenc => "av1-nvenc",
            Self::Av1Svt => "av1-svt",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoDecoderOverride {
    Auto,
    Vp9Libvpx,
    Av1Dav1d,
}

impl VideoDecoderOverride {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Vp9Libvpx => "vp9-libvpx",
            Self::Av1Dav1d => "av1-dav1d",
        }
    }
}

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
        "auto" | "wgc" | "dxgi" | "pipewire" | "x11" | "scrap"
    )
    .then_some(normalized)
}

pub fn env_video_encoder_override() -> Result<Option<VideoEncoderOverride>, String> {
    let value = match std::env::var("TSOD_VIDEO_ENCODER") {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let normalized = value.trim().to_ascii_lowercase();
    let parsed = match normalized.as_str() {
        "auto" => VideoEncoderOverride::Auto,
        "vp9-libvpx" => VideoEncoderOverride::Vp9Libvpx,
        "av1-nvenc" => VideoEncoderOverride::Av1Nvenc,
        "av1-svt" => VideoEncoderOverride::Av1Svt,
        "vp9-svt" => {
            return Err(
                "TSOD_VIDEO_ENCODER=vp9-svt is unsupported: VP9 SVT backend is not implemented"
                    .to_string(),
            )
        }
        _ => {
            return Err(format!(
                "TSOD_VIDEO_ENCODER={normalized} is invalid; supported values: auto, vp9-libvpx, av1-nvenc, av1-svt"
            ))
        }
    };

    Ok(Some(parsed))
}

pub fn env_video_decoder_override() -> Result<Option<VideoDecoderOverride>, String> {
    let value = match std::env::var("TSOD_VIDEO_DECODER") {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let normalized = value.trim().to_ascii_lowercase();
    let parsed = match normalized.as_str() {
        "auto" => VideoDecoderOverride::Auto,
        "vp9-libvpx" => VideoDecoderOverride::Vp9Libvpx,
        "av1-dav1d" => VideoDecoderOverride::Av1Dav1d,
        "vp9-ffvp9" => {
            return Err(
                "TSOD_VIDEO_DECODER=vp9-ffvp9 is unsupported: FFVP9 backend is not implemented"
                    .to_string(),
            )
        }
        _ => {
            return Err(format!(
                "TSOD_VIDEO_DECODER={normalized} is invalid; supported values: auto, vp9-libvpx, av1-dav1d"
            ))
        }
    };

    Ok(Some(parsed))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_override_rejects_unsupported_alias() {
        unsafe { std::env::set_var("TSOD_VIDEO_ENCODER", "vp9-svt") };
        let result = env_video_encoder_override();
        assert!(result.is_err());
        unsafe { std::env::remove_var("TSOD_VIDEO_ENCODER") };
    }

    #[test]
    fn decoder_override_rejects_unsupported_alias() {
        unsafe { std::env::set_var("TSOD_VIDEO_DECODER", "vp9-ffvp9") };
        let result = env_video_decoder_override();
        assert!(result.is_err());
        unsafe { std::env::remove_var("TSOD_VIDEO_DECODER") };
    }
}
