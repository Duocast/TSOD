use anyhow::{anyhow, Context, Result};

use crate::proto::voiceplatform::v1 as pb;

use crate::media_codec::{DecodedVideoFrame, VideoDecoder};

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

struct Av1AvifDecoder;

impl VideoDecoder for Av1AvifDecoder {
    fn decode(&mut self, encoded: &[u8]) -> Result<DecodedVideoFrame> {
        let image = image::load_from_memory(encoded).context("decode AV1/AVIF frame")?;
        let rgba = image.to_rgba8();
        Ok(DecodedVideoFrame {
            width: rgba.width() as usize,
            height: rgba.height() as usize,
            rgba: rgba.into_raw(),
        })
    }
}

pub fn decoder_for_codec(codec: pb::VideoCodec) -> Option<Box<dyn VideoDecoder>> {
    match codec {
        pb::VideoCodec::Av1 if cfg!(feature = "video-av1") => Some(Box::new(Av1AvifDecoder)),
        // TODO(video-vp9): replace AVIF-frame fallback with a realtime VP9 decoder.
        pb::VideoCodec::Vp9 if cfg!(feature = "video-vp9") => Some(Box::new(Av1AvifDecoder)),
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
