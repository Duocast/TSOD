use anyhow::{anyhow, Result};

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_frame::EncodedAccessUnit;
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::runtime_probe::DecodeBackendKind;

pub mod av1;
pub mod vp9;

pub struct VideoDecoderCache {
    decoders: std::collections::HashMap<(pb::VideoCodec, u8), Box<dyn VideoDecoder>>,
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
        let key = (encoded.codec, encoded.layer_id);
        if let std::collections::hash_map::Entry::Vacant(slot) = self.decoders.entry(key) {
            let mut decoder = decoder_for_codec(encoded.codec)
                .ok_or_else(|| anyhow!("no decoder available for codec {:?}", encoded.codec))?;
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
            .get_mut(&key)
            .expect("decoder inserted above")
            .decode(encoded, metadata)
    }
}

pub fn decoder_for_codec(codec: pb::VideoCodec) -> Option<Box<dyn VideoDecoder>> {
    match codec {
        pb::VideoCodec::Av1 if cfg!(feature = "video-av1-decode") => {
            let backends = vec![
                DecodeBackendKind::MfHwAv1,
                DecodeBackendKind::VaapiAv1,
                DecodeBackendKind::Dav1d,
            ];
            av1::build_av1_decoder(&backends).ok()
        }
        pb::VideoCodec::Vp9 if cfg!(feature = "video-vp9") => {
            let backends = vec![
                DecodeBackendKind::MfHwVp9,
                DecodeBackendKind::VaapiVp9,
                DecodeBackendKind::Libvpx,
            ];
            vp9::build_vp9_decoder(&backends).ok()
        }
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
