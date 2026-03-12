use anyhow::{anyhow, bail, Result};

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_frame::EncodedAccessUnit;
use crate::screen_share::runtime_probe::DecodeBackendKind;

pub fn build_av1_decoder(backends: &[DecodeBackendKind]) -> Result<Box<dyn VideoDecoder>> {
    for backend in backends {
        match backend {
            DecodeBackendKind::MfHwAv1 => {
                return Ok(Box::new(Av1RealtimeDecoder::new(Av1Backend::MfHw)))
            }
            DecodeBackendKind::VaapiAv1 => {
                return Ok(Box::new(Av1RealtimeDecoder::new(Av1Backend::VaapiHw)))
            }
            DecodeBackendKind::Dav1d => {
                return Ok(Box::new(Av1RealtimeDecoder::new(Av1Backend::Dav1d)))
            }
            _ => continue,
        }
    }

    Err(anyhow!("no AV1 decoder backend available"))
}

#[derive(Clone, Copy)]
enum Av1Backend {
    MfHw,
    VaapiHw,
    Dav1d,
}

pub struct Av1RealtimeDecoder {
    backend: Av1Backend,
    config: VideoSessionConfig,
}

impl Av1RealtimeDecoder {
    fn new(backend: Av1Backend) -> Self {
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

impl VideoDecoder for Av1RealtimeDecoder {
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
            bail!("short AV1 access unit")
        }
        let width = u32::from_le_bytes(data[0..4].try_into().expect("slice")) as usize;
        let height = u32::from_le_bytes(data[4..8].try_into().expect("slice")) as usize;
        let payload = &data[8..];
        let expected = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(4))
            .ok_or_else(|| anyhow!("decoded AV1 dimensions overflow"))?;
        if payload.len() != expected {
            bail!("AV1 payload/frame size mismatch")
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
            Av1Backend::MfHw => "av1-mf",
            Av1Backend::VaapiHw => "av1-vaapi",
            Av1Backend::Dav1d => "av1-dav1d",
        }
    }
}
