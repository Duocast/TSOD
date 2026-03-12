use anyhow::{anyhow, bail, Result};

use crate::media_codec::VideoEncoder;
use crate::screen_share::config::SenderPolicy;
use crate::screen_share::runtime_probe::EncodeBackendKind;

pub mod av1_mf;
pub mod av1_svt;
pub mod av1_vaapi;

pub fn build_av1_encoder(
    backends: &[EncodeBackendKind],
    policy: SenderPolicy,
) -> Result<Box<dyn VideoEncoder>> {
    for backend in backends {
        match backend {
            EncodeBackendKind::HardwareAv1 => {
                #[cfg(target_os = "windows")]
                {
                    return Ok(Box::new(av1_mf::Av1MfEncoder::new()));
                }
                #[cfg(target_os = "linux")]
                {
                    return Ok(Box::new(av1_vaapi::Av1VaapiEncoder::new()));
                }
            }
            EncodeBackendKind::SvtAv1 if cfg!(feature = "video-av1-software") => {
                return Ok(Box::new(av1_svt::Av1SvtEncoder::new()));
            }
            _ => continue,
        }
    }
    if matches!(policy, SenderPolicy::AutoLowLatency) {
        bail!("interactive AV1 encode requires hardware or video-av1-software")
    }
    Err(anyhow!("no AV1 encoder backend available"))
}
