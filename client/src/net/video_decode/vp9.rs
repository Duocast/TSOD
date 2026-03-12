use anyhow::{anyhow, Result};

use crate::media_codec::VideoDecoder;
use crate::screen_share::runtime_probe::DecodeBackendKind;

pub mod vp9_libvpx;
pub mod vp9_mf;
pub mod vp9_vaapi;

pub fn build_vp9_decoder(backends: &[DecodeBackendKind]) -> Result<Box<dyn VideoDecoder>> {
    for backend in backends {
        match backend {
            DecodeBackendKind::HardwareVp9 => {
                #[cfg(target_os = "windows")]
                {
                    return Ok(Box::new(vp9_mf::Vp9MfDecoder::new()));
                }
                #[cfg(target_os = "linux")]
                {
                    return Ok(Box::new(vp9_vaapi::Vp9VaapiDecoder::new()));
                }
            }
            DecodeBackendKind::Libvpx => return Ok(Box::new(vp9_libvpx::Vp9LibvpxDecoder::new())),
            _ => continue,
        }
    }
    Err(anyhow!("no VP9 decoder backend available"))
}
