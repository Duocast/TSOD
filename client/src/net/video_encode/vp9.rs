use anyhow::{anyhow, bail, Result};
use tracing::warn;

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_frame::{EncodedAccessUnit, FramePlanes, PixelFormat, VideoFrame};
use crate::net::vpx_codec::{self, LibvpxEncoder};
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::runtime_probe::EncodeBackendKind;

/// Build a real VP9 encoder for the first usable backend in `backends`.
///
/// Backend selection:
/// - `Libvpx`  → software VP9 via libvpx (supported)
/// - `MfHwVp9` → Windows Media Foundation hardware VP9 (not yet implemented; skipped)
/// - `VaapiVp9`→ Linux VAAPI hardware VP9 (not yet implemented; skipped)
///
/// Skipped backends are **not** removed from the preference list here; runtime
/// probing calls `can_initialize_backend` and will never advertise a backend
/// that returns an error.
pub fn build_vp9_encoder(backends: &[EncodeBackendKind]) -> Result<Box<dyn VideoEncoder>> {
    for backend in backends {
        match backend {
            EncodeBackendKind::Libvpx => {
                // Eagerly verify the library can init before advertising.
                match Vp9RealtimeEncoder::new_libvpx() {
                    Ok(enc) => return Ok(Box::new(enc)),
                    Err(err) => {
                        warn!(error = %err, "[vp9] libvpx init failed, skipping");
                        continue;
                    }
                }
            }
            EncodeBackendKind::MfHwVp9 | EncodeBackendKind::VaapiVp9 => {
                // Hardware VP9 paths are not yet implemented.
                // Returning an error here ensures `can_initialize_backend`
                // returns false and the backend is never advertised.
                continue;
            }
            _ => continue,
        }
    }
    Err(anyhow!("no VP9 encoder backend available"))
}

// ── Internal state ────────────────────────────────────────────────────────────

pub struct Vp9RealtimeEncoder {
    encoder: LibvpxEncoder,
    config: VideoSessionConfig,
    force_next_keyframe: bool,
}

impl Vp9RealtimeEncoder {
    fn new_libvpx() -> Result<Self> {
        // Bootstrap with minimal config; reconfigured on `configure_session`.
        let encoder = LibvpxEncoder::new(16, 16, 30, 500_000)?;
        Ok(Self {
            encoder,
            config: VideoSessionConfig {
                width: 16,
                height: 16,
                fps: 30,
                target_bitrate_bps: 500_000,
                low_latency: true,
                allow_frame_drop: true,
            },
            force_next_keyframe: false,
        })
    }
}

impl VideoEncoder for Vp9RealtimeEncoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        if config.width == 0 || config.height == 0 {
            // Called before the first real frame; store and return.
            self.config = config;
            return Ok(());
        }

        // Re-create the encoder context if dimensions changed.
        if config.width != self.config.width || config.height != self.config.height {
            self.encoder = LibvpxEncoder::new(
                config.width,
                config.height,
                config.fps,
                config.target_bitrate_bps,
            )?;
        } else if config.target_bitrate_bps != self.config.target_bitrate_bps {
            self.encoder.update_bitrate(config.target_bitrate_bps)?;
        }

        self.config = config;
        Ok(())
    }

    fn request_keyframe(&mut self) -> Result<()> {
        self.force_next_keyframe = true;
        Ok(())
    }

    fn update_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        self.config.target_bitrate_bps = bitrate_bps;
        self.encoder.update_bitrate(bitrate_bps)
    }

    fn encode(&mut self, frame: VideoFrame) -> Result<EncodedAccessUnit> {
        if frame.format != PixelFormat::Bgra {
            bail!("VP9 encoder expects BGRA input, got {:?}", frame.format);
        }

        let width = frame.width as usize;
        let height = frame.height as usize;

        // Re-initialise if the session config hasn't been applied yet
        // (configure_session was called with width=0).
        if self.config.width != frame.width || self.config.height != frame.height {
            self.encoder = LibvpxEncoder::new(
                frame.width,
                frame.height,
                self.config.fps,
                self.config.target_bitrate_bps,
            )?;
            self.config.width = frame.width;
            self.config.height = frame.height;
        }

        let (y, u, v) = match &frame.planes {
            FramePlanes::Bgra { bytes, stride } => {
                vpx_codec::bgra_to_i420(bytes.as_ref(), *stride as usize, width, height)?
            }
            _ => bail!("VP9 encoder: unexpected plane format"),
        };

        let force_kf = self.force_next_keyframe;
        self.force_next_keyframe = false;

        let outputs = self.encoder.encode(&y, &u, &v, force_kf)?;

        // libvpx may emit zero packets on some frames in CBR mode (sub-frame
        // period); return an empty access unit so the transport layer can still
        // track timestamps.  is_keyframe=false on empty packets.
        let (data, is_keyframe) = if let Some(out) = outputs.into_iter().next() {
            (bytes::Bytes::from(out.data), out.is_keyframe)
        } else {
            (bytes::Bytes::new(), false)
        };

        Ok(EncodedAccessUnit {
            codec: pb::VideoCodec::Vp9,
            layer_id: 0,
            ts_ms: frame.ts_ms,
            is_keyframe,
            data,
        })
    }

    fn backend_name(&self) -> &'static str {
        "vp9-libvpx"
    }
}

/// Runtime capability probe used by `runtime_probe::verified_encode_backends`.
pub(crate) fn can_initialize_backend(backend: EncodeBackendKind) -> bool {
    match backend {
        EncodeBackendKind::Libvpx => vpx_codec::probe_encoder(),
        // Hardware VP9 paths not implemented; never advertise them.
        EncodeBackendKind::MfHwVp9 | EncodeBackendKind::VaapiVp9 => false,
        _ => false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::video_frame::{FramePlanes, VideoFrame};
    use bytes::Bytes;

    fn make_frame(width: u32, height: u32) -> VideoFrame {
        // Solid magenta (B=255, G=0, R=255, A=255) frame
        let stride = width as usize * 4;
        let mut bytes = vec![0_u8; height as usize * stride];
        for px in bytes.chunks_exact_mut(4) {
            px[0] = 255; // B
            px[1] = 0;   // G
            px[2] = 255; // R
            px[3] = 255; // A
        }
        VideoFrame {
            width,
            height,
            ts_ms: 42,
            format: PixelFormat::Bgra,
            planes: FramePlanes::Bgra {
                bytes: Bytes::from(bytes),
                stride: stride as u32,
            },
        }
    }

    fn make_encoder(width: u32, height: u32) -> Option<Vp9RealtimeEncoder> {
        if !vpx_codec::probe_encoder() {
            return None; // libvpx not available on this system
        }
        let mut enc = Vp9RealtimeEncoder::new_libvpx().unwrap();
        enc.configure_session(VideoSessionConfig {
            width,
            height,
            fps: 30,
            target_bitrate_bps: 2_000_000,
            low_latency: true,
            allow_frame_drop: true,
        })
        .unwrap();
        Some(enc)
    }

    #[test]
    fn keyframe_request_marks_next_output() {
        let mut enc = match make_encoder(64, 64) {
            Some(e) => e,
            None => return, // libvpx absent; skip
        };
        enc.request_keyframe().unwrap();
        let au = enc.encode(make_frame(64, 64)).unwrap();
        assert!(au.is_keyframe, "forced keyframe should be marked");
    }

    #[test]
    fn bitrate_update_does_not_crash() {
        let mut enc = match make_encoder(64, 64) {
            Some(e) => e,
            None => return,
        };
        enc.update_bitrate(500_000).unwrap();
        // Encode a frame to confirm the encoder still works after the update.
        enc.encode(make_frame(64, 64)).unwrap();
    }

    #[test]
    fn output_is_real_vp9_bitstream() {
        // A real VP9 bitstream starts with the uncompressed header which has
        // a 2-bit frame_marker = 0b10 in the most-significant bits of the
        // first byte.  Spec §7.2.
        let mut enc = match make_encoder(64, 64) {
            Some(e) => e,
            None => return,
        };
        // Force a keyframe so we know the first packet is a complete frame.
        enc.request_keyframe().unwrap();
        let au = enc.encode(make_frame(64, 64)).unwrap();
        assert!(!au.data.is_empty(), "should produce a non-empty bitstream");
        // VP9 frame marker is 0b10 in bits [7:6]
        let marker = (au.data[0] >> 6) & 0b11;
        assert_eq!(marker, 0b10, "VP9 frame_marker expected 0b10, got {marker:#04b}");
    }

    #[test]
    fn backend_name_is_libvpx() {
        if let Some(enc) = make_encoder(16, 16) {
            assert_eq!(enc.backend_name(), "vp9-libvpx");
        }
    }

    #[test]
    fn non_bgra_input_returns_error() {
        let mut enc = match make_encoder(16, 16) {
            Some(e) => e,
            None => return,
        };
        let frame = VideoFrame {
            width: 16,
            height: 16,
            ts_ms: 0,
            format: PixelFormat::Nv12,
            planes: FramePlanes::Nv12 {
                y: bytes::Bytes::from(vec![0u8; 256]),
                uv: bytes::Bytes::from(vec![128u8; 128]),
                y_stride: 16,
                uv_stride: 16,
            },
        };
        assert!(enc.encode(frame).is_err());
    }
}
