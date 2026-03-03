//! DSP pipeline: RNNoise (noise suppression + VAD), AGC, and optional AEC.
//!
//! Processing chain (capture path):
//!   Mic PCM → AGC (pre-amplify) → [AEC if enabled] → RNNoise (denoise + VAD) → output
//!
//! Processing chain (playout path):
//!   Network PCM → [Spatial mix if enabled] → AGC (normalize) → speaker

#[cfg(feature = "aec")]
pub mod aec;
pub mod agc;
pub mod rnnoise;
pub mod vad;

use anyhow::Result;

/// Full DSP pipeline for the capture (microphone) path.
pub struct CaptureDsp {
    agc: agc::Agc,
    denoiser: rnnoise::Denoiser,
    vad_threshold: f32,
    noise_suppression_enabled: bool,
    agc_enabled: bool,
    #[cfg(feature = "aec")]
    aec: Option<aec::Aec>,
    echo_cancellation_enabled: bool,
    echo_ref_scratch: Vec<i16>,
}

impl CaptureDsp {
    /// Create a new capture DSP pipeline.
    /// `sample_rate` must be 48000 (RNNoise requirement).
    pub fn new(sample_rate: u32) -> Result<Self> {
        anyhow::ensure!(
            sample_rate == 48_000,
            "RNNoise requires 48kHz, got {sample_rate}"
        );
        Ok(Self {
            agc: agc::Agc::with_preset(agc::AgcPreset::Balanced),
            denoiser: rnnoise::Denoiser::new(),
            vad_threshold: 0.5,
            noise_suppression_enabled: true,
            agc_enabled: true,
            #[cfg(feature = "aec")]
            aec: Some(aec::Aec::new(sample_rate)?),
            echo_cancellation_enabled: false,
            echo_ref_scratch: Vec::with_capacity(960),
        })
    }

    /// Process a frame of PCM samples in-place. Returns VAD probability (0.0..1.0).
    /// Frame must be exactly 480 samples (10ms at 48kHz) for RNNoise.
    /// For 20ms frames (960 samples), call twice with each half.
    pub fn process_frame(&mut self, pcm: &mut [i16]) -> f32 {
        // Pre-amplify with AGC
        if self.agc_enabled {
            self.agc.process(pcm);
        }

        #[cfg(feature = "aec")]
        if self.echo_cancellation_enabled {
            if let Some(aec) = self.aec.as_mut() {
                aec.process(pcm);
            }
        }

        // Denoise and get VAD
        if self.noise_suppression_enabled {
            self.denoiser.process_frame(pcm)
        } else {
            // Still run VAD for level reporting even if denoiser is off
            0.0
        }
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

    pub fn set_agc_preset(&mut self, preset: agc::AgcPreset) {
        self.agc.set_preset(preset);
    }

    pub fn last_vad_probability(&self) -> f32 {
        self.denoiser.last_vad()
    }

    /// Enable or disable noise suppression (RNNoise).
    pub fn set_noise_suppression(&mut self, enabled: bool) {
        self.noise_suppression_enabled = enabled;
    }

    /// Enable or disable automatic gain control.
    pub fn set_agc(&mut self, enabled: bool) {
        self.agc_enabled = enabled;
    }

    /// Enable or disable acoustic echo cancellation.
    pub fn set_echo_cancellation(&mut self, enabled: bool) {
        self.echo_cancellation_enabled = enabled;
    }

    /// Feed playout/reference audio to the echo canceller.
    pub fn feed_echo_reference(&mut self, pcm: &[i16]) {
        #[cfg(feature = "aec")]
        if self.echo_cancellation_enabled {
            if let Some(aec) = self.aec.as_mut() {
                self.echo_ref_scratch.clear();
                self.echo_ref_scratch.extend_from_slice(pcm);
                aec.feed_reference(&self.echo_ref_scratch);
            }
        }
    }
}

/// DSP pipeline for the playout (speaker) path.
pub struct PlayoutDsp {
    agc: agc::Agc,
    frame_scratch: Vec<i16>,
}

impl PlayoutDsp {
    pub fn new() -> Self {
        Self {
            agc: agc::Agc::with_preset(agc::AgcPreset::Balanced),
            frame_scratch: Vec::with_capacity(960),
        }
    }

    /// Normalize playout volume.
    pub fn process_frame(&mut self, pcm: &mut [i16]) {
        self.frame_scratch.clear();
        self.frame_scratch.extend_from_slice(pcm);
        self.agc.process(&mut self.frame_scratch);
        pcm.copy_from_slice(&self.frame_scratch);
    }
}
