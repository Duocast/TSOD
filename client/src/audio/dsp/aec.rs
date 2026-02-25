//! Acoustic Echo Cancellation (AEC) using `sonora-aec3`.

use anyhow::Result;
use sonora_aec3::block_processor::BlockProcessor;
use sonora_aec3::config::EchoCanceller3Config;

const TEN_MS_AT_48K: usize = 480;

pub struct Aec {
    inner: BlockProcessor,
    render_buf: Vec<i16>,
    capture_buf: Vec<i16>,
}

impl Aec {
    pub fn new(sample_rate: u32) -> Result<Self> {
        anyhow::ensure!(sample_rate == 48_000, "AEC requires 48kHz audio");

        let config = EchoCanceller3Config::default();
        let inner = BlockProcessor::new(&config, sample_rate as usize, 1, 1);

        Ok(Self {
            inner,
            render_buf: Vec::with_capacity(TEN_MS_AT_48K),
            capture_buf: Vec::with_capacity(TEN_MS_AT_48K),
        })
    }

    pub fn feed_reference(&mut self, reference: &[i16]) {
        self.render_buf.extend_from_slice(reference);
        while self.render_buf.len() >= TEN_MS_AT_48K {
            let _frame: Vec<i16> = self.render_buf.drain(..TEN_MS_AT_48K).collect();
            // TODO: convert frame to Block and inner.buffer_render
        }
    }

    pub fn process(&mut self, capture: &mut [i16]) {
        self.capture_buf.extend_from_slice(capture);
        let mut out = Vec::with_capacity(self.capture_buf.len());

        while self.capture_buf.len() >= TEN_MS_AT_48K {
            let frame: Vec<i16> = self.capture_buf.drain(..TEN_MS_AT_48K).collect();
            // TODO: convert to Block, process, convert back
            out.extend(frame);
        }

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
