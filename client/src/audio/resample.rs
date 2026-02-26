/// Phase-tracking linear interpolation resampler for mono audio.
///
/// Converts between arbitrary sample rates using first-order (linear)
/// interpolation. Not audiophile-grade, but sufficient for real-time voice
/// where the device rate differs from the codec rate (e.g. 44 100 ↔ 48 000).
pub(crate) struct LinearResampler {
    step: f64,
    phase: f64,
    history: Vec<f32>,
}

impl LinearResampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            step: input_rate as f64 / output_rate as f64,
            phase: 0.0,
            history: Vec::new(),
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }

        self.history.extend_from_slice(input);
        while self.phase + 1.0 < self.history.len() as f64 {
            let i0 = self.phase.floor() as usize;
            let i1 = i0 + 1;
            let frac = (self.phase - i0 as f64) as f32;
            let s0 = self.history[i0];
            let s1 = self.history[i1];
            out.push(s0 + (s1 - s0) * frac);
            self.phase += self.step;
        }

        let consumed = self.phase.floor() as usize;
        if consumed > 0 {
            self.history.drain(..consumed);
            self.phase -= consumed as f64;
        }
    }
}
