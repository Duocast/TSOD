use anyhow::{anyhow, bail, Context, Result};
use tracing::warn;

use crate::media_codec::VideoEncoder;
use crate::screen_share::config::{env_video_encoder_override, SenderPolicy};
use crate::screen_share::runtime_probe::EncodeBackendKind;

pub mod caps;
mod nvenc;
mod svt;

pub fn build_av1_encoder(
    backends: &[EncodeBackendKind],
    policy: SenderPolicy,
) -> Result<Box<dyn VideoEncoder>> {
    let explicit = env_video_encoder_override().and_then(|value| match value.as_str() {
        "av1-nvenc" => Some(EncodeBackendKind::NvencAv1),
        "av1-svt" => Some(EncodeBackendKind::SvtAv1),
        _ => None,
    });

    if let Some(requested) = explicit {
        return build_backend(requested)
            .with_context(|| format!("explicit AV1 backend request `{requested:?}` failed"));
    }

    for backend in backends {
        match build_backend(*backend) {
            Ok(encoder) => return Ok(encoder),
            Err(err) => {
                warn!(backend = ?backend, error = %err, "[video] AV1 backend unavailable, trying fallback");
            }
        }
    }

    if matches!(policy, SenderPolicy::AutoLowLatency) {
        bail!("interactive AV1 encode requires NVENC AV1 or SVT-AV1 backend")
    }

    Err(anyhow!("no AV1 encoder backend available"))
}

fn build_backend(backend: EncodeBackendKind) -> Result<Box<dyn VideoEncoder>> {
    match backend {
        EncodeBackendKind::NvencAv1 => nvenc::build_nvenc_encoder(),
        EncodeBackendKind::SvtAv1 if cfg!(feature = "video-av1-software") => {
            svt::build_svt_encoder()
        }
        _ => bail!("unsupported AV1 backend {backend:?}"),
    }
}

pub(crate) fn can_initialize_backend(backend: EncodeBackendKind) -> bool {
    build_backend(backend).is_ok()
}
