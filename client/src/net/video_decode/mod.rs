use anyhow::{anyhow, Result};

use crate::media_codec::{DecodeMetadata, DecodedVideoFrame, VideoDecoder, VideoSessionConfig};
use crate::net::video_frame::EncodedAccessUnit;
use crate::proto::voiceplatform::v1 as pb;
use crate::screen_share::runtime_probe::{probe_media_caps, DecodeBackendKind, MediaRuntimeCaps};

pub mod av1;
pub mod vp9;

fn default_probe_source() -> crate::ShareSource {
    #[cfg(target_os = "windows")]
    {
        crate::ShareSource::WindowsDisplay("0".to_string())
    }
    #[cfg(target_os = "linux")]
    {
        crate::ShareSource::LinuxPortal(String::new())
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        crate::ShareSource::WindowsDisplay("0".to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeStreamState {
    Healthy,
    NeedKeyframe,
}

pub struct VideoDecoderCache {
    caps: MediaRuntimeCaps,
    decoders: std::collections::HashMap<(pb::VideoCodec, u8), Box<dyn VideoDecoder>>,
    stream_state: std::collections::HashMap<(pb::VideoCodec, u8), DecodeStreamState>,
}

impl VideoDecoderCache {
    pub fn new() -> Self {
        Self {
            caps: probe_media_caps(&default_probe_source()),
            decoders: std::collections::HashMap::new(),
            stream_state: std::collections::HashMap::new(),
        }
    }

    pub fn mark_stream_corrupted(&mut self, codec: pb::VideoCodec, layer_id: u8) {
        let key = (codec, layer_id);
        if let Some(decoder) = self.decoders.get_mut(&key) {
            let _ = decoder.reset();
        }
        self.stream_state
            .insert(key, DecodeStreamState::NeedKeyframe);
    }

    pub fn decode(
        &mut self,
        encoded: &EncodedAccessUnit,
        metadata: DecodeMetadata,
    ) -> Result<Option<DecodedVideoFrame>> {
        let key = (encoded.codec, encoded.layer_id);
        if let std::collections::hash_map::Entry::Vacant(slot) = self.decoders.entry(key) {
            let mut decoder = decoder_for_codec(encoded.codec, &self.caps)
                .ok_or_else(|| anyhow!("no decoder available for codec {:?}", encoded.codec))?;
            decoder.configure_session(VideoSessionConfig {
                width: 0,
                height: 0,
                fps: 60,
                target_bitrate_bps: 0,
                low_latency: true,
                allow_frame_drop: true,
            })?;
            slot.insert(decoder);
        }

        let state = self
            .stream_state
            .entry(key)
            .or_insert(DecodeStreamState::NeedKeyframe);
        if *state == DecodeStreamState::NeedKeyframe && !encoded.is_keyframe {
            return Ok(None);
        }
        if encoded.is_keyframe {
            *state = DecodeStreamState::Healthy;
        }

        match self
            .decoders
            .get_mut(&key)
            .expect("decoder inserted above")
            .decode(encoded, metadata)
        {
            Ok(frame) => Ok(frame),
            Err(err) => {
                if let Some(decoder) = self.decoders.get_mut(&key) {
                    let _ = decoder.reset();
                }
                *state = DecodeStreamState::NeedKeyframe;
                Err(err)
            }
        }
    }
}

pub fn decoder_for_codec(
    codec: pb::VideoCodec,
    caps: &MediaRuntimeCaps,
) -> Option<Box<dyn VideoDecoder>> {
    let backends = caps.decode_backends.get(&codec)?;
    match codec {
        pb::VideoCodec::Av1 if cfg!(feature = "video-av1-decode") => {
            av1::build_av1_decoder(backends).ok()
        }
        pb::VideoCodec::Vp9 if cfg!(feature = "video-vp9") => vp9::build_vp9_decoder(backends).ok(),
        _ => None,
    }
}

pub fn decode_video_frame(encoded: &EncodedAccessUnit) -> Result<Option<DecodedVideoFrame>> {
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
