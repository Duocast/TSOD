//! Automatic Gain Control (AGC).
//!
//! Simple envelope-following AGC that adjusts gain to reach a target RMS level.
//! Uses a slow attack / fast release to avoid pumping artifacts.

pub struct Agc {
    target_rms: f32,    // target RMS amplitude (linear)
    gain: f32,          // current gain factor
    attack: f32,        // smoothing coefficient for increasing gain (slow)
    release: f32,       // smoothing coefficient for decreasing gain (fast)
    max_gain: f32,
    min_gain: f32,
}

impl Agc {
    /// Create a new AGC with target level in dBFS and smoothing factor.
    /// `target_db`: target RMS in dBFS (e.g., -18.0)
    /// `smoothing`: 0.0..1.0, higher = slower adaptation
    pub fn new(target_db: f32, smoothing: f32) -> Self {
        let target_rms = 32768.0 * 10.0f32.powf(target_db / 20.0);
        Self {
            target_rms,
            gain: 1.0,
            attack: smoothing.clamp(0.01, 0.99),
            release: (smoothing * 0.5).clamp(0.01, 0.99),
            max_gain: 40.0,  // +32 dB max boost
            min_gain: 0.1,   // -20 dB max cut
        }
    }

    /// Set the target level in dBFS.
    pub fn set_target(&mut self, target_db: f32) {
        self.target_rms = 32768.0 * 10.0f32.powf(target_db / 20.0);
    }

    /// Process a frame of i16 PCM in-place, applying gain adjustment.
    pub fn process(&mut self, pcm: &mut [i16]) {
        if pcm.is_empty() {
            return;
        }

        // Compute RMS of current frame
        let sum_sq: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
        let rms = (sum_sq / pcm.len() as f64).sqrt() as f32;

        // Don't adjust gain for silence (avoids division by zero and pumping)
        if rms > 10.0 {
            let desired_gain = self.target_rms / rms;
            let desired_gain = desired_gain.clamp(self.min_gain, self.max_gain);

            // Asymmetric smoothing: slow attack, fast release
            let alpha = if desired_gain > self.gain {
                self.attack
            } else {
                self.release
            };
            self.gain = self.gain * alpha + desired_gain * (1.0 - alpha);
        }

        // Apply gain
        for s in pcm.iter_mut() {
            let amplified = (*s as f32 * self.gain).round();
            *s = amplified.clamp(-32768.0, 32767.0) as i16;
        }
    }

    /// Current gain in dB.
    pub fn gain_db(&self) -> f32 {
        20.0 * self.gain.log10()
    }
}
