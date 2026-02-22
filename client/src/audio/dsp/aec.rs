//! Acoustic Echo Cancellation (AEC) - feature-gated.
//!
//! Uses a Normalized Least Mean Squares (NLMS) adaptive filter to cancel
//! the echo of the playout signal from the capture signal.
//!
//! This is a simplified AEC suitable for basic echo cancellation. For
//! production use, consider integrating WebRTC's AEC3 via FFI.

/// NLMS-based Acoustic Echo Canceller.
pub struct Aec {
    /// Adaptive filter taps (length = filter_len).
    weights: Vec<f32>,
    /// Reference signal buffer (playout audio, ring buffer).
    ref_buf: Vec<f32>,
    /// Write position in ref_buf.
    ref_pos: usize,
    /// Step size (mu) for NLMS adaptation.
    mu: f32,
    /// Small constant to prevent division by zero.
    delta: f32,
}

impl Aec {
    /// Create a new AEC.
    /// `filter_len`: number of taps (e.g., 4800 = 100ms at 48kHz)
    /// `mu`: NLMS step size (0.0..1.0, typical 0.5)
    pub fn new(filter_len: usize, mu: f32) -> Self {
        Self {
            weights: vec![0.0; filter_len],
            ref_buf: vec![0.0; filter_len],
            ref_pos: 0,
            mu: mu.clamp(0.01, 1.0),
            delta: 1e-6,
        }
    }

    /// Feed reference (playout) audio to the AEC. Call this whenever
    /// audio is sent to the speaker so the AEC can model the echo path.
    pub fn feed_reference(&mut self, reference: &[i16]) {
        for &s in reference {
            self.ref_buf[self.ref_pos] = s as f32;
            self.ref_pos = (self.ref_pos + 1) % self.ref_buf.len();
        }
    }

    /// Process a capture frame in-place, removing estimated echo.
    pub fn process(&mut self, capture: &mut [i16]) {
        let filter_len = self.weights.len();

        for s in capture.iter_mut() {
            let mic = *s as f32;

            // Compute filter output (estimated echo)
            let mut echo_est = 0.0f32;
            let mut ref_power = 0.0f32;

            for k in 0..filter_len {
                let idx = (self.ref_pos + self.ref_buf.len() - 1 - k) % self.ref_buf.len();
                let r = self.ref_buf[idx];
                echo_est += self.weights[k] * r;
                ref_power += r * r;
            }

            // Error = mic - estimated echo
            let error = mic - echo_est;

            // NLMS weight update
            let norm = ref_power + self.delta;
            let step = self.mu * error / norm;

            for k in 0..filter_len {
                let idx = (self.ref_pos + self.ref_buf.len() - 1 - k) % self.ref_buf.len();
                self.weights[k] += step * self.ref_buf[idx];
            }

            *s = error.round().clamp(-32768.0, 32767.0) as i16;
        }
    }
}
