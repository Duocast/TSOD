use anyhow::{anyhow, Result};

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_frame::EncodedAccessUnit;
use crate::proto::voiceplatform::v1 as pb;

pub mod av1;
pub mod vp9;

pub struct VideoDecoderCache {
    decoders: std::collections::HashMap<pb::VideoCodec, Box<dyn VideoDecoder>>,
}

impl VideoDecoderCache {
    pub fn new() -> Self {
        Self {
            decoders: std::collections::HashMap::new(),
        }
    }

    pub fn decode(
        &mut self,
        encoded: &EncodedAccessUnit,
        metadata: DecodeMetadata,
    ) -> Result<DecodedVideoFrame> {
        let codec = encoded.codec;
        if let std::collections::hash_map::Entry::Vacant(slot) = self.decoders.entry(codec) {
            let mut decoder = decoder_for_codec(codec)
                .ok_or_else(|| anyhow!("no decoder available for codec {codec:?}"))?;
            decoder.configure_session(VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 30,
                target_bitrate_bps: 0,
                low_latency: true,
                allow_frame_drop: true,
            })?;
            slot.insert(decoder);
        }
        self.decoders
            .get_mut(&codec)
            .expect("decoder inserted above")
            .decode(encoded, metadata)
    }
}

pub fn decoder_for_codec(codec: pb::VideoCodec) -> Option<Box<dyn VideoDecoder>> {
    match codec {
        pb::VideoCodec::Av1 if cfg!(feature = "video-av1-decode") => {
            Some(Box::new(av1::Av1RealtimeDecoder::new()))
        }
        pb::VideoCodec::Vp9 if cfg!(feature = "video-vp9") => Some(Box::new(vp9::Vp9LibvpxDecoder)),
        _ => None,
    }
}

pub fn decode_video_frame(encoded: &EncodedAccessUnit) -> Result<DecodedVideoFrame> {
    let mut cache = VideoDecoderCache::new();
    cache.decode(encoded, DecodeMetadata { ts_ms: 0 })
}

pub fn available_decodable_codecs() -> Vec<pb::VideoCodec> {
    let mut codecs = Vec::with_capacity(2);
    if cfg!(feature = "video-av1-decode") {
        codecs.push(pb::VideoCodec::Av1);
    }
    if cfg!(feature = "video-vp9") {
        codecs.push(pb::VideoCodec::Vp9);
    }
    codecs
}
