use anyhow::{bail, Result};

use crate::media_codec::{VideoEncoder, VideoSessionConfig};
use crate::net::video_frame::{EncodedAccessUnit, VideoFrame};
use crate::screen_share::runtime_probe::nvidia::probe_nvenc_av1;

pub fn build_nvenc_encoder() -> Result<Box<dyn VideoEncoder>> {
    let status = probe_nvenc_av1();
    if !status.available {
        bail!(
            "NVENC AV1 unavailable: {}",
            status.reason.unwrap_or_else(|| "probe failed".into())
        );
    }
    Ok(Box::new(NvencAv1Encoder::default()))
}

#[derive(Default)]
pub struct NvencAv1Encoder {
    force_next_keyframe: bool,
    _session: Option<VideoSessionConfig>,
}

impl VideoEncoder for NvencAv1Encoder {
    fn configure_session(&mut self, config: VideoSessionConfig) -> Result<()> {
        self._session = Some(config);
        Ok(())
    }

    fn request_keyframe(&mut self) -> Result<()> {
        self.force_next_keyframe = true;
        Ok(())
    }

    fn update_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        if let Some(session) = self._session.as_mut() {
            session.target_bitrate_bps = bitrate_bps;
        }
        Ok(())
    }

    fn encode(&mut self, _frame: VideoFrame) -> Result<EncodedAccessUnit> {
        bail!("NVENC AV1 runtime encode is not available in this build")
    }

    fn backend_name(&self) -> &'static str {
        "av1-nvenc"
    }
}
