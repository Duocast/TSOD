//! DSP pipeline: RNNoise (noise suppression + VAD), AGC, and optional AEC.
//!
//! Processing chain (capture path):
//!   Mic PCM → AGC (pre-amplify) → RNNoise (denoise + VAD) → [AEC if enabled] → output
//!
//! Processing chain (playout path):
//!   Network PCM → [Spatial mix if enabled] → AGC (normalize) → speaker

pub mod rnnoise;
pub mod agc;
pub mod vad;
#[cfg(feature = "aec")]
pub mod aec;

use anyhow::Result;

/// Full DSP pipeline for the capture (microphone) path.
pub struct CaptureDsp {
    agc: agc::Agc,
    denoiser: rnnoise::Denoiser,
    vad_threshold: f32,
}

impl CaptureDsp {
    /// Create a new capture DSP pipeline.
    /// `sample_rate` must be 48000 (RNNoise requirement).
    pub fn new(sample_rate: u32) -> Result<Self> {
        assert_eq!(sample_rate, 48000, "RNNoise requires 48kHz");
        Ok(Self {
            agc: agc::Agc::new(-18.0, 0.3),
            denoiser: rnnoise::Denoiser::new(),
            vad_threshold: 0.5,
        })
    }

    /// Process a frame of PCM samples in-place. Returns VAD probability (0.0..1.0).
    /// Frame must be exactly 480 samples (10ms at 48kHz) for RNNoise.
    /// For 20ms frames (960 samples), call twice with each half.
    pub fn process_frame(&mut self, pcm: &mut [i16]) -> f32 {
        // Pre-amplify with AGC
        self.agc.process(pcm);

        // Denoise and get VAD
        self.denoiser.process_frame(pcm)
    }

    /// Returns true if the last processed frame had voice activity.
    pub fn is_voice_active(&self) -> bool {
        self.denoiser.last_vad() >= self.vad_threshold
    }

    /// Set the VAD threshold (0.0 = always active, 1.0 = very strict).
    pub fn set_vad_threshold(&mut self, threshold: f32) {
        self.vad_threshold = threshold.clamp(0.0, 1.0);
    }

    /// Set the AGC target level in dBFS (e.g., -18.0).
    pub fn set_agc_target(&mut self, target_db: f32) {
        self.agc.set_target(target_db);
    }

    pub fn last_vad_probability(&self) -> f32 {
        self.denoiser.last_vad()
    }
}

/// DSP pipeline for the playout (speaker) path.
pub struct PlayoutDsp {
    agc: agc::Agc,
}

impl PlayoutDsp {
    pub fn new() -> Self {
        Self {
            agc: agc::Agc::new(-14.0, 0.2),
        }
    }

    /// Normalize playout volume.
    pub fn process_frame(&mut self, pcm: &mut [i16]) {
        self.agc.process(pcm);
    }
}
