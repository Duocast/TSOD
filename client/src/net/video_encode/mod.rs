use anyhow::{anyhow, Result};

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_frame::{EncodedAccessUnit, FramePlanes, PixelFormat, VideoFrame};
use crate::proto::voiceplatform::v1 as pb;

pub mod av1;
pub mod vp9;

pub const RAW_VIDEO_MAGIC: [u8; 4] = *b"TSRV";

pub fn build_screen_encoder(codec: &str, _profile: &str) -> Result<Box<dyn VideoEncoder>> {
    match codec {
        "AV1" if cfg!(feature = "video-av1-software") => {
            Ok(Box::new(av1::Av1RealtimeEncoder::new()))
        }
        "VP9" if cfg!(feature = "video-vp9") => Ok(Box::new(vp9::Vp9RealtimeEncoder::new())),
        other => Err(anyhow!(
            "unsupported screen codec '{}'; enable matching feature",
            other
        )),
    }
}

pub(crate) fn encode_raw_payload(
    frame: VideoFrame,
    codec: pb::VideoCodec,
    backend_tag: u8,
    force_keyframe: bool,
    frame_seq: u32,
) -> Result<EncodedAccessUnit> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let mut bgra = vec![0_u8; width * height * 4];

    match (&frame.format, frame.planes) {
        (PixelFormat::Bgra, FramePlanes::Bgra { bytes, stride }) => {
            let src = bytes;
            let stride = stride as usize;
            for y in 0..height {
                let src_row = &src[y * stride..(y * stride) + width * 4];
                let dst_row = &mut bgra[y * width * 4..(y + 1) * width * 4];
                dst_row.copy_from_slice(src_row);
            }
        }
        (PixelFormat::Nv12, FramePlanes::Nv12 { .. }) => {
            return Err(anyhow!("NV12 screen encoding is not implemented"));
        }
        _ => {
            return Err(anyhow!("pixel format and frame planes mismatch"));
        }
    }

    let mut encoded = Vec::with_capacity(16 + bgra.len());
    encoded.extend_from_slice(&RAW_VIDEO_MAGIC);
    encoded.push(backend_tag);
    encoded.push(0);
    encoded.extend_from_slice(&frame.width.to_le_bytes());
    encoded.extend_from_slice(&frame.height.to_le_bytes());
    encoded.extend_from_slice(&bgra);

    Ok(EncodedAccessUnit {
        codec,
        layer_id: 0,
        ts_ms: frame.ts_ms,
        is_keyframe: force_keyframe || frame_seq % 60 == 0,
        data: bytes::Bytes::from(encoded),
    })
}

pub(crate) fn apply_config(config: &mut VideoSessionConfig, update: VideoSessionConfig) {
    *config = update;
}
