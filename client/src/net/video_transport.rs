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
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use super::video_datagram::{self, VideoHeader, MAX_VIDEO_PAYLOAD, VIDEO_HDR_LEN};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoSendError {
    FrameTooLarge,
}

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
    max_frame_bytes: usize,
    max_frags_per_frame: usize,
    pacer: Pacer,
    force_recovery_next_frame: bool,
    last_recovery_request_at: Option<Instant>,
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
            pool: BufferPool::new(VIDEO_HDR_LEN + MAX_VIDEO_PAYLOAD, max_fragments_per_frame),
            max_frame_bytes: vp_voice::MAX_FRAGS_PER_FRAME as usize * MAX_VIDEO_PAYLOAD,
            max_frags_per_frame: vp_voice::MAX_FRAGS_PER_FRAME as usize,
            pacer: Pacer::new(
                PacingPolicy {
                    target_bps: 8_000_000,
                    max_burst_packets: 3,
                },
                vp_voice::MAX_VIDEO_DATAGRAM_BYTES,
            ),
            force_recovery_next_frame: false,
            last_recovery_request_at: None,
        }
    }

    pub fn request_recovery(&mut self, now: Instant, min_interval: Duration) -> bool {
        if self
            .last_recovery_request_at
            .map(|prev| now.duration_since(prev) < min_interval)
            .unwrap_or(false)
        {
            return false;
        }
        self.last_recovery_request_at = Some(now);
        self.force_recovery_next_frame = true;
        true
    }

    pub fn mark_next_frame_recovery(&mut self) {
        self.force_recovery_next_frame = true;
    }

    /// Set per-stream sender pacing.
    pub fn set_pacing_policy(&mut self, target_bps: u64, max_burst_packets: usize) {
        self.pacer.set_policy(
            PacingPolicy {
                target_bps,
                max_burst_packets,
            },
            vp_voice::MAX_VIDEO_DATAGRAM_BYTES,
        );
    }

    #[inline]
    fn on_frame_too_large(&mut self) {
        // Hook point for adaptive downshift behavior (e.g. fps / scale reduction).
        self.mark_next_frame_recovery();
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
    ) -> Result<(), VideoSendError>
    where
        F: FnMut(Bytes),
    {
        if encoded_frame.len() > self.max_frame_bytes {
            self.on_frame_too_large();
            return Err(VideoSendError::FrameTooLarge);
        }

        let frame_seq = self.frame_seq;
        self.frame_seq = self.frame_seq.wrapping_add(1);

        let max_payload = MAX_VIDEO_PAYLOAD;
        let frag_total = if encoded_frame.is_empty() {
            1
        } else {
            (encoded_frame.len() + max_payload - 1) / max_payload
        };

        if frag_total > self.max_frags_per_frame {
            self.on_frame_too_large();
            return Err(VideoSendError::FrameTooLarge);
        }

        let frag_total = frag_total as u16;

        let mut flags = 0u8;
        if is_keyframe {
            flags |= vp_voice::VIDEO_FLAG_KEYFRAME;
        }
        if self.force_recovery_next_frame {
            flags |= vp_voice::VIDEO_FLAG_RECOVERY;
            self.force_recovery_next_frame = false;
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

            self.pacer.acquire((VIDEO_HDR_LEN + chunk.len()) as u64);
            let mut buf = self.pool.get();
            let datagram = video_datagram::write_video_datagram_into(&mut buf, &hdr, chunk);
            // Return the BytesMut shell to the pool (its backing storage was split off).
            self.pool.put(buf);
            emit(datagram);
        }

        Ok(())
    }

    /// Async variant of [`send_frame`] for tokio tasks.
    ///
    /// Uses `tokio::time::sleep` for pacing so runtime worker threads are never blocked.
    pub async fn send_frame_async<F>(
        &mut self,
        ts_ms: u32,
        is_keyframe: bool,
        encoded_frame: &[u8],
        mut emit: F,
    ) -> Result<(), VideoSendError>
    where
        F: FnMut(Bytes),
    {
        if encoded_frame.len() > self.max_frame_bytes {
            self.on_frame_too_large();
            return Err(VideoSendError::FrameTooLarge);
        }

        let frame_seq = self.frame_seq;
        self.frame_seq = self.frame_seq.wrapping_add(1);

        let max_payload = MAX_VIDEO_PAYLOAD;
        let frag_total = if encoded_frame.is_empty() {
            1
        } else {
            (encoded_frame.len() + max_payload - 1) / max_payload
        };

        if frag_total > self.max_frags_per_frame {
            self.on_frame_too_large();
            return Err(VideoSendError::FrameTooLarge);
        }

        let frag_total = frag_total as u16;

        let mut flags = 0u8;
        if is_keyframe {
            flags |= vp_voice::VIDEO_FLAG_KEYFRAME;
        }
        if self.force_recovery_next_frame {
            flags |= vp_voice::VIDEO_FLAG_RECOVERY;
            self.force_recovery_next_frame = false;
        }

        for (i, chunk) in encoded_frame.chunks(max_payload).enumerate().chain(
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

            self.pacer
                .acquire_async((VIDEO_HDR_LEN + chunk.len()) as u64)
                .await;
            let mut buf = self.pool.get();
            let datagram = video_datagram::write_video_datagram_into(&mut buf, &hdr, chunk);
            self.pool.put(buf);
            emit(datagram);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct PacingPolicy {
    target_bps: u64,
    max_burst_packets: usize,
}

#[derive(Debug, Clone)]
struct Pacer {
    policy: PacingPolicy,
    burst_bytes: u64,
    tokens: f64,
    last_refill: Instant,
}

impl Pacer {
    fn new(policy: PacingPolicy, mtu_bytes: usize) -> Self {
        let burst_bytes = (policy.max_burst_packets * mtu_bytes) as u64;
        Self {
            policy,
            burst_bytes,
            tokens: burst_bytes as f64,
            last_refill: Instant::now(),
        }
    }

    fn set_policy(&mut self, policy: PacingPolicy, mtu_bytes: usize) {
        self.policy = policy;
        self.burst_bytes = (policy.max_burst_packets * mtu_bytes) as u64;
        self.tokens = self.tokens.min(self.burst_bytes as f64);
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.last_refill = now;
        let add = elapsed * (self.policy.target_bps as f64 / 8.0);
        self.tokens = (self.tokens + add).min(self.burst_bytes as f64);
    }

    fn required_delay(&mut self, bytes: u64, now: Instant) -> Duration {
        self.refill(now);
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            Duration::ZERO
        } else {
            let deficit = bytes as f64 - self.tokens;
            self.tokens = 0.0;
            Duration::from_secs_f64(deficit / (self.policy.target_bps as f64 / 8.0))
        }
    }

    fn acquire(&mut self, bytes: u64) {
        let delay = self.required_delay(bytes, Instant::now());
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }

    async fn acquire_async(&mut self, bytes: u64) {
        let delay = self.required_delay(bytes, Instant::now());
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
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
        if frag_idx >= self.frag_total || frag_idx >= vp_voice::MAX_FRAGS_PER_FRAME {
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
    /// Concatenated payload for direct decode/render paths.
    pub payload: Bytes,
    /// Fragment payloads in order. Caller concatenates for decoding.
    pub fragments: Vec<Bytes>,
}

fn flatten_fragments(fragments: &[Bytes]) -> Bytes {
    let total = fragments.iter().map(Bytes::len).sum::<usize>();
    let mut out = BytesMut::with_capacity(total);
    for fragment in fragments {
        out.extend_from_slice(fragment);
    }
    out.freeze()
}

/// Bounded video frame reassembly cache.
///
/// Design:
/// - Fixed number of in-flight frame slots (e.g., 4).
/// - When a new frame arrives and cache is full, evict the oldest incomplete frame.
/// - Fragments are stored as `Bytes` (zero-copy slices of incoming datagrams).
pub struct VideoReceiver {
    slots: VecDeque<FrameSlot>,
    free_slots: Vec<FrameSlot>,
    max_slots: usize,
    #[cfg(test)]
    slot_allocations: usize,
}

impl VideoReceiver {
    pub fn new(max_in_flight_frames: usize) -> Self {
        Self {
            slots: VecDeque::with_capacity(max_in_flight_frames),
            free_slots: Vec::with_capacity(max_in_flight_frames),
            max_slots: max_in_flight_frames,
            #[cfg(test)]
            slot_allocations: 0,
        }
    }

    /// Process an incoming video datagram fragment.
    ///
    /// Returns `Some(ReassembledFrame)` if this fragment completed a frame.
    pub fn receive(&mut self, datagram: &Bytes) -> Option<ReassembledFrame> {
        let hdr = VideoHeader::parse(datagram)?;
        let payload = datagram.slice(VIDEO_HDR_LEN..);

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
                    let fragments: Vec<Bytes> =
                        slot.take_fragments().into_iter().flatten().collect();
                    let payload = flatten_fragments(&fragments);
                    let frame = ReassembledFrame {
                        stream_tag: key.stream_tag,
                        layer_id: key.layer_id,
                        frame_seq: key.frame_seq,
                        ts_ms: slot.ts_ms,
                        is_keyframe: slot.is_keyframe,
                        payload,
                        fragments,
                    };
                    slot.reset(key, hdr.frag_total, hdr.is_keyframe(), hdr.ts_ms);
                    self.free_slots.push(slot);
                    return Some(frame);
                }
                None
            }
            None => {
                // New frame. Evict oldest if full.
                if self.slots.len() >= self.max_slots {
                    if let Some(mut evicted) = self.slots.pop_front() {
                        evicted.reset(
                            evicted.key,
                            evicted.frag_total,
                            evicted.is_keyframe,
                            evicted.ts_ms,
                        );
                        self.free_slots.push(evicted);
                    }
                }
                let mut slot = if let Some(mut reused) = self.free_slots.pop() {
                    reused.reset(key, hdr.frag_total, hdr.is_keyframe(), hdr.ts_ms);
                    reused
                } else {
                    #[cfg(test)]
                    {
                        self.slot_allocations += 1;
                    }
                    FrameSlot::new(key, hdr.frag_total, hdr.is_keyframe(), hdr.ts_ms)
                };
                let complete = slot.insert(hdr.frag_idx, payload);
                if complete {
                    let fragments: Vec<Bytes> =
                        slot.take_fragments().into_iter().flatten().collect();
                    let payload = flatten_fragments(&fragments);
                    let frame = ReassembledFrame {
                        stream_tag: key.stream_tag,
                        layer_id: key.layer_id,
                        frame_seq: key.frame_seq,
                        ts_ms: slot.ts_ms,
                        is_keyframe: slot.is_keyframe,
                        payload,
                        fragments,
                    };
                    slot.reset(key, hdr.frag_total, hdr.is_keyframe(), hdr.ts_ms);
                    self.free_slots.push(slot);
                    return Some(frame);
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

    #[cfg(test)]
    fn free_slot_count(&self) -> usize {
        self.free_slots.len()
    }

    #[cfg(test)]
    fn slot_allocations(&self) -> usize {
        self.slot_allocations
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

        sender
            .send_frame(100, false, frame, |dg| datagrams.push(dg))
            .unwrap();

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

        sender
            .send_frame(200, true, &frame, |dg| datagrams.push(dg))
            .unwrap();

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
            sender
                .send_frame(0, false, b"x", |dg| {
                    let hdr = VideoHeader::parse(&dg).unwrap();
                    seqs.push(hdr.frame_seq);
                })
                .unwrap();
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

        sender
            .send_frame(500, true, &original, |dg| {
                if let Some(frame) = receiver.receive(&dg) {
                    completed = Some(frame);
                }
            })
            .unwrap();

        let frame = completed.expect("should have reassembled");
        assert_eq!(frame.stream_tag, 42);
        assert!(frame.is_keyframe);
        let reassembled: Vec<u8> = frame
            .fragments
            .iter()
            .flat_map(|b| b.iter().copied())
            .collect();
        assert_eq!(reassembled, original);
    }

    #[test]
    fn sender_rejects_too_many_fragments() {
        let mut sender = VideoSender::new(1, 0, 4);
        let frame = vec![0u8; MAX_VIDEO_PAYLOAD * (vp_voice::MAX_FRAGS_PER_FRAME as usize + 1)];
        let err = sender.send_frame(0, false, &frame, |_dg| {}).unwrap_err();
        assert_eq!(err, VideoSendError::FrameTooLarge);
    }

    #[test]
    fn oversized_frame_returns_frame_too_large() {
        let mut sender = VideoSender::new(1, 0, 4);
        let frame = vec![0u8; MAX_VIDEO_PAYLOAD * vp_voice::MAX_FRAGS_PER_FRAME as usize + 1];
        let err = sender.send_frame(0, false, &frame, |_dg| {}).unwrap_err();
        assert_eq!(err, VideoSendError::FrameTooLarge);
    }

    #[test]
    fn frame_at_boundary_fragments_to_max_frags() {
        let mut sender = VideoSender::new(1, 0, 4);
        let frame = vec![7u8; MAX_VIDEO_PAYLOAD * vp_voice::MAX_FRAGS_PER_FRAME as usize];
        let mut count = 0usize;
        sender
            .send_frame(0, false, &frame, |_dg| count += 1)
            .unwrap();
        assert_eq!(count, vp_voice::MAX_FRAGS_PER_FRAME as usize);
    }

    #[test]
    fn pacer_delays_when_burst_exceeded() {
        let mut pacer = Pacer::new(
            PacingPolicy {
                target_bps: 8_000,
                max_burst_packets: 1,
            },
            1200,
        );
        let now = Instant::now();
        assert_eq!(pacer.required_delay(1200, now), Duration::ZERO);
        let delay = pacer.required_delay(1200, now);
        assert!(delay >= Duration::from_millis(1000));
    }

    #[tokio::test]
    async fn sender_async_keyframe_is_paced_and_spaced() {
        let mut sender = VideoSender::new(1, 0, 8);
        sender.set_pacing_policy(8_000, 1);

        let mut emitted_at = Vec::new();
        let frame = vec![0x11u8; MAX_VIDEO_PAYLOAD * 3];
        sender
            .send_frame_async(0, true, &frame, |_dg| emitted_at.push(Instant::now()))
            .await
            .unwrap();

        assert_eq!(emitted_at.len(), 3);
        let spacing1 = emitted_at[1].duration_since(emitted_at[0]);
        let spacing2 = emitted_at[2].duration_since(emitted_at[1]);

        // At 8kbps and ~1200B payload, each packet should be delayed by ~1s.
        // Keep threshold relaxed for scheduler jitter; we only need bounded spacing.
        assert!(spacing1 >= Duration::from_millis(500));
        assert!(spacing2 >= Duration::from_millis(500));
    }

    #[tokio::test]
    async fn sender_async_rejects_too_many_fragments() {
        let mut sender = VideoSender::new(1, 0, 4);
        let frame = vec![0u8; MAX_VIDEO_PAYLOAD * (vp_voice::MAX_FRAGS_PER_FRAME as usize + 1)];
        let err = sender
            .send_frame_async(0, false, &frame, |_dg| {})
            .await
            .unwrap_err();
        assert_eq!(err, VideoSendError::FrameTooLarge);
    }

    #[test]
    fn recovery_rate_limit_works() {
        let mut sender = VideoSender::new(1, 0, 4);
        let now = Instant::now();
        assert!(sender.request_recovery(now, Duration::from_millis(500)));
        assert!(
            !sender.request_recovery(now + Duration::from_millis(100), Duration::from_millis(500))
        );
        assert!(
            sender.request_recovery(now + Duration::from_millis(600), Duration::from_millis(500))
        );
    }

    fn receiver_payload_is_zero_copy_slice() {
        let mut rx = VideoReceiver::new(4);
        let payload = b"hello-zero-copy";
        let dg = make_frag(1, 10, 0, 1, vp_voice::VIDEO_FLAG_END_OF_FRAME, payload);
        let frame = rx.receive(&dg).expect("frame");
        let frag = &frame.fragments[0];
        assert_eq!(&frag[..], payload);
        assert_eq!(frag.as_ptr(), unsafe { dg.as_ptr().add(VIDEO_HDR_LEN) });
    }

    #[test]
    fn receiver_reuses_slots_via_freelist() {
        let mut rx = VideoReceiver::new(2);
        for seq in 0..8u32 {
            let dg = make_frag(1, seq, 0, 1, vp_voice::VIDEO_FLAG_END_OF_FRAME, b"x");
            let _ = rx.receive(&dg).expect("complete frame");
        }
        assert!(rx.free_slot_count() > 0);
        assert_eq!(rx.slot_allocations(), 1);
    }
}
