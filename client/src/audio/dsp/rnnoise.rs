//! RNNoise-based noise suppression using the `nnnoiseless` crate.
//!
//! RNNoise processes 480-sample frames (10ms at 48kHz) of f32 audio.
//! It returns a VAD probability alongside the denoised output.

use nnnoiseless::{DenoiseState, RnnModel};

pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    last_vad: f32,
    /// Temporary buffer for f32 conversion (480 samples).
    f32_buf: Vec<f32>,
    /// Reused denoised output frame buffer (480 samples).
    output_buf: Vec<f32>,
}

impl Denoiser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a denoiser with an owned custom RNNoise model.
    #[allow(dead_code)]
    pub fn from_model(model: RnnModel) -> Self {
        Self {
            state: DenoiseState::from_model(model),
            last_vad: 0.0,
            f32_buf: vec![0.0; DenoiseState::FRAME_SIZE],
            output_buf: vec![0.0; DenoiseState::FRAME_SIZE],
        }
    }

    /// Create a denoiser with a borrowed custom RNNoise model.
    ///
    /// The model reference must be `'static` to match this denoiser's storage.
    #[allow(dead_code)]
    pub fn with_model(model: &'static RnnModel) -> Self {
        Self {
            state: DenoiseState::with_model(model),
            last_vad: 0.0,
            f32_buf: vec![0.0; DenoiseState::FRAME_SIZE],
            output_buf: vec![0.0; DenoiseState::FRAME_SIZE],
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
            vad = self
                .state
                .process_frame(&mut self.output_buf[..frame_size], &self.f32_buf);
            self.f32_buf[..frame_size].copy_from_slice(&self.output_buf[..frame_size]);

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

impl Default for Denoiser {
    fn default() -> Self {
        Self {
            state: DenoiseState::new(),
            last_vad: 0.0,
            f32_buf: vec![0.0; DenoiseState::FRAME_SIZE],
            output_buf: vec![0.0; DenoiseState::FRAME_SIZE],
        }
    }
}
