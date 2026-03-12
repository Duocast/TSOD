use anyhow::{anyhow, Result};

use crate::media_codec::VideoEncoder;
use crate::screen_share::runtime_probe::EncodeBackendKind;

mod vp9_impl {
    pub use super::vp9_libvpx::Vp9LibvpxEncoder;
    pub use super::vp9_mf::Vp9MfEncoder;
    pub use super::vp9_vaapi::Vp9VaapiEncoder;
}

pub mod vp9_libvpx;
pub mod vp9_mf;
pub mod vp9_vaapi;

pub fn build_vp9_encoder(backends: &[EncodeBackendKind]) -> Result<Box<dyn VideoEncoder>> {
    for backend in backends {
        match backend {
            EncodeBackendKind::HardwareVp9 => {
                #[cfg(target_os = "windows")]
                {
                    return Ok(Box::new(vp9_impl::Vp9MfEncoder::new()));
                }
                #[cfg(target_os = "linux")]
                {
                    return Ok(Box::new(vp9_impl::Vp9VaapiEncoder::new()));
                }
            }
            EncodeBackendKind::Libvpx => return Ok(Box::new(vp9_impl::Vp9LibvpxEncoder::new())),
            _ => continue,
        }
    }
    Err(anyhow!("no VP9 encoder backend available"))
}
