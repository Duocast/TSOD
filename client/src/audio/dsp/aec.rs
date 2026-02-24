//! Acoustic Echo Cancellation (AEC) using `sonora-aec3`.
//!
//! This wraps the crate with a small API that fits the existing capture/playout
//! pipeline: feed speaker/reference audio, then process microphone/capture audio.

use anyhow::{Context, Result};
use sonora_aec3::Aec as SonoraAec;

const TEN_MS_AT_48K: usize = 480;

/// AEC3-based acoustic echo canceller.
pub struct Aec {
    inner: SonoraAec,
    render_buf: Vec<i16>,
    capture_buf: Vec<i16>,
}

impl Aec {
    /// Create a new AEC instance for 48kHz mono audio.
    pub fn new(sample_rate: u32) -> Result<Self> {
        anyhow::ensure!(sample_rate == 48_000, "AEC requires 48kHz audio");

        let inner =
            SonoraAec::new(sample_rate as usize, 1).context("create sonora-aec3 processor")?;

        Ok(Self {
            inner,
            render_buf: Vec::with_capacity(TEN_MS_AT_48K),
            capture_buf: Vec::with_capacity(TEN_MS_AT_48K),
        })
    }

    /// Feed reference/playout samples to the AEC.
    pub fn feed_reference(&mut self, reference: &[i16]) {
        self.render_buf.extend_from_slice(reference);

        while self.render_buf.len() >= TEN_MS_AT_48K {
            let frame: Vec<f32> = self
                .render_buf
                .drain(..TEN_MS_AT_48K)
                .map(|s| s as f32 / 32768.0)
                .collect();
            // Render step should not be fatal; if it fails, we simply skip this frame.
            let _ = self.inner.analyze_render(&frame);
        }
    }

    /// Process capture/microphone samples in place.
    pub fn process(&mut self, capture: &mut [i16]) {
        self.capture_buf.extend_from_slice(capture);
        let mut out = Vec::with_capacity(self.capture_buf.len());

        while self.capture_buf.len() >= TEN_MS_AT_48K {
            let mut frame: Vec<f32> = self
                .capture_buf
                .drain(..TEN_MS_AT_48K)
                .map(|s| s as f32 / 32768.0)
                .collect();

            // If processing fails for a frame, we still forward converted audio.
            let _ = self.inner.process_capture(&mut frame);
            out.extend(frame.into_iter().map(|s| {
                (s * 32768.0)
                    .round()
                    .clamp(i16::MIN as f32, i16::MAX as f32) as i16
            }));
        }

        // Preserve tail samples that have not yet made a full 10ms frame by appending
        // the original trailing input unchanged.
        let tail = self.capture_buf.len();
        if tail > 0 {
            let start = capture.len().saturating_sub(tail);
            out.extend_from_slice(&capture[start..]);
        }

        if out.len() == capture.len() {
            capture.copy_from_slice(&out);
        }
    }
}
