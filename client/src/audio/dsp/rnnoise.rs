//! RNNoise-based noise suppression using the `nnnoiseless` crate.
//!
//! RNNoise processes 480-sample frames (10ms at 48kHz) of f32 audio.
//! It returns a VAD probability alongside the denoised output.

use nnnoiseless::DenoiseState;

pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    last_vad: f32,
    /// Temporary buffer for f32 conversion (480 samples).
    f32_buf: Vec<f32>,
}

impl Denoiser {
    pub fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            last_vad: 0.0,
            f32_buf: vec![0.0; DenoiseState::FRAME_SIZE],
        }
    }

    /// Process a frame of i16 PCM in-place. The frame length must be
    /// a multiple of 480 (RNNoise frame size). Returns the VAD probability
    /// of the last sub-frame processed.
    pub fn process_frame(&mut self, pcm: &mut [i16]) -> f32 {
        let frame_size = DenoiseState::FRAME_SIZE; // 480
        let mut vad = 0.0f32;

        for chunk in pcm.chunks_mut(frame_size) {
            if chunk.len() < frame_size {
                break; // skip partial tail
            }

            // i16 → f32
            for (i, &s) in chunk.iter().enumerate() {
                self.f32_buf[i] = s as f32;
            }

            // Denoise in-place, get VAD
            let mut output = vec![0.0f32; frame_size];
            vad = self.state.process_frame(&mut output, &self.f32_buf);
            self.f32_buf.copy_from_slice(&output);

            // f32 → i16 (clamp)
            for (i, out) in self.f32_buf.iter().enumerate() {
                chunk[i] = out.round().clamp(-32768.0, 32767.0) as i16;
            }
        }

        self.last_vad = vad;
        vad
    }

    /// Last VAD probability from the most recent `process_frame` call.
    pub fn last_vad(&self) -> f32 {
        self.last_vad
    }
}
