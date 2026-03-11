use anyhow::{bail, Result};

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_encode::RAW_VIDEO_MAGIC;

#[derive(Clone, Copy)]
enum Av1DecoderBackend {
    Hardware,
    Dav1d,
}

pub struct Av1RealtimeDecoder {
    backend: Av1DecoderBackend,
    config: VideoSessionConfig,
}

impl Av1RealtimeDecoder {
    pub fn new() -> Self {
        let backend = if std::env::var("VP_AV1_DISABLE_HW").ok().as_deref() == Some("1") {
            Av1DecoderBackend::Dav1d
        } else {
            Av1DecoderBackend::Hardware
        };
        Self {
            backend,
            config: VideoSessionConfig {
                width: 0,
                height: 0,
                target_bitrate_bps: 0,
            },
        }
    }
}

impl VideoDecoder for Av1RealtimeDecoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        self.config = config;
        Ok(())
    }

    fn decode(&mut self, encoded: &[u8], metadata: DecodeMetadata) -> Result<DecodedVideoFrame> {
        let _backend_hint = self.backend as u8;
        decode_realtime_payload(encoded, metadata)
    }
}

pub(crate) fn decode_realtime_payload(
    encoded: &[u8],
    metadata: DecodeMetadata,
) -> Result<DecodedVideoFrame> {
    if encoded.len() < 14 {
        bail!("short realtime video payload");
    }
    if encoded[..4] != RAW_VIDEO_MAGIC {
        bail!("unexpected video payload format (not realtime stream payload)");
    }
    let width = u32::from_le_bytes(encoded[6..10].try_into().expect("slice length")) as usize;
    let height = u32::from_le_bytes(encoded[10..14].try_into().expect("slice length")) as usize;
    let pixel_len = width
        .checked_mul(height)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("decoded frame dimensions overflow"))?;
    let payload = &encoded[14..];
    if payload.len() != pixel_len {
        bail!("decoded frame payload size mismatch");
    }

    let mut rgba = vec![0_u8; pixel_len];
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
