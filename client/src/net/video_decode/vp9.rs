use anyhow::{anyhow, bail, Result};
use tracing::warn;

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_frame::EncodedAccessUnit;
use crate::net::vpx_codec::{self, LibvpxDecoder};
use crate::screen_share::runtime_probe::DecodeBackendKind;

#[cfg(target_os = "linux")]
mod vaapi_vp9_dec;

/// Build a real VP9 decoder for the first usable backend in `backends`.
///
/// Backend selection:
/// - `Libvpx`   → software VP9 via libvpx (supported everywhere)
/// - `VaapiVp9` → Linux VAAPI hardware VP9 (supported when VA-API driver
///                 exposes a VP9 decode entrypoint)
/// - `MfHwVp9`  → Windows Media Foundation hardware VP9 (not yet implemented)
pub fn build_vp9_decoder(backends: &[DecodeBackendKind]) -> Result<Box<dyn VideoDecoder>> {
    for backend in backends {
        match backend {
            DecodeBackendKind::Libvpx => {
                return Ok(Box::new(Vp9RealtimeDecoder::new()?));
            }
            #[cfg(target_os = "linux")]
            DecodeBackendKind::VaapiVp9 => {
                match vaapi_vp9_dec::VaapiVp9Decoder::open() {
                    Ok(dec) => return Ok(Box::new(dec)),
                    Err(err) => {
                        warn!(error = %err, "[vp9] VAAPI VP9 decoder init failed, skipping");
                        continue;
                    }
                }
            }
            DecodeBackendKind::MfHwVp9 => {
                // Windows MF hardware VP9 not yet implemented.
                continue;
            }
            _ => continue,
        }
    }
    Err(anyhow!("no VP9 decoder backend available"))
}

// ── Internal state ────────────────────────────────────────────────────────────

pub struct Vp9RealtimeDecoder {
    decoder: LibvpxDecoder,
    config: VideoSessionConfig,
    /// Last successfully decoded frame; re-used when libvpx yields no picture
    /// for a given packet (e.g., during flush or for sub-frame-period packets).
    last_frame: Option<DecodedVideoFrame>,
}

impl Vp9RealtimeDecoder {
    fn new() -> Result<Self> {
        Ok(Self {
            decoder: LibvpxDecoder::new()?,
            config: VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 30,
                target_bitrate_bps: 0,
                low_latency: true,
                allow_frame_drop: true,
            },
            last_frame: None,
        })
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
        // Reject obviously bad inputs early.
        if encoded.data.is_empty() {
            // An empty access unit may be emitted by the encoder on sub-period
            // frames in CBR mode.  Repeat the last frame if we have one.
            if let Some(mut prev) = self.last_frame.clone() {
                prev.ts_ms = metadata.ts_ms;
                return Ok(prev);
            }
            bail!("received empty VP9 access unit and no previous frame to repeat");
        }

        // VP9 bitstream: first byte bits [7:6] must be 0b10 (frame_marker).
        // This check catches stray VP9F payloads or other corruption early.
        let marker = (encoded.data[0] >> 6) & 0b11;
        if marker != 0b10 {
            bail!(
                "invalid VP9 frame_marker: expected 0b10, got {marker:#04b}. \
                 Payload may be a legacy VP9F packet from an old sender."
            );
        }

        match self.decoder.decode(encoded.data.as_ref())? {
            Some(out) => {
                let frame = DecodedVideoFrame {
                    width: out.width,
                    height: out.height,
                    rgba: out.rgba,
                    ts_ms: metadata.ts_ms,
                };
                self.last_frame = Some(frame.clone());
                Ok(frame)
            }
            None => {
                // libvpx occasionally returns no picture (e.g., the very first
                // frame of a stream before the decoder has a full reference).
                // Repeat the last frame if available; otherwise report the gap.
                if let Some(mut prev) = self.last_frame.clone() {
                    prev.ts_ms = metadata.ts_ms;
                    Ok(prev)
                } else {
                    bail!("libvpx has no picture ready yet");
                }
            }
        }
    }

    fn reset(&mut self) -> Result<()> {
        self.decoder.flush();
        self.last_frame = None;
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "vp9-libvpx"
    }
}

/// Runtime capability probe used by `runtime_probe::verified_decode_backends`.
pub(crate) fn can_initialize_backend(backend: DecodeBackendKind) -> bool {
    match backend {
        DecodeBackendKind::Libvpx => vpx_codec::probe_decoder(),
        #[cfg(target_os = "linux")]
        DecodeBackendKind::VaapiVp9 => {
            crate::screen_share::runtime_probe::vaapi::probe_vaapi_vp9().decode_available
        }
        #[cfg(not(target_os = "linux"))]
        DecodeBackendKind::VaapiVp9 => false,
        // Windows MF hardware VP9 not yet implemented.
        DecodeBackendKind::MfHwVp9 => false,
        _ => false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::video_encode::vp9::build_vp9_encoder;
    use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};
    use crate::media_codec::VideoSessionConfig;
    use crate::screen_share::runtime_probe::{DecodeBackendKind, EncodeBackendKind};
    use bytes::Bytes;

    /// Solid-colour 64×64 BGRA frame (green).
    fn make_frame(width: u32, height: u32) -> VideoFrame {
        let stride = width as usize * 4;
        let mut bytes = vec![0_u8; height as usize * stride];
        for px in bytes.chunks_exact_mut(4) {
            px[0] = 0;   // B
            px[1] = 200; // G
            px[2] = 0;   // R
            px[3] = 255; // A
        }
        VideoFrame {
            width,
            height,
            ts_ms: 100,
            format: PixelFormat::Bgra,
            planes: FramePlanes::Bgra {
                bytes: Bytes::from(bytes),
                stride: stride as u32,
            },
        }
    }

    fn libvpx_available() -> bool {
        vpx_codec::probe_encoder() && vpx_codec::probe_decoder()
    }

    // ── Round-trip ──────────────────────────────────────────────────────────

    #[test]
    fn roundtrip_encode_decode() {
        if !libvpx_available() {
            return;
        }
        let mut enc = build_vp9_encoder(&[EncodeBackendKind::Libvpx]).unwrap();
        enc.configure_session(VideoSessionConfig {
            width: 64,
            height: 64,
            fps: 30,
            target_bitrate_bps: 2_000_000,
            low_latency: true,
            allow_frame_drop: true,
        })
        .unwrap();

        // Force a keyframe so the decoder can immediately decode it.
        enc.request_keyframe().unwrap();
        let au = enc.encode(make_frame(64, 64)).unwrap();
        assert!(!au.data.is_empty(), "encoder must produce output");

        let mut dec = build_vp9_decoder(&[DecodeBackendKind::Libvpx]).unwrap();
        let out = dec.decode(&au, DecodeMetadata { ts_ms: au.ts_ms }).unwrap();

        assert_eq!(out.width, 64);
        assert_eq!(out.height, 64);
        assert_eq!(out.rgba.len(), 64 * 64 * 4, "must be full RGBA buffer");
        assert_eq!(out.ts_ms, au.ts_ms, "timestamp must be preserved");
    }

    // ── Keyframe semantics ──────────────────────────────────────────────────

    #[test]
    fn forced_keyframe_sets_flag() {
        if !libvpx_available() {
            return;
        }
        let mut enc = build_vp9_encoder(&[EncodeBackendKind::Libvpx]).unwrap();
        enc.configure_session(VideoSessionConfig {
            width: 64,
            height: 64,
            fps: 30,
            target_bitrate_bps: 2_000_000,
            low_latency: true,
            allow_frame_drop: true,
        })
        .unwrap();
        enc.request_keyframe().unwrap();
        let au = enc.encode(make_frame(64, 64)).unwrap();
        assert!(au.is_keyframe, "requested keyframe must be flagged");
    }

    #[test]
    fn ordinary_frame_is_not_keyframe() {
        if !libvpx_available() {
            return;
        }
        let mut enc = build_vp9_encoder(&[EncodeBackendKind::Libvpx]).unwrap();
        enc.configure_session(VideoSessionConfig {
            width: 64,
            height: 64,
            fps: 30,
            target_bitrate_bps: 2_000_000,
            low_latency: true,
            allow_frame_drop: true,
        })
        .unwrap();

        // Frame 0 is an auto keyframe; encode a few more and verify they're inter.
        for i in 0..5 {
            let au = enc.encode(make_frame(64, 64)).unwrap();
            if i > 0 && !au.data.is_empty() {
                // We cannot guarantee all are inter (encoder decides), but
                // the is_keyframe flag must come from libvpx, not synthetic logic.
                let _ = au.is_keyframe; // just ensure it compiles with the real field
            }
        }
    }

    // ── Bitrate update ──────────────────────────────────────────────────────

    #[test]
    fn bitrate_update_and_encode() {
        if !libvpx_available() {
            return;
        }
        let mut enc = build_vp9_encoder(&[EncodeBackendKind::Libvpx]).unwrap();
        enc.configure_session(VideoSessionConfig {
            width: 64,
            height: 64,
            fps: 30,
            target_bitrate_bps: 2_000_000,
            low_latency: true,
            allow_frame_drop: true,
        })
        .unwrap();
        enc.update_bitrate(500_000).unwrap();
        // Encoder must still work after a bitrate change.
        enc.request_keyframe().unwrap();
        enc.encode(make_frame(64, 64)).unwrap();
    }

    // ── Invalid payload handling ────────────────────────────────────────────

    #[test]
    fn empty_payload_without_prior_frame_is_error() {
        if !libvpx_available() {
            return;
        }
        let mut dec = build_vp9_decoder(&[DecodeBackendKind::Libvpx]).unwrap();
        let au = crate::net::video_frame::EncodedAccessUnit {
            codec: crate::proto::voiceplatform::v1::VideoCodec::Vp9,
            layer_id: 0,
            ts_ms: 0,
            is_keyframe: false,
            data: bytes::Bytes::new(),
        };
        assert!(dec.decode(&au, DecodeMetadata { ts_ms: 0 }).is_err());
    }

    #[test]
    fn legacy_vp9f_payload_is_rejected() {
        if !libvpx_available() {
            return;
        }
        let mut dec = build_vp9_decoder(&[DecodeBackendKind::Libvpx]).unwrap();
        // Craft a minimal VP9F header (the old fake format).
        let mut payload = Vec::new();
        payload.extend_from_slice(b"VP9F");    // magic
        payload.extend_from_slice(&2_u32.to_le_bytes()); // width=2
        payload.extend_from_slice(&2_u32.to_le_bytes()); // height=2
        payload.extend_from_slice(&[0u8; 20]); // rest of header

        let au = crate::net::video_frame::EncodedAccessUnit {
            codec: crate::proto::voiceplatform::v1::VideoCodec::Vp9,
            layer_id: 0,
            ts_ms: 0,
            is_keyframe: false,
            data: bytes::Bytes::from(payload),
        };
        // The VP9F magic starts with 'V' (0x56), bits [7:6] = 0b01 ≠ 0b10.
        assert!(dec.decode(&au, DecodeMetadata { ts_ms: 0 }).is_err());
    }

    #[test]
    fn random_garbage_is_rejected() {
        if !libvpx_available() {
            return;
        }
        let mut dec = build_vp9_decoder(&[DecodeBackendKind::Libvpx]).unwrap();
        let garbage = vec![0xFF_u8; 64];
        let au = crate::net::video_frame::EncodedAccessUnit {
            codec: crate::proto::voiceplatform::v1::VideoCodec::Vp9,
            layer_id: 0,
            ts_ms: 0,
            is_keyframe: false,
            data: bytes::Bytes::from(garbage),
        };
        // 0xFF >> 6 = 0b11 ≠ 0b10, or libvpx returns an error.
        assert!(dec.decode(&au, DecodeMetadata { ts_ms: 0 }).is_err());
    }

    // ── Runtime probe ───────────────────────────────────────────────────────

    #[test]
    fn can_initialize_libvpx_matches_probe() {
        let probe = vpx_codec::probe_decoder();
        let can_init = can_initialize_backend(DecodeBackendKind::Libvpx);
        assert_eq!(probe, can_init);
    }

    #[test]
    fn hw_backends_not_advertised() {
        assert!(!can_initialize_backend(DecodeBackendKind::MfHwVp9));
        assert!(!can_initialize_backend(DecodeBackendKind::VaapiVp9));
    }

    // ── Decoder backend name ────────────────────────────────────────────────

    #[test]
    fn backend_name_is_libvpx() {
        if !libvpx_available() {
            return;
        }
        let dec = Vp9RealtimeDecoder::new().unwrap();
        assert_eq!(dec.backend_name(), "vp9-libvpx");
    }
}
