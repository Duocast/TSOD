use anyhow::{anyhow, bail, Result};

use crate::proto::voiceplatform::v1 as pb;

use crate::media_codec::{DecodedVideoFrame, VideoDecoder};

const RAW_VIDEO_MAGIC: [u8; 4] = *b"TSRV";

pub struct VideoDecoderCache {
    decoders: std::collections::HashMap<pb::VideoCodec, Box<dyn VideoDecoder>>,
}

impl VideoDecoderCache {
    pub fn new() -> Self {
        Self {
            decoders: std::collections::HashMap::new(),
        }
    }

    pub fn decode(&mut self, codec: pb::VideoCodec, encoded: &[u8]) -> Result<DecodedVideoFrame> {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.decoders.entry(codec) {
            let decoder = decoder_for_codec(codec)
                .ok_or_else(|| anyhow!("no decoder available for codec {codec:?}"))?;
            slot.insert(decoder);
        }
        self.decoders
            .get_mut(&codec)
            .expect("decoder inserted above")
            .decode(encoded)
    }
}

#[derive(Clone, Copy)]
enum Av1DecoderBackend {
    Hardware,
    Dav1d,
}

struct Av1RealtimeDecoder {
    backend: Av1DecoderBackend,
}

impl Av1RealtimeDecoder {
    fn new() -> Self {
        let backend = if std::env::var("VP_AV1_DISABLE_HW").ok().as_deref() == Some("1") {
            Av1DecoderBackend::Dav1d
        } else {
            Av1DecoderBackend::Hardware
        };
        Self { backend }
    }
}

impl VideoDecoder for Av1RealtimeDecoder {
    fn decode(&mut self, encoded: &[u8]) -> Result<DecodedVideoFrame> {
        decode_realtime_payload(encoded, self.backend as u8)
    }
}

struct Vp9LibvpxDecoder;

impl VideoDecoder for Vp9LibvpxDecoder {
    fn decode(&mut self, encoded: &[u8]) -> Result<DecodedVideoFrame> {
        decode_realtime_payload(encoded, 0)
    }
}

fn decode_realtime_payload(encoded: &[u8], _backend_hint: u8) -> Result<DecodedVideoFrame> {
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
        .ok_or_else(|| anyhow!("decoded frame dimensions overflow"))?;
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
    })
}

pub fn decoder_for_codec(codec: pb::VideoCodec) -> Option<Box<dyn VideoDecoder>> {
    match codec {
        pb::VideoCodec::Av1 if cfg!(feature = "video-av1") => {
            Some(Box::new(Av1RealtimeDecoder::new()))
        }
        pb::VideoCodec::Vp9 if cfg!(feature = "video-vp9") => Some(Box::new(Vp9LibvpxDecoder)),
        _ => None,
    }
}

pub fn decode_video_frame(codec: pb::VideoCodec, encoded: &[u8]) -> Result<DecodedVideoFrame> {
    let mut cache = VideoDecoderCache::new();
    cache.decode(codec, encoded)
}

pub fn available_decodable_codecs() -> Vec<pb::VideoCodec> {
    let mut codecs = Vec::with_capacity(2);
    if cfg!(feature = "video-av1") {
        codecs.push(pb::VideoCodec::Av1);
    }
    if cfg!(feature = "video-vp9") {
        codecs.push(pb::VideoCodec::Vp9);
    }
    codecs
}
