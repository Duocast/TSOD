use anyhow::{anyhow, Context, Result};

use crate::proto::voiceplatform::v1 as pb;

pub struct DecodedVideoFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub trait VideoDecoder: Send {
    fn decode(&mut self, encoded: &[u8]) -> Result<DecodedVideoFrame>;
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
        _ => None,
    }
}

pub fn decode_video_frame(codec: pb::VideoCodec, encoded: &[u8]) -> Result<DecodedVideoFrame> {
    let mut decoder = decoder_for_codec(codec)
        .ok_or_else(|| anyhow!("no decoder available for codec {codec:?}"))?;
    decoder.decode(encoded)
}

pub fn available_decodable_codecs() -> Vec<pb::VideoCodec> {
    let mut codecs = Vec::with_capacity(1);
    if cfg!(feature = "video-av1") {
        codecs.push(pb::VideoCodec::Av1);
    }
    codecs
}
