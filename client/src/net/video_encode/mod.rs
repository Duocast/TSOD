use anyhow::{anyhow, bail, Result};
use tracing::info;

use crate::media_codec::VideoEncoder;
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::config::{env_video_encoder_override, SenderPolicy};
use crate::screen_share::runtime_probe::{EncodeBackendKind, MediaRuntimeCaps};

pub mod av1;
#[cfg(target_os = "linux")]
pub mod hw_linux;
#[cfg(target_os = "windows")]
pub mod hw_windows;
pub mod vp9;

pub fn build_screen_encoder(
    codec: pb::VideoCodec,
    policy: SenderPolicy,
    caps: &MediaRuntimeCaps,
) -> Result<Box<dyn VideoEncoder>> {
    if matches!(policy, SenderPolicy::AutoLowLatency) && codec == pb::VideoCodec::Av1 {
        let sw_enabled = cfg!(feature = "video-av1-software");
        let has_hw_av1 = caps
            .encode_backends
            .get(&pb::VideoCodec::Av1)
            .map(|backends| {
                backends
                    .iter()
                    .any(|b| matches!(b, EncodeBackendKind::MfHwAv1 | EncodeBackendKind::VaapiAv1))
            })
            .unwrap_or(false);
        if !has_hw_av1 && !sw_enabled {
            bail!(
                "AV1 software encoder is disabled for interactive mode; enable `video-av1-software` or provide hardware AV1"
            );
        }
    }

    let requested = env_video_encoder_override().unwrap_or_else(|| "auto".to_string());
    let backends = caps
        .encode_backends
        .get(&codec)
        .ok_or_else(|| anyhow!("no runtime backends available for codec {codec:?}"))?;

    let encoder = match codec {
        pb::VideoCodec::Vp9 => vp9::build_vp9_encoder(backends)?,
        pb::VideoCodec::Av1 => av1::build_av1_encoder(backends, policy)?,
        _ => bail!("unsupported screen encoder codec {codec:?}"),
    };

    info!(
        codec = ?codec,
        policy = policy.as_str(),
        env_override = %requested,
        backend = encoder.backend_name(),
        "[video] selected screen encoder backend"
    );

    Ok(encoder)
}
