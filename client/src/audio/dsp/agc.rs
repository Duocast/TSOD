//! Automatic Gain Control (AGC).
//!
//! Simple envelope-following AGC that adjusts gain to reach a target RMS level.
//! Uses a slow attack / fast release to avoid pumping artifacts.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AgcPreset {
    Conservative,
    Balanced,
    Boosted,
}

impl AgcPreset {
    pub const ALL: [AgcPreset; 3] = [
        AgcPreset::Conservative,
        AgcPreset::Balanced,
        AgcPreset::Boosted,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AgcPreset::Conservative => "Conservative",
            AgcPreset::Balanced => "Balanced",
            AgcPreset::Boosted => "Boosted",
        }
    }

    pub fn target_db(self) -> f32 {
        self.profile().target_db
    }

    fn profile(self) -> AgcProfile {
        match self {
            AgcPreset::Conservative => AgcProfile::new(-20.0, 0.92, 0.75),
            AgcPreset::Balanced => AgcProfile::new(-18.0, 0.84, 0.65),
            AgcPreset::Boosted => AgcProfile::new(-14.0, 0.72, 0.55),
        }
    }
}

impl Default for AgcPreset {
    fn default() -> Self {
        Self::Balanced
    }
}

#[derive(Debug, Clone, Copy)]
struct AgcProfile {
    target_db: f32,
    attack: f32,
    release: f32,
}

impl AgcProfile {
    const fn new(target_db: f32, attack: f32, release: f32) -> Self {
        Self {
            target_db,
            attack,
            release,
        }
    }
}

pub struct Agc {
    target_rms: f32, // target RMS amplitude (linear)
    gain: f32,       // current gain factor
    attack: f32,     // smoothing coefficient for increasing gain (slow)
    release: f32,    // smoothing coefficient for decreasing gain (fast)
    max_gain: f32,
    min_gain: f32,
    noise_floor_rms: f32,
    noisy_max_gain: f32,
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
            max_gain: 40.0, // +32 dB max boost
            min_gain: 0.1,  // -20 dB max cut
            noise_floor_rms: 10.0,
            noisy_max_gain: 8.0, // +18 dB ceiling when background noise dominates
        }
    }

    pub fn with_preset(preset: AgcPreset) -> Self {
        let profile = preset.profile();
        let mut agc = Self::new(profile.target_db, 0.3);
        agc.attack = profile.attack;
        agc.release = profile.release;
        agc
    }

    pub fn set_preset(&mut self, preset: AgcPreset) {
        let profile = preset.profile();
        self.set_target(profile.target_db);
        self.attack = profile.attack;
        self.release = profile.release;
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

        self.update_noise_floor(rms);

        // Don't adjust gain for silence (avoids division by zero and pumping)
        if rms > 10.0 {
            let desired_gain = self.target_rms / rms;
            let adaptive_max_gain = self.adaptive_max_gain(rms);
            let desired_gain = desired_gain.clamp(self.min_gain, adaptive_max_gain);

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

    fn update_noise_floor(&mut self, rms: f32) {
        // Track a low-percentile envelope: rises slowly, falls very slowly.
        let alpha = if rms < self.noise_floor_rms {
            0.995
        } else {
            0.999
        };
        self.noise_floor_rms = self.noise_floor_rms * alpha + rms * (1.0 - alpha);
    }

    fn adaptive_max_gain(&self, frame_rms: f32) -> f32 {
        // When frame energy sits near the estimated noise floor, cap gain to avoid
        // amplifying denoiser residual artifacts.
        let noise_ratio = frame_rms / (self.noise_floor_rms + 1.0);
        if noise_ratio <= 1.6 {
            self.noisy_max_gain.min(self.max_gain)
        } else {
            self.max_gain
        }
    }
}
