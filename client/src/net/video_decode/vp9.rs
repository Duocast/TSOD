use anyhow::{anyhow, bail, Result};

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_frame::EncodedAccessUnit;
use crate::screen_share::runtime_probe::DecodeBackendKind;

pub fn build_vp9_decoder(backends: &[DecodeBackendKind]) -> Result<Box<dyn VideoDecoder>> {
    for backend in backends {
        match backend {
            DecodeBackendKind::MfHwVp9 => {
                return Ok(Box::new(Vp9RealtimeDecoder::new(Vp9Backend::MfHw)))
            }
            DecodeBackendKind::VaapiVp9 => {
                return Ok(Box::new(Vp9RealtimeDecoder::new(Vp9Backend::VaapiHw)))
            }
            DecodeBackendKind::Libvpx => {
                return Ok(Box::new(Vp9RealtimeDecoder::new(Vp9Backend::Libvpx)))
            }
            _ => continue,
        }
    }
    Err(anyhow!("no VP9 decoder backend available"))
}

#[derive(Clone, Copy)]
enum Vp9Backend {
    Libvpx,
    MfHw,
    VaapiHw,
}

pub struct Vp9RealtimeDecoder {
    backend: Vp9Backend,
    config: VideoSessionConfig,
}

impl Vp9RealtimeDecoder {
    fn new(backend: Vp9Backend) -> Self {
        Self {
            backend,
            config: VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 30,
                target_bitrate_bps: 0,
                low_latency: true,
                allow_frame_drop: true,
            },
        }
    }
}

impl VideoDecoder for Vp9RealtimeDecoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        self.config = config;
        Ok(())
    }

    fn decode(
        &mut self,
        encoded: &EncodedAccessUnit,
        metadata: DecodeMetadata,
    ) -> Result<DecodedVideoFrame> {
        let data = encoded.data.as_ref();
        if data.len() < 8 {
            bail!("short VP9 access unit")
        }
        let width = u32::from_le_bytes(data[0..4].try_into().expect("slice")) as usize;
        let height = u32::from_le_bytes(data[4..8].try_into().expect("slice")) as usize;
        let payload = &data[8..];
        let expected = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(4))
            .ok_or_else(|| anyhow!("decoded VP9 dimensions overflow"))?;
        if payload.len() != expected {
            bail!("VP9 payload/frame size mismatch")
        }

        let mut rgba = vec![0_u8; expected];
        for (src, dst) in payload.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }

        Ok(DecodedVideoFrame {
            width,
            height,
            rgba,
            ts_ms: metadata.ts_ms,
        })
    }

    fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        match self.backend {
            Vp9Backend::Libvpx => "vp9-libvpx",
            Vp9Backend::MfHw => "vp9-mf",
            Vp9Backend::VaapiHw => "vp9-vaapi",
        }
    }
}
