//! Voice Activity Detection (VAD) utilities.
//!
//! The primary VAD comes from RNNoise (neural network based).
//! This module provides additional energy-based VAD as a fallback
//! and a hysteresis wrapper to avoid rapid on/off switching.

/// Hysteresis wrapper around a VAD probability source.
/// Requires the probability to exceed `on_threshold` to activate,
/// and drop below `off_threshold` to deactivate. This prevents
/// rapid toggling at the boundary.
pub struct VadHysteresis {
    on_threshold: f32,
    off_threshold: f32,
    active: bool,
    attack_frames: u32,
    voiced_run_frames: u32,
    /// Number of frames to hold active state after dropping below off_threshold.
    hangover_frames: u32,
    hangover_counter: u32,
}

impl VadHysteresis {
    pub fn new(
        on_threshold: f32,
        off_threshold: f32,
        attack_frames: u32,
        hangover_frames: u32,
    ) -> Self {
        Self {
            on_threshold,
            off_threshold,
            active: false,
            attack_frames,
            voiced_run_frames: 0,
            hangover_frames,
            hangover_counter: 0,
        }
    }

    pub fn from_timing(
        on_threshold: f32,
        off_threshold: f32,
        attack_ms: u32,
        hangover_ms: u32,
        frame_ms: u32,
    ) -> Self {
        let frame_ms = frame_ms.max(1);
        let attack_frames = (attack_ms / frame_ms).max(1);
        let hangover_frames = (hangover_ms / frame_ms).max(1);
        Self::new(on_threshold, off_threshold, attack_frames, hangover_frames)
    }

    /// Update with a new VAD probability. Returns whether voice is active.
    pub fn update(&mut self, probability: f32) -> bool {
        if probability >= self.on_threshold {
            self.voiced_run_frames = self.voiced_run_frames.saturating_add(1);
            if self.voiced_run_frames >= self.attack_frames {
                self.active = true;
            }
            self.hangover_counter = 0;
        } else if probability < self.off_threshold {
            self.voiced_run_frames = 0;
            if self.active {
                self.hangover_counter += 1;
                if self.hangover_counter >= self.hangover_frames {
                    self.active = false;
                    self.hangover_counter = 0;
                }
            }
        } else {
            // In the hysteresis band: maintain current state
            self.voiced_run_frames = 0;
            self.hangover_counter = 0;
        }

        self.active
    }

    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        self.active
    }
}

/// Simple energy-based VAD as a fallback when RNNoise is not available.
pub fn energy_vad(pcm: &[i16], threshold_db: f32) -> bool {
    if pcm.is_empty() {
        return false;
    }
    let sum_sq: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum_sq / pcm.len() as f64).sqrt();
    let db = if rms > 0.0 {
        20.0 * (rms / 32768.0).log10() as f32
    } else {
        -96.0
    };
    db > threshold_db
}
