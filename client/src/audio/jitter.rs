use std::collections::BTreeMap;

pub struct JitterBuffer {
    max_frames: usize,
    expected_seq: u32,
    buf: BTreeMap<u32, Vec<u8>>,
}

impl JitterBuffer {
    pub fn new(max_frames: usize) -> Self {
        Self { max_frames, expected_seq: 0, buf: BTreeMap::new() }
    }

    pub fn push(&mut self, seq: u32, payload: Vec<u8>) {
        if self.buf.len() >= self.max_frames {
            // Drop farthest future to keep bounded
            if let Some((&last, _)) = self.buf.iter().next_back() {
                self.buf.remove(&last);
            }
        }
        self.buf.insert(seq, payload);
    }

    pub fn pop_ready(&mut self) -> Option<Vec<u8>> {
        if let Some(p) = self.buf.remove(&self.expected_seq) {
            self.expected_seq = self.expected_seq.wrapping_add(1);
            return Some(p);
        }
        None
    }

    pub fn set_expected(&mut self, seq: u32) {
        self.expected_seq = seq;
        self.buf.clear();
    }
}
