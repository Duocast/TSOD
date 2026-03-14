use anyhow::Result;

use crate::media_audio_loopback::AudioLoopbackBackend;
use crate::screen_share::runtime_probe::{MediaRuntimeCaps, SystemAudioBackendKind};
use crate::ShareSource;

#[cfg(target_os = "linux")]
pub mod pipewire_monitor;
#[cfg(target_os = "windows")]
pub mod wasapi_loopback;

pub fn build_system_audio_backend(
    runtime_caps: &MediaRuntimeCaps,
    share_source: &ShareSource,
) -> Result<Option<Box<dyn AudioLoopbackBackend>>> {
    tracing::info!(source=?share_source, "[audio-share] selecting system audio backend");
    for backend in &runtime_caps.audio_backends {
        match backend {
            SystemAudioBackendKind::WasapiLoopback => {
                #[cfg(target_os = "windows")]
                {
                    return Ok(Some(Box::new(wasapi_loopback::WasapiLoopback::new()?)));
                }
            }
            SystemAudioBackendKind::PipewireMonitor => {
                #[cfg(target_os = "linux")]
                {
                    return Ok(Some(Box::new(pipewire_monitor::PipeWireMonitor::new()?)));
                }
            }
            SystemAudioBackendKind::Off => return Ok(None),
        }
    }
    Ok(None)
}
