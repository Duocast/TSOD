use audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{
    Async as AsyncResampler, FixedAsync, Resampler as _, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

const DEFAULT_INPUT_FRAMES: usize = 960;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResamplerMode {
    Rubato,
    Linear,
}

impl ResamplerMode {
    pub fn from_env() -> Self {
        match std::env::var("VP_AUDIO_RESAMPLER") {
            Ok(mode) if mode.eq_ignore_ascii_case("linear") => Self::Linear,
            _ => Self::Rubato,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rubato => "rubato",
            Self::Linear => "linear",
        }
    }
}

pub(crate) enum ResamplerImpl {
    Rubato(RubatoResampler),
    Linear(LinearResampler),
}

impl ResamplerImpl {
    pub fn new(input_rate: u32, output_rate: u32, channels: usize, mode: ResamplerMode) -> Self {
        if input_rate == output_rate {
            return Self::Linear(LinearResampler::new(input_rate, output_rate));
        }

        match mode {
            ResamplerMode::Rubato => RubatoResampler::new(input_rate, output_rate, channels)
                .map(Self::Rubato)
                .unwrap_or_else(|err| {
                    tracing::warn!(
                        "[audio] failed to initialize rubato resampler ({} -> {}, ch={}): {}; using linear fallback",
                        input_rate,
                        output_rate,
                        channels,
                        err
                    );
                    Self::Linear(LinearResampler::new(input_rate, output_rate))
                }),
            ResamplerMode::Linear => Self::Linear(LinearResampler::new(input_rate, output_rate)),
        }
    }

    pub fn process_mono(&mut self, input: &[f32], out: &mut Vec<f32>) {
        match self {
            Self::Rubato(r) => r.process_mono(input, out),
            Self::Linear(r) => r.process(input, out),
        }
    }

    pub fn process_interleaved(&mut self, input: &[f32], channels: usize, out: &mut Vec<f32>) {
        match self {
            Self::Rubato(r) => r.process_interleaved(input, channels, out),
            Self::Linear(r) => process_linear_interleaved(r, input, channels, out),
        }
    }
}

struct RubatoResampler {
    inner: AsyncResampler<f32>,
    channels: usize,
    pending_interleaved: Vec<f32>,
    in_planar: Vec<Vec<f32>>,
    out_planar: Vec<Vec<f32>>,
    out_interleaved: Vec<f32>,
}

impl RubatoResampler {
    fn new(
        input_rate: u32,
        output_rate: u32,
        channels: usize,
    ) -> Result<Self, rubato::ResamplerConstructionError> {
        let ratio = output_rate as f64 / input_rate as f64;
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let chunk_size = DEFAULT_INPUT_FRAMES;
        let inner = AsyncResampler::<f32>::new_sinc(
            ratio,
            1.0,
            &params,
            chunk_size,
            channels,
            FixedAsync::Input,
        )?;
        let max_out = inner.output_frames_max();
        let mut in_planar = Vec::with_capacity(channels);
        let mut out_planar = Vec::with_capacity(channels);
        for _ in 0..channels {
            in_planar.push(vec![0.0; chunk_size]);
            out_planar.push(vec![0.0; max_out]);
        }

        Ok(Self {
            inner,
            channels,
            pending_interleaved: Vec::new(),
            in_planar,
            out_planar,
            out_interleaved: vec![0.0; max_out.saturating_mul(channels)],
        })
    }

    fn process_mono(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.process_interleaved(input, 1, out);
    }

    fn process_interleaved(&mut self, input: &[f32], channels: usize, out: &mut Vec<f32>) {
        if input.is_empty() || channels == 0 || channels != self.channels {
            return;
        }

        if input.len() < channels {
            return;
        }

        self.pending_interleaved.extend_from_slice(input);

        let required_in = self.inner.input_frames_next();
        let required_samples = required_in.saturating_mul(channels);

        while self.pending_interleaved.len() >= required_samples {
            for ch in 0..channels {
                self.in_planar[ch].resize(required_in, 0.0);
            }

            for frame_idx in 0..required_in {
                let src_base = frame_idx * channels;
                for ch in 0..channels {
                    self.in_planar[ch][frame_idx] = self.pending_interleaved[src_base + ch];
                }
            }

            let in_adapter = SequentialSliceOfVecs::new(&self.in_planar[..], channels, required_in)
                .expect("in_planar size mismatch");
            let out_frames_cap = self.out_planar[0].len();
            let mut out_adapter =
                SequentialSliceOfVecs::new_mut(&mut self.out_planar[..], channels, out_frames_cap)
                    .expect("out_planar size mismatch");
            match self
                .inner
                .process_into_buffer(&in_adapter, &mut out_adapter, None)
            {
                Ok((_in, out_frames)) => {
                    let needed = out_frames * channels;
                    if self.out_interleaved.len() < needed {
                        self.out_interleaved.resize(needed, 0.0);
                    }
                    for frame_idx in 0..out_frames {
                        let dst_base = frame_idx * channels;
                        for ch in 0..channels {
                            self.out_interleaved[dst_base + ch] = self.out_planar[ch][frame_idx];
                        }
                    }
                    out.extend_from_slice(&self.out_interleaved[..needed]);
                }
                Err(err) => {
                    tracing::warn!("[audio] rubato process failed: {err}");
                    return;
                }
            }

            self.pending_interleaved.drain(..required_samples);
        }
    }
}

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

fn process_linear_interleaved(
    linear: &mut LinearResampler,
    input: &[f32],
    channels: usize,
    out: &mut Vec<f32>,
) {
    if channels == 0 || input.is_empty() {
        return;
    }
    if channels == 1 {
        linear.process(input, out);
        return;
    }

    let frames = input.len() / channels;
    if frames == 0 {
        return;
    }

    let mut mono = Vec::with_capacity(frames);
    let mut mono_out = Vec::new();
    for ch in 0..channels {
        mono.clear();
        for frame in input.chunks_exact(channels) {
            mono.push(frame[ch]);
        }
        mono_out.clear();
        linear.process(&mono, &mut mono_out);
        if ch == 0 {
            out.resize(mono_out.len() * channels, 0.0);
        }
        for (idx, &sample) in mono_out.iter().enumerate() {
            out[idx * channels + ch] = sample;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ResamplerImpl, ResamplerMode};

    fn sine(frames: usize, rate: u32, freq: f32) -> Vec<f32> {
        (0..frames)
            .map(|i| ((i as f32 * freq * 2.0 * std::f32::consts::PI) / rate as f32).sin())
            .collect()
    }

    #[test]
    fn ratio_48k_to_44k1_close() {
        let mut r = ResamplerImpl::new(48_000, 44_100, 1, ResamplerMode::Rubato);
        let input = sine(4_800, 48_000, 440.0);
        let mut out = Vec::new();
        r.process_mono(&input, &mut out);
        let expected = (input.len() as f64 * 44_100f64 / 48_000f64) as isize;
        assert!((out.len() as isize - expected).abs() <= 4);
    }

    #[test]
    fn ratio_44k1_to_48k_close() {
        let mut r = ResamplerImpl::new(44_100, 48_000, 1, ResamplerMode::Rubato);
        let input = sine(4_410, 44_100, 440.0);
        let mut out = Vec::new();
        r.process_mono(&input, &mut out);
        let expected = (input.len() as f64 * 48_000f64 / 44_100f64) as isize;
        assert!((out.len() as isize - expected).abs() <= 4);
    }

    #[test]
    fn interleaved_stays_separated() {
        let mut r = ResamplerImpl::new(48_000, 44_100, 2, ResamplerMode::Rubato);
        let frames = 4_800;
        let mut input = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            input.push((i as f32 / 100.0).sin());
            input.push(0.0);
        }
        let mut out = Vec::new();
        r.process_interleaved(&input, 2, &mut out);
        let right_energy: f32 =
            out.chunks_exact(2).map(|f| f[1].abs()).sum::<f32>() / (out.len().max(2) / 2) as f32;
        assert!(right_energy < 1e-4);
    }

    #[test]
    fn sequential_calls_keep_state() {
        let mut r = ResamplerImpl::new(48_000, 44_100, 1, ResamplerMode::Rubato);
        let input = sine(9_600, 48_000, 220.0);

        let mut whole = Vec::new();
        r.process_mono(&input, &mut whole);

        let mut r2 = ResamplerImpl::new(48_000, 44_100, 1, ResamplerMode::Rubato);
        let mut chunked = Vec::new();
        for chunk in input.chunks(240) {
            r2.process_mono(chunk, &mut chunked);
        }

        assert_eq!(whole.len(), chunked.len());
        let max_delta = whole
            .iter()
            .zip(chunked.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_delta < 0.02);
    }

    #[test]
    fn rubato_handles_sub_chunk_inputs_without_error() {
        let mut r = ResamplerImpl::new(48_000, 44_100, 1, ResamplerMode::Rubato);
        let input = sine(1_120, 48_000, 220.0);

        let mut first = Vec::new();
        r.process_mono(&input[..560], &mut first);
        assert!(first.is_empty());

        let mut second = Vec::new();
        r.process_mono(&input[560..], &mut second);
        assert!(!second.is_empty());
    }
}
