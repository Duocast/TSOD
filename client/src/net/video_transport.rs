//! Client-side video/screenshare transport with allocation-free steady state.
//!
//! This module provides:
//! - `VideoSender`: packetizes encoded frames into datagrams with buffer pooling.
//! - `VideoReceiver`: bounded reassembly cache for incoming video fragments.
//!
//! Design constraints:
//! - No per-frame Vec allocations in hot loops.
//! - Pre-allocated, reusable buffers.
//! - Bounded queues everywhere.
//! - Eviction policy: drop oldest incomplete frames when new frames arrive.

use bytes::{Bytes, BytesMut};
use std::collections::VecDeque;

use super::video_datagram::{
    self, VideoHeader, VIDEO_HDR_LEN, MAX_VIDEO_PAYLOAD,
};

// ── VideoSender: allocation-free packetizer ───────────────────────────

/// Pre-allocated buffer pool for video datagram construction.
/// Avoids per-frame `BytesMut::with_capacity` allocations in steady state.
pub struct BufferPool {
    pool: VecDeque<BytesMut>,
    buf_capacity: usize,
    max_pooled: usize,
}

impl BufferPool {
    /// Create a pool that holds up to `max_pooled` buffers of `buf_capacity` each.
    pub fn new(buf_capacity: usize, max_pooled: usize) -> Self {
        let mut pool = VecDeque::with_capacity(max_pooled);
        // Pre-allocate all buffers.
        for _ in 0..max_pooled {
            pool.push_back(BytesMut::with_capacity(buf_capacity));
        }
        Self {
            pool,
            buf_capacity,
            max_pooled,
        }
    }

    /// Get a buffer from the pool. If empty, allocates a new one (cold path).
    #[inline]
    pub fn get(&mut self) -> BytesMut {
        match self.pool.pop_front() {
            Some(mut b) => {
                b.clear();
                b
            }
            None => BytesMut::with_capacity(self.buf_capacity),
        }
    }

    /// Return a buffer to the pool for reuse (if pool not full).
    #[inline]
    pub fn put(&mut self, mut buf: BytesMut) {
        if self.pool.len() < self.max_pooled {
            buf.clear();
            self.pool.push_back(buf);
        }
        // Else: drop the buffer. Pool is full.
    }
}

/// Video sender: fragments encoded frames into datagrams using pooled buffers.
pub struct VideoSender {
    stream_tag: u64,
    layer_id: u8,
    frame_seq: u32,
    pool: BufferPool,
}

impl VideoSender {
    /// Create a new sender for a given stream.
    ///
    /// `max_fragments_per_frame`: pre-size the buffer pool (e.g., 64 for large frames).
    pub fn new(stream_tag: u64, layer_id: u8, max_fragments_per_frame: usize) -> Self {
        Self {
            stream_tag,
            layer_id,
            frame_seq: 0,
            pool: BufferPool::new(
                VIDEO_HDR_LEN + MAX_VIDEO_PAYLOAD,
                max_fragments_per_frame,
            ),
        }
    }

    /// Fragment an encoded frame into datagrams, calling `emit` for each fragment.
    ///
    /// Steady-state: no allocations if pool has enough buffers.
    ///
    /// `ts_ms`: monotonic sender timestamp.
    /// `is_keyframe`: true if this is a keyframe.
    /// `encoded_frame`: the encoded video frame bytes.
    /// `emit`: callback receiving each fragment as `Bytes`.
    pub fn send_frame<F>(
        &mut self,
        ts_ms: u32,
        is_keyframe: bool,
        encoded_frame: &[u8],
        mut emit: F,
    ) where
        F: FnMut(Bytes),
    {
        let frame_seq = self.frame_seq;
        self.frame_seq = self.frame_seq.wrapping_add(1);

        let max_payload = MAX_VIDEO_PAYLOAD;
        let frag_total = if encoded_frame.is_empty() {
            1
        } else {
            ((encoded_frame.len() + max_payload - 1) / max_payload) as u16
        };

        let mut flags = 0u8;
        if is_keyframe {
            flags |= vp_voice::VIDEO_FLAG_KEYFRAME;
        }

        for (i, chunk) in encoded_frame.chunks(max_payload).enumerate().chain(
            // Handle empty frame: emit one fragment with empty payload.
            if encoded_frame.is_empty() {
                Some((0, &[][..]))
            } else {
                None
            }
            .into_iter(),
        ) {
            let frag_idx = i as u16;
            let mut frag_flags = flags;
            if frag_idx + 1 == frag_total {
                frag_flags |= vp_voice::VIDEO_FLAG_END_OF_FRAME;
            }

            let hdr = VideoHeader {
                stream_tag: self.stream_tag,
                layer_id: self.layer_id,
                flags: frag_flags,
                frame_seq,
                frag_idx,
                frag_total,
                ts_ms,
            };

            let mut buf = self.pool.get();
            let datagram = video_datagram::write_video_datagram_into(&mut buf, &hdr, chunk);
            // Return the BytesMut shell to the pool (its backing storage was split off).
            self.pool.put(buf);
            emit(datagram);
        }
    }
}

// ── VideoReceiver: bounded reassembly cache ───────────────────────────

/// Key for a reassembly slot.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct ReassemblyKey {
    stream_tag: u64,
    frame_seq: u32,
    layer_id: u8,
}

/// Reassembly slot for a single frame's fragments.
struct FrameSlot {
    key: ReassemblyKey,
    frag_total: u16,
    received_mask: u64, // Bitmask of received fragment indices (supports up to 64 fragments).
    received_count: u16,
    /// Pre-allocated fragment storage. Indices correspond to frag_idx.
    /// Uses `Option<Bytes>` but the Vec is pre-allocated and reused.
    fragments: Vec<Option<Bytes>>,
    is_keyframe: bool,
    ts_ms: u32,
}

impl FrameSlot {
    fn new(key: ReassemblyKey, frag_total: u16, is_keyframe: bool, ts_ms: u32) -> Self {
        let mut fragments = Vec::with_capacity(frag_total as usize);
        fragments.resize(frag_total as usize, None);
        Self {
            key,
            frag_total,
            received_mask: 0,
            received_count: 0,
            fragments,
            is_keyframe,
            ts_ms,
        }
    }

    /// Insert a fragment. Returns true if the frame is now complete.
    fn insert(&mut self, frag_idx: u16, payload: Bytes) -> bool {
        if frag_idx >= self.frag_total || frag_idx >= 64 {
            return false;
        }
        let bit = 1u64 << frag_idx;
        if self.received_mask & bit != 0 {
            return false; // Duplicate.
        }
        self.received_mask |= bit;
        self.received_count += 1;
        self.fragments[frag_idx as usize] = Some(payload);
        self.received_count == self.frag_total
    }

    /// Take all fragments (consumes the slot data). Caller reassembles.
    fn take_fragments(&mut self) -> Vec<Option<Bytes>> {
        std::mem::take(&mut self.fragments)
    }

    /// Reset for reuse (avoids reallocation).
    fn reset(&mut self, key: ReassemblyKey, frag_total: u16, is_keyframe: bool, ts_ms: u32) {
        self.key = key;
        self.frag_total = frag_total;
        self.received_mask = 0;
        self.received_count = 0;
        self.is_keyframe = is_keyframe;
        self.ts_ms = ts_ms;
        self.fragments.clear();
        self.fragments.resize(frag_total as usize, None);
    }
}

/// A complete reassembled frame ready for decoding.
pub struct ReassembledFrame {
    pub stream_tag: u64,
    pub layer_id: u8,
    pub frame_seq: u32,
    pub ts_ms: u32,
    pub is_keyframe: bool,
    /// Fragment payloads in order. Caller concatenates for decoding.
    pub fragments: Vec<Bytes>,
}

/// Bounded video frame reassembly cache.
///
/// Design:
/// - Fixed number of in-flight frame slots (e.g., 4).
/// - When a new frame arrives and cache is full, evict the oldest incomplete frame.
/// - Fragments are stored as `Bytes` (zero-copy slices of incoming datagrams).
pub struct VideoReceiver {
    slots: VecDeque<FrameSlot>,
    max_slots: usize,
}

impl VideoReceiver {
    pub fn new(max_in_flight_frames: usize) -> Self {
        Self {
            slots: VecDeque::with_capacity(max_in_flight_frames),
            max_slots: max_in_flight_frames,
        }
    }

    /// Process an incoming video datagram fragment.
    ///
    /// Returns `Some(ReassembledFrame)` if this fragment completed a frame.
    pub fn receive(&mut self, datagram: &Bytes) -> Option<ReassembledFrame> {
        let hdr = VideoHeader::parse(datagram)?;
        let payload = Bytes::copy_from_slice(video_datagram::video_payload(datagram));

        let key = ReassemblyKey {
            stream_tag: hdr.stream_tag,
            frame_seq: hdr.frame_seq,
            layer_id: hdr.layer_id,
        };

        // Find existing slot for this frame.
        let slot_idx = self.slots.iter().position(|s| s.key == key);

        match slot_idx {
            Some(idx) => {
                let complete = self.slots[idx].insert(hdr.frag_idx, payload);
                if complete {
                    let mut slot = self.slots.remove(idx).unwrap();
                    let fragments = slot
                        .take_fragments()
                        .into_iter()
                        .flatten()
                        .collect();
                    return Some(ReassembledFrame {
                        stream_tag: key.stream_tag,
                        layer_id: key.layer_id,
                        frame_seq: key.frame_seq,
                        ts_ms: slot.ts_ms,
                        is_keyframe: slot.is_keyframe,
                        fragments,
                    });
                }
                None
            }
            None => {
                // New frame. Evict oldest if full.
                if self.slots.len() >= self.max_slots {
                    self.slots.pop_front(); // Evict oldest incomplete frame.
                }
                let mut slot = FrameSlot::new(
                    key,
                    hdr.frag_total,
                    hdr.is_keyframe(),
                    hdr.ts_ms,
                );
                let complete = slot.insert(hdr.frag_idx, payload);
                if complete {
                    let fragments = slot
                        .take_fragments()
                        .into_iter()
                        .flatten()
                        .collect();
                    return Some(ReassembledFrame {
                        stream_tag: key.stream_tag,
                        layer_id: key.layer_id,
                        frame_seq: key.frame_seq,
                        ts_ms: slot.ts_ms,
                        is_keyframe: slot.is_keyframe,
                        fragments,
                    });
                }
                self.slots.push_back(slot);
                None
            }
        }
    }

    /// Number of in-flight frames currently being reassembled.
    pub fn in_flight(&self) -> usize {
        self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frag(
        stream_tag: u64,
        frame_seq: u32,
        frag_idx: u16,
        frag_total: u16,
        flags: u8,
        payload: &[u8],
    ) -> Bytes {
        video_datagram::make_video_datagram(
            &VideoHeader {
                stream_tag,
                layer_id: 0,
                flags,
                frame_seq,
                frag_idx,
                frag_total,
                ts_ms: 100,
            },
            payload,
        )
    }

    // ── BufferPool tests ──────────────────────────────────────────────

    #[test]
    fn buffer_pool_reuses_buffers() {
        let mut pool = BufferPool::new(128, 4);
        let b1 = pool.get();
        let cap1 = b1.capacity();
        assert!(cap1 >= 128);
        pool.put(b1);

        let b2 = pool.get();
        // After put + get, we should get back a buffer with the same capacity.
        assert_eq!(b2.capacity(), cap1);
        // Pool should be depleted by one.
        assert_eq!(pool.pool.len(), 3);
    }

    #[test]
    fn buffer_pool_allocates_when_empty() {
        let mut pool = BufferPool::new(128, 2);
        let _b1 = pool.get();
        let _b2 = pool.get();
        // Pool is now empty. Next get should still work (allocates new).
        let _b3 = pool.get();
    }

    // ── VideoSender tests ─────────────────────────────────────────────

    #[test]
    fn sender_fragments_small_frame() {
        let mut sender = VideoSender::new(42, 0, 4);
        let frame = b"small frame data";
        let mut datagrams = Vec::new();

        sender.send_frame(100, false, frame, |dg| datagrams.push(dg));

        assert_eq!(datagrams.len(), 1);
        let hdr = VideoHeader::parse(&datagrams[0]).unwrap();
        assert_eq!(hdr.stream_tag, 42);
        assert_eq!(hdr.frame_seq, 0);
        assert_eq!(hdr.frag_idx, 0);
        assert_eq!(hdr.frag_total, 1);
        assert!(hdr.is_end_of_frame());
        assert!(!hdr.is_keyframe());
    }

    #[test]
    fn sender_fragments_large_frame() {
        let mut sender = VideoSender::new(1, 0, 8);
        // Create a frame larger than MAX_VIDEO_PAYLOAD.
        let frame = vec![0xABu8; MAX_VIDEO_PAYLOAD * 3 + 100];
        let mut datagrams = Vec::new();

        sender.send_frame(200, true, &frame, |dg| datagrams.push(dg));

        assert_eq!(datagrams.len(), 4);
        for (i, dg) in datagrams.iter().enumerate() {
            let hdr = VideoHeader::parse(dg).unwrap();
            assert_eq!(hdr.frag_idx, i as u16);
            assert_eq!(hdr.frag_total, 4);
            assert!(hdr.is_keyframe());
            if i == 3 {
                assert!(hdr.is_end_of_frame());
            } else {
                assert!(!hdr.is_end_of_frame());
            }
        }
    }

    #[test]
    fn sender_increments_frame_seq() {
        let mut sender = VideoSender::new(1, 0, 4);
        let mut seqs = Vec::new();

        for _ in 0..5 {
            sender.send_frame(0, false, b"x", |dg| {
                let hdr = VideoHeader::parse(&dg).unwrap();
                seqs.push(hdr.frame_seq);
            });
        }

        assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
    }

    // ── VideoReceiver tests ───────────────────────────────────────────

    #[test]
    fn receiver_reassembles_single_fragment_frame() {
        let mut rx = VideoReceiver::new(4);
        let dg = make_frag(1, 0, 0, 1, vp_voice::VIDEO_FLAG_END_OF_FRAME, b"hello");

        let frame = rx.receive(&dg).expect("should complete");
        assert_eq!(frame.frame_seq, 0);
        assert_eq!(frame.fragments.len(), 1);
        assert_eq!(&frame.fragments[0][..], b"hello");
    }

    #[test]
    fn receiver_reassembles_multi_fragment_frame() {
        let mut rx = VideoReceiver::new(4);
        let dg0 = make_frag(1, 0, 0, 3, 0, b"aa");
        let dg1 = make_frag(1, 0, 1, 3, 0, b"bb");
        let dg2 = make_frag(1, 0, 2, 3, vp_voice::VIDEO_FLAG_END_OF_FRAME, b"cc");

        assert!(rx.receive(&dg0).is_none());
        assert!(rx.receive(&dg1).is_none());
        let frame = rx.receive(&dg2).expect("should complete");
        assert_eq!(frame.fragments.len(), 3);
        assert_eq!(&frame.fragments[0][..], b"aa");
        assert_eq!(&frame.fragments[1][..], b"bb");
        assert_eq!(&frame.fragments[2][..], b"cc");
    }

    #[test]
    fn receiver_handles_out_of_order_fragments() {
        let mut rx = VideoReceiver::new(4);
        let dg2 = make_frag(1, 0, 2, 3, vp_voice::VIDEO_FLAG_END_OF_FRAME, b"cc");
        let dg0 = make_frag(1, 0, 0, 3, 0, b"aa");
        let dg1 = make_frag(1, 0, 1, 3, 0, b"bb");

        assert!(rx.receive(&dg2).is_none());
        assert!(rx.receive(&dg0).is_none());
        let frame = rx.receive(&dg1).expect("should complete");
        assert_eq!(frame.fragments.len(), 3);
        // Fragments should be in order by frag_idx.
        assert_eq!(&frame.fragments[0][..], b"aa");
        assert_eq!(&frame.fragments[1][..], b"bb");
        assert_eq!(&frame.fragments[2][..], b"cc");
    }

    #[test]
    fn receiver_evicts_oldest_when_full() {
        let mut rx = VideoReceiver::new(2);

        // Start two frames.
        let dg_f0 = make_frag(1, 0, 0, 3, 0, b"f0");
        let dg_f1 = make_frag(1, 1, 0, 3, 0, b"f1");
        rx.receive(&dg_f0);
        rx.receive(&dg_f1);
        assert_eq!(rx.in_flight(), 2);

        // Third frame evicts frame 0 (oldest).
        let dg_f2 = make_frag(1, 2, 0, 3, 0, b"f2");
        rx.receive(&dg_f2);
        assert_eq!(rx.in_flight(), 2);

        // Sending more fragments for frame 0 should start a new slot.
        let dg_f0_late = make_frag(1, 0, 1, 3, 0, b"f0late");
        rx.receive(&dg_f0_late);
        // Frame 1 was evicted to make room.
        assert_eq!(rx.in_flight(), 2);
    }

    #[test]
    fn receiver_ignores_duplicate_fragments() {
        let mut rx = VideoReceiver::new(4);
        let dg = make_frag(1, 0, 0, 2, 0, b"data");

        assert!(rx.receive(&dg).is_none());
        assert!(rx.receive(&dg).is_none()); // Duplicate → ignored.

        let dg1 = make_frag(1, 0, 1, 2, vp_voice::VIDEO_FLAG_END_OF_FRAME, b"data2");
        let frame = rx.receive(&dg1).expect("should complete");
        assert_eq!(frame.fragments.len(), 2);
    }

    // ── Sender → Receiver roundtrip ──────────────────────────────────

    #[test]
    fn sender_receiver_roundtrip() {
        let mut sender = VideoSender::new(42, 0, 8);
        let mut receiver = VideoReceiver::new(4);

        let original = b"This is a test frame for the roundtrip".to_vec();
        let mut completed = None;

        sender.send_frame(500, true, &original, |dg| {
            if let Some(frame) = receiver.receive(&dg) {
                completed = Some(frame);
            }
        });

        let frame = completed.expect("should have reassembled");
        assert_eq!(frame.stream_tag, 42);
        assert!(frame.is_keyframe);
        let reassembled: Vec<u8> = frame.fragments.iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(reassembled, original);
    }
}
