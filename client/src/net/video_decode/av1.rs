use anyhow::{anyhow, Result};

use crate::media_codec::VideoDecoder;
use crate::screen_share::runtime_probe::DecodeBackendKind;

pub mod av1_dav1d;
pub mod av1_mf;
pub mod av1_vaapi;

pub fn build_av1_decoder(backends: &[DecodeBackendKind]) -> Result<Box<dyn VideoDecoder>> {
    for backend in backends {
        match backend {
            DecodeBackendKind::HardwareAv1 => {
                #[cfg(target_os = "windows")]
                {
                    return Ok(Box::new(av1_mf::Av1MfDecoder::new()));
                }
                #[cfg(target_os = "linux")]
                {
                    return Ok(Box::new(av1_vaapi::Av1VaapiDecoder::new()));
                }
            }
            DecodeBackendKind::Dav1d => return Ok(Box::new(av1_dav1d::Av1Dav1dDecoder::new())),
            _ => continue,
        }
    }
    Err(anyhow!("no AV1 decoder backend available"))
}
