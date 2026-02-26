//! Acoustic Echo Cancellation (AEC) using `sonora-aec3`.

use anyhow::Result;
use sonora_aec3::block::Block;
use sonora_aec3::block_framer::BlockFramer;
use sonora_aec3::block_processor::BlockProcessor;
use sonora_aec3::config::EchoCanceller3Config;
use sonora_aec3::frame_blocker::FrameBlocker;
use std::collections::VecDeque;

const INTERNAL_SAMPLE_RATE: u32 = 16_000;
const SUBFRAME_SAMPLES_16K: usize = 80;
const SUBFRAME_SAMPLES_48K: usize = SUBFRAME_SAMPLES_16K * 3;

pub struct Aec {
    inner: BlockProcessor,
    render_in: VecDeque<i16>,
    capture_in: VecDeque<i16>,
    capture_out: VecDeque<i16>,
    render_blocker: FrameBlocker,
    capture_blocker: FrameBlocker,
    capture_framer: BlockFramer,
    capture_framer_seeded: bool,
    original_capture: Vec<i16>,
    subframe_16k: Vec<f32>,
    framed_subframe: Vec<Vec<Vec<f32>>>,
}

impl Aec {
    pub fn new(sample_rate: u32) -> Result<Self> {
        anyhow::ensure!(sample_rate == 48_000, "AEC requires 48kHz audio");

        let config = EchoCanceller3Config::default();
        let inner = BlockProcessor::new(&config, INTERNAL_SAMPLE_RATE as usize, 1, 1);

        Ok(Self {
            inner,
            render_in: VecDeque::with_capacity(6 * SUBFRAME_SAMPLES_48K),
            capture_in: VecDeque::with_capacity(6 * SUBFRAME_SAMPLES_48K),
            capture_out: VecDeque::with_capacity(6 * SUBFRAME_SAMPLES_48K),
            render_blocker: FrameBlocker::new(1, 1),
            capture_blocker: FrameBlocker::new(1, 1),
            capture_framer: BlockFramer::new(1, 1),
            capture_framer_seeded: false,
            original_capture: Vec::new(),
            subframe_16k: vec![0.0; SUBFRAME_SAMPLES_16K],
            framed_subframe: vec![vec![vec![0.0f32; SUBFRAME_SAMPLES_16K]; 1]; 1],
        })
    }

    pub fn feed_reference(&mut self, reference: &[i16]) {
        self.render_in.extend(reference.iter().copied());
        while self.render_in.len() >= SUBFRAME_SAMPLES_48K {
            Self::pop_and_downsample_48k_to_16k(&mut self.render_in, &mut self.subframe_16k);
            let mut block = Block::new(1, 1);
            let subframe_view = vec![vec![self.subframe_16k.as_slice()]];
            self.render_blocker
                .insert_sub_frame_and_extract_block(&subframe_view, &mut block);
            self.inner.buffer_render(&block);

            if self.render_blocker.is_block_available() {
                let mut extra = Block::new(1, 1);
                self.render_blocker.extract_block(&mut extra);
                self.inner.buffer_render(&extra);
            }
        }
    }

    pub fn process(&mut self, capture: &mut [i16]) {
        self.original_capture.clear();
        self.original_capture.extend_from_slice(capture);
        self.capture_in.extend(capture.iter().copied());

        while self.capture_in.len() >= SUBFRAME_SAMPLES_48K {
            Self::pop_and_downsample_48k_to_16k(&mut self.capture_in, &mut self.subframe_16k);
            let mut block = Block::new(1, 1);
            let subframe_view = vec![vec![self.subframe_16k.as_slice()]];
            self.capture_blocker
                .insert_sub_frame_and_extract_block(&subframe_view, &mut block);
            self.inner.process_capture(false, false, None, &mut block);
            self.push_capture_block(block);

            if self.capture_blocker.is_block_available() {
                let mut extra = Block::new(1, 1);
                self.capture_blocker.extract_block(&mut extra);
                self.inner.process_capture(false, false, None, &mut extra);
                self.push_capture_block(extra);
            }
        }

        for (idx, sample) in capture.iter_mut().enumerate() {
            *sample = self
                .capture_out
                .pop_front()
                .unwrap_or(self.original_capture[idx]);
        }
    }

    fn push_capture_block(&mut self, block: Block) {
        if !self.capture_framer_seeded {
            self.capture_framer.insert_block(&block);
            self.capture_framer_seeded = true;
            return;
        }

        self.capture_framer
            .insert_block_and_extract_sub_frame(&block, &mut self.framed_subframe);
        for &s in &self.framed_subframe[0][0] {
            let s16 = s.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            self.capture_out.push_back(s16);
            self.capture_out.push_back(s16);
            self.capture_out.push_back(s16);
        }
    }

    fn pop_and_downsample_48k_to_16k(queue: &mut VecDeque<i16>, out_buf: &mut [f32]) {
        for out in out_buf.iter_mut() {
            let a = queue.pop_front().unwrap_or_default() as i32;
            let b = queue.pop_front().unwrap_or_default() as i32;
            let c = queue.pop_front().unwrap_or_default() as i32;
            *out = ((a + b + c) as f32) / 3.0;
        }
    }
}
