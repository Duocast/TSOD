use std::collections::BTreeMap;

pub enum PopResult {
    Frame(Vec<u8>),
    Missing,
    Waiting,
}

pub struct JitterBuffer {
    max_frames: usize,
    expected_seq: u32,
    expected_wait_started_ms: Option<u64>,
    buf: BTreeMap<u32, Vec<u8>>,
}

impl JitterBuffer {
    pub fn new(max_frames: usize) -> Self {
        Self {
            max_frames,
            expected_seq: 0,
            expected_wait_started_ms: None,
            buf: BTreeMap::new(),
        }
    }

    pub fn push(&mut self, seq: u32, payload: Vec<u8>) {
        if self.buf.is_empty() {
            self.expected_seq = seq;
            self.expected_wait_started_ms = None;
        }

        if self.buf.len() >= self.max_frames {
            // Drop farthest future to keep bounded
            if let Some((&last, _)) = self.buf.iter().next_back() {
                self.buf.remove(&last);
            }
        }
        self.buf.insert(seq, payload);
    }

    pub fn pop_ready(&mut self, now_ms: u64, max_wait_ms: u64) -> PopResult {
        if let Some(p) = self.buf.remove(&self.expected_seq) {
            self.expected_seq = self.expected_seq.wrapping_add(1);
            self.expected_wait_started_ms = None;
            return PopResult::Frame(p);
        }

        if let Some((&min_seq, _)) = self.buf.iter().next() {
            let timed_out = match self.expected_wait_started_ms {
                Some(start_ms) => now_ms.saturating_sub(start_ms) >= max_wait_ms,
                None => {
                    self.expected_wait_started_ms = Some(now_ms);
                    false
                }
            };

            if timed_out || min_seq != self.expected_seq {
                self.expected_seq = self.expected_seq.wrapping_add(1);
                self.expected_wait_started_ms = Some(now_ms);
                return PopResult::Missing;
            }
        }

        PopResult::Waiting
    }

    pub fn set_expected(&mut self, seq: u32) {
        self.expected_seq = seq;
        self.expected_wait_started_ms = None;
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{JitterBuffer, PopResult};

    #[test]
    fn initializes_expected_seq_from_first_packet() {
        let mut jitter = JitterBuffer::new(4);
        jitter.push(42, vec![1, 2, 3]);

        match jitter.pop_ready(1_000, 40) {
            PopResult::Frame(p) => assert_eq!(p, vec![1, 2, 3]),
            _ => panic!("expected frame"),
        }
    }

    #[test]
    fn skips_missing_seq_when_future_packet_exists() {
        let mut jitter = JitterBuffer::new(4);
        jitter.set_expected(10);
        jitter.push(11, vec![4]);

        assert!(matches!(jitter.pop_ready(1_000, 40), PopResult::Missing));
        assert!(matches!(jitter.pop_ready(1_020, 40), PopResult::Frame(_)));
    }

    #[test]
    fn skips_after_wait_timeout() {
        let mut jitter = JitterBuffer::new(4);
        jitter.set_expected(10);
        jitter.push(10, vec![1]);
        jitter.push(12, vec![2]);

        assert!(matches!(jitter.pop_ready(1_000, 40), PopResult::Frame(_)));
        assert!(matches!(jitter.pop_ready(1_010, 40), PopResult::Waiting));
        assert!(matches!(jitter.pop_ready(1_051, 40), PopResult::Missing));
        assert!(matches!(jitter.pop_ready(1_052, 40), PopResult::Frame(_)));
    }
}
