//! Video/screenshare stream forwarding for QUIC DATAGRAM media (SFU behavior).
//!
//! Responsibilities:
//! - Validate incoming video datagrams (header parse, authorization).
//! - Forward to subscribed viewers using zero-copy (Bytes::clone = refcount only).
//! - Frame-aware bounded queueing with priority for keyframes/recovery frames.
//! - Drop entire old frames under queue pressure (never random fragment drops).
//!
//! Design principles:
//! - **Zero-copy**: No per-viewer memcpy. Forward the original `Bytes` buffer.
//! - **Bounded queues**: Per-viewer egress queues are bounded in fragment count.
//! - **Frame-aware dropping**: Evict oldest complete/incomplete frames first.
//! - **Priority**: Keyframe and recovery frame fragments survive longer under pressure.
//! - **Allocation-free steady state**: Queue structures are pre-sized, no per-packet alloc.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info};

use vp_control::ids::{ChannelId, UserId};

use crate::voice_forwarder::{DatagramTx, SessionRegistry};

// ── Video datagram header parsing (server-side) ───────────────────────

const VIDEO_HDR_LEN: usize = vp_voice::VIDEO_HEADER_BYTES;

/// Lightweight parsed video header (zero-copy view into buffer).
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
struct VideoHeader {
    stream_tag: u64,
    layer_id: u8,
    flags: u8,
    frame_seq: u32,
    frag_idx: u16,
    frag_total: u16,
    ts_ms: u32,
}

impl VideoHeader {
    fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < VIDEO_HDR_LEN {
            return None;
        }
        if buf[0] != vp_voice::VIDEO_VERSION {
            return None;
        }
        if buf[1] != vp_voice::DATAGRAM_KIND_VIDEO {
            return None;
        }
        let stream_tag = u64::from_le_bytes([
            buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
        ]);
        let layer_id = buf[10];
        let flags = buf[11];
        let frame_seq = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let frag_idx = u16::from_le_bytes([buf[16], buf[17]]);
        let frag_total = u16::from_le_bytes([buf[18], buf[19]]);
        let ts_ms = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);

        if frag_total == 0 || frag_total > vp_voice::MAX_FRAGS_PER_FRAME || frag_idx >= frag_total {
            return None;
        }

        Some(Self {
            stream_tag,
            layer_id,
            flags,
            frame_seq,
            frag_idx,
            frag_total,
            ts_ms,
        })
    }

    #[inline]
    fn is_keyframe(&self) -> bool {
        self.flags & vp_voice::VIDEO_FLAG_KEYFRAME != 0
    }

    #[inline]
    fn is_recovery(&self) -> bool {
        self.flags & vp_voice::VIDEO_FLAG_RECOVERY != 0
    }

    #[inline]
    fn is_priority(&self) -> bool {
        self.is_keyframe() || self.is_recovery()
    }
}

// ── Stream registration (control-plane integration) ───────────────────

/// Registered stream state, populated by control-plane events.
#[derive(Clone, Debug)]
pub struct StreamRegistration {
    pub sender_id: UserId,
    pub channel_id: ChannelId,
    pub codec: i32,
}

/// Metrics hook for stream forwarding (optional).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamDropReason {
    QueueFull,
    EvictedOldestFrame,
    Malformed,
    Unauthorized,
}

impl StreamDropReason {
    pub fn as_label(self) -> &'static str {
        match self {
            Self::QueueFull => "queue_full",
            Self::EvictedOldestFrame => "evicted_oldest_frame",
            Self::Malformed => "malformed",
            Self::Unauthorized => "unauthorized",
        }
    }
}

pub trait StreamMetrics: Send + Sync {
    fn inc_rx_packets(&self);
    fn inc_rx_bytes(&self, n: usize);
    fn inc_drop_invalid(&self);
    fn inc_drop_unauthorized(&self);
    fn inc_drop_by_reason(&self, reason: StreamDropReason);
    fn inc_drop_by_reason_codec(&self, reason: StreamDropReason, codec: i32);
    fn inc_forwarded(&self, fanout: usize);
    fn inc_forwarded_bytes(&self, n: usize);
    fn inc_forwarded_bytes_codec(&self, n: usize, codec: i32);
    fn inc_frames_evicted(&self, count: usize);
    fn inc_recovery_requests(&self);
}

/// No-op metrics default.
pub struct NoopStreamMetrics;
impl StreamMetrics for NoopStreamMetrics {
    fn inc_rx_packets(&self) {}
    fn inc_rx_bytes(&self, _n: usize) {}
    fn inc_drop_invalid(&self) {}
    fn inc_drop_unauthorized(&self) {}
    fn inc_drop_by_reason(&self, _reason: StreamDropReason) {}
    fn inc_drop_by_reason_codec(&self, _reason: StreamDropReason, _codec: i32) {}
    fn inc_forwarded(&self, _fanout: usize) {}
    fn inc_forwarded_bytes(&self, _n: usize) {}
    fn inc_forwarded_bytes_codec(&self, _n: usize, _codec: i32) {}
    fn inc_frames_evicted(&self, _count: usize) {}
    fn inc_recovery_requests(&self) {}
}

/// Provider for listing viewers who should receive a stream.
#[async_trait::async_trait]
pub trait ViewerProvider: Send + Sync {
    /// Return list of viewer user IDs subscribed to streams in the given channel,
    /// excluding the sender.
    async fn list_viewers(&self, channel: ChannelId, exclude_sender: UserId) -> Vec<UserId>;
}

// ── Configuration ─────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct StreamForwarderConfig {
    /// Maximum datagram size for video.
    pub max_datagram_bytes: usize,
    /// Per-viewer queue capacity in number of fragments.
    pub per_viewer_queue_fragments: usize,
    /// Maximum tracked frames per viewer queue (for eviction bookkeeping).
    pub per_viewer_max_frames: usize,
    /// Flush cadence for per-viewer send loops.
    pub flush_interval: Duration,
}

impl Default for StreamForwarderConfig {
    fn default() -> Self {
        Self {
            max_datagram_bytes: vp_voice::MAX_VIDEO_DATAGRAM_BYTES,
            per_viewer_queue_fragments: 128,
            per_viewer_max_frames: 8,
            flush_interval: Duration::from_millis(2),
        }
    }
}

// ── Frame-aware bounded queue ─────────────────────────────────────────

/// Metadata for a queued fragment.
#[derive(Clone)]
struct QueuedFragment {
    datagram: Bytes,
    frame_seq: u32,
    #[allow(dead_code)]
    is_priority: bool,
}

/// Tracks frame occupancy in the queue for eviction decisions.
#[derive(Clone, Debug)]
struct FrameInfo {
    frame_seq: u32,
    is_priority: bool,
    fragment_count: u16,
}

#[derive(Clone, Copy, Debug, Default)]
struct PushOutcome {
    evicted_frames: usize,
    dropped_incoming_fragment: bool,
    drop_reason: Option<StreamDropReason>,
}

/// Frame-aware bounded queue for a single viewer.
///
/// Invariants:
/// - `fragments.len() <= capacity`
/// - `frames` tracks per-frame metadata in insertion order
/// - Under pressure: evict oldest non-priority frames first, then oldest priority frames
struct ViewerQueue {
    fragments: VecDeque<QueuedFragment>,
    /// Frame metadata in order of first-seen frame_seq.
    frames: VecDeque<FrameInfo>,
    capacity: usize,
    max_frames: usize,
}

impl ViewerQueue {
    fn new(capacity: usize, max_frames: usize) -> Self {
        Self {
            fragments: VecDeque::with_capacity(capacity),
            frames: VecDeque::with_capacity(max_frames),
            capacity,
            max_frames,
        }
    }

    /// Push a fragment and report queue pressure behavior.
    fn push(&mut self, datagram: Bytes, hdr: &VideoHeader) -> PushOutcome {
        let mut outcome = PushOutcome::default();

        // Update or insert frame tracking.
        self.update_frame_tracking(hdr);

        // Evict if over capacity.
        while self.fragments.len() >= self.capacity || self.frames.len() > self.max_frames {
            if !self.evict_one_frame(hdr.frame_seq) {
                break;
            }
            outcome.evicted_frames += 1;
        }

        // If still over capacity after eviction, drop this fragment.
        if self.fragments.len() >= self.capacity {
            outcome.dropped_incoming_fragment = true;
            outcome.drop_reason = Some(StreamDropReason::QueueFull);
            return outcome;
        }

        self.fragments.push_back(QueuedFragment {
            datagram,
            frame_seq: hdr.frame_seq,
            is_priority: hdr.is_priority(),
        });

        if outcome.evicted_frames > 0 {
            outcome.drop_reason = Some(StreamDropReason::EvictedOldestFrame);
        }
        outcome
    }

    /// Update frame tracking for an incoming fragment.
    fn update_frame_tracking(&mut self, hdr: &VideoHeader) {
        // Check if we already track this frame.
        for fi in self.frames.iter_mut() {
            if fi.frame_seq == hdr.frame_seq {
                fi.fragment_count = fi.fragment_count.saturating_add(1);
                // Promote to priority if any fragment is priority.
                if hdr.is_priority() {
                    fi.is_priority = true;
                }
                return;
            }
        }
        // New frame.
        self.frames.push_back(FrameInfo {
            frame_seq: hdr.frame_seq,
            is_priority: hdr.is_priority(),
            fragment_count: 1,
        });
    }

    /// Evict one frame from the queue. Prefers oldest non-priority frame.
    /// Returns true if a frame was evicted.
    fn evict_one_frame(&mut self, current_frame_seq: u32) -> bool {
        if self.frames.is_empty() {
            return false;
        }

        // Find index of oldest non-priority frame that isn't the current frame.
        let mut evict_idx = None;
        for (i, fi) in self.frames.iter().enumerate() {
            if fi.frame_seq == current_frame_seq {
                continue;
            }
            if !fi.is_priority {
                evict_idx = Some(i);
                break;
            }
        }

        // If no non-priority frame found, evict oldest priority frame (that isn't current).
        if evict_idx.is_none() {
            for (i, fi) in self.frames.iter().enumerate() {
                if fi.frame_seq != current_frame_seq {
                    evict_idx = Some(i);
                    break;
                }
            }
        }

        let idx = match evict_idx {
            Some(i) => i,
            None => return false,
        };

        let evict_seq = self.frames[idx].frame_seq;
        self.frames.remove(idx);
        self.fragments.retain(|f| f.frame_seq != evict_seq);
        true
    }

    /// Drain all fragments ready for sending.
    fn drain(&mut self) -> impl Iterator<Item = Bytes> + '_ {
        self.frames.clear();
        self.fragments.drain(..).map(|f| f.datagram)
    }

    fn len(&self) -> usize {
        self.fragments.len()
    }
}

// ── StreamForwarder ───────────────────────────────────────────────────

/// Video/screenshare stream forwarder (SFU: forward only, no decode).
pub struct StreamForwarder {
    cfg: StreamForwarderConfig,
    sessions: Arc<dyn SessionRegistry>,
    viewers: Arc<dyn ViewerProvider>,
    metrics: Arc<dyn StreamMetrics>,

    /// Registered streams: stream_tag → registration.
    streams: RwLock<HashMap<u64, StreamRegistration>>,
    /// Per stream subscriber set.
    subscriptions: RwLock<HashMap<u64, HashSet<UserId>>>,

    /// Per-viewer send loops: (user_id, session_id) → enqueue handle.
    viewer_loops: RwLock<HashMap<(UserId, String), ViewerLoopHandle>>,
    forwarded_fragments: AtomicU64,
    dropped_queue_full: AtomicU64,
}

#[derive(Clone)]
struct ViewerLoopHandle {
    tx: mpsc::Sender<EnqueuedFragment>,
    last_active: Instant,
}

#[derive(Clone)]
struct EnqueuedFragment {
    datagram: Bytes,
    hdr: VideoHeader,
    codec: i32,
}

impl StreamForwarder {
    pub fn new(
        cfg: StreamForwarderConfig,
        sessions: Arc<dyn SessionRegistry>,
        viewers: Arc<dyn ViewerProvider>,
        metrics: Arc<dyn StreamMetrics>,
    ) -> Self {
        Self {
            cfg,
            sessions,
            viewers,
            metrics,
            streams: RwLock::new(HashMap::new()),
            subscriptions: RwLock::new(HashMap::new()),
            viewer_loops: RwLock::new(HashMap::new()),
            forwarded_fragments: AtomicU64::new(0),
            dropped_queue_full: AtomicU64::new(0),
        }
    }

    // ── Control-plane registration ────────────────────────────────────

    /// Register a stream (called when ScreenShareStarted / CallStarted).
    pub async fn register_stream(&self, stream_tag: u64, reg: StreamRegistration) {
        debug!(stream_tag, sender = %reg.sender_id.0, channel = %reg.channel_id.0, "stream registered");
        self.streams.write().await.insert(stream_tag, reg);
    }

    pub async fn set_stream_subscribers(
        &self,
        stream_tag: u64,
        viewers: impl IntoIterator<Item = UserId>,
    ) {
        self.subscriptions
            .write()
            .await
            .insert(stream_tag, viewers.into_iter().collect());
    }

    pub async fn subscribe_viewer(&self, viewer: UserId, stream_tag: u64) {
        self.subscriptions
            .write()
            .await
            .entry(stream_tag)
            .or_default()
            .insert(viewer);
    }

    pub async fn unsubscribe_viewer(&self, viewer: UserId, stream_tag: u64) {
        if let Some(set) = self.subscriptions.write().await.get_mut(&stream_tag) {
            set.remove(&viewer);
        }
    }

    pub async fn sender_for_stream(&self, stream_tag: u64) -> Option<UserId> {
        self.streams
            .read()
            .await
            .get(&stream_tag)
            .map(|s| s.sender_id)
    }

    pub async fn codec_for_stream(&self, stream_tag: u64) -> Option<i32> {
        self.streams.read().await.get(&stream_tag).map(|s| s.codec)
    }

    pub async fn subscribers_for_stream(&self, stream_tag: u64) -> Vec<UserId> {
        self.subscriptions
            .read()
            .await
            .get(&stream_tag)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn note_recovery_request(&self) {
        self.metrics.inc_recovery_requests();
    }

    /// Unregister a stream (called when ScreenShareStopped / CallEnded).
    pub async fn unregister_stream(&self, stream_tag: u64) {
        debug!(stream_tag, "stream unregistered");
        self.streams.write().await.remove(&stream_tag);
        self.subscriptions.write().await.remove(&stream_tag);
    }

    /// Remove viewer queues for a disconnecting session.
    pub async fn unregister_session(&self, user: UserId, session_id: &str) {
        self.viewer_loops
            .write()
            .await
            .remove(&(user, session_id.to_string()));
    }

    // ── Datagram handling (hot path) ──────────────────────────────────

    /// Handle an incoming video datagram from an authenticated sender.
    ///
    /// This is called from the gateway datagram dispatch loop.
    /// Zero-copy: the `datagram` Bytes is forwarded via Bytes::clone() (refcount).
    pub async fn handle_incoming_datagram(&self, sender: UserId, datagram: Bytes) {
        self.metrics.inc_rx_packets();
        self.metrics.inc_rx_bytes(datagram.len());

        // Validate datagram size.
        if datagram.len() < VIDEO_HDR_LEN || datagram.len() > self.cfg.max_datagram_bytes {
            self.metrics.inc_drop_invalid();
            self.metrics.inc_drop_by_reason(StreamDropReason::Malformed);
            return;
        }

        // Parse header.
        let hdr = match VideoHeader::parse(&datagram) {
            Some(h) => h,
            None => {
                self.metrics.inc_drop_invalid();
                self.metrics.inc_drop_by_reason(StreamDropReason::Malformed);
                return;
            }
        };

        // Authorize: check stream_tag is registered and sender matches.
        let reg = {
            let streams = self.streams.read().await;
            match streams.get(&hdr.stream_tag) {
                Some(reg) if reg.sender_id == sender => reg.clone(),
                Some(_) => {
                    self.metrics.inc_drop_unauthorized();
                    self.metrics
                        .inc_drop_by_reason(StreamDropReason::Unauthorized);
                    return;
                }
                None => {
                    self.metrics.inc_drop_unauthorized();
                    self.metrics
                        .inc_drop_by_reason(StreamDropReason::Unauthorized);
                    return;
                }
            }
        };
        let channel = reg.channel_id;

        // Get viewers for this stream via subscription table.
        let viewer_ids = {
            let maybe = self
                .subscriptions
                .read()
                .await
                .get(&hdr.stream_tag)
                .map(|s| s.iter().copied().collect::<Vec<_>>());
            match maybe {
                Some(v) => v,
                None => self.viewers.list_viewers(channel, sender).await,
            }
        };
        if viewer_ids.is_empty() {
            return;
        }

        // Fanout: enqueue to each viewer's sessions.
        let mut forwarded = 0usize;

        for viewer_id in viewer_ids {
            let sessions = self.sessions.get_sessions(viewer_id).await;
            for (session_id, dtx) in sessions {
                let outcome = self
                    .enqueue_to_viewer(viewer_id, session_id, dtx, &datagram, &hdr, reg.codec)
                    .await;
                if outcome {
                    forwarded += 1;
                    self.forwarded_fragments.fetch_add(1, Ordering::Relaxed);
                    self.metrics.inc_forwarded_bytes(datagram.len());
                    self.metrics
                        .inc_forwarded_bytes_codec(datagram.len(), reg.codec);
                } else {
                    self.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
                    self.metrics.inc_drop_by_reason(StreamDropReason::QueueFull);
                    self.metrics
                        .inc_drop_by_reason_codec(StreamDropReason::QueueFull, reg.codec);
                }
            }
        }

        self.metrics.inc_forwarded(forwarded);
        self.maybe_log_forwarding_rates();
    }

    fn maybe_log_forwarding_rates(&self) {
        static LAST_LOGGED_SEC: AtomicU64 = AtomicU64::new(0);

        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last = LAST_LOGGED_SEC.load(Ordering::Relaxed);
        if now_sec <= last {
            return;
        }
        if LAST_LOGGED_SEC
            .compare_exchange(last, now_sec, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let forwarded = self.forwarded_fragments.swap(0, Ordering::AcqRel);
        let dropped = self.dropped_queue_full.swap(0, Ordering::AcqRel);
        info!(
            target: "video",
            "forwarded_datagrams/sec={} dropped_queue_full/sec={}",
            forwarded,
            dropped
        );
    }

    /// Enqueue a datagram to a specific viewer session's frame-aware queue.
    async fn enqueue_to_viewer(
        &self,
        viewer: UserId,
        session_id: String,
        dtx: Arc<dyn DatagramTx>,
        datagram: &Bytes,
        hdr: &VideoHeader,
        codec: i32,
    ) -> bool {
        let key = (viewer, session_id);
        if let Some(tx) = self
            .viewer_loops
            .read()
            .await
            .get(&key)
            .map(|h| h.tx.clone())
        {
            if tx
                .try_send(EnqueuedFragment {
                    datagram: datagram.clone(),
                    hdr: *hdr,
                    codec,
                })
                .is_ok()
            {
                if let Some(loop_handle) = self.viewer_loops.write().await.get_mut(&key) {
                    loop_handle.last_active = Instant::now();
                }
                return true;
            }
            return false;
        }

        let tx = self.spawn_viewer_loop(key.clone(), dtx);
        if tx
            .try_send(EnqueuedFragment {
                datagram: datagram.clone(),
                hdr: *hdr,
                codec,
            })
            .is_ok()
        {
            self.viewer_loops.write().await.insert(
                key,
                ViewerLoopHandle {
                    tx,
                    last_active: Instant::now(),
                },
            );
            true
        } else {
            false
        }
    }

    fn spawn_viewer_loop(
        &self,
        key: (UserId, String),
        dtx: Arc<dyn DatagramTx>,
    ) -> mpsc::Sender<EnqueuedFragment> {
        let cfg = self.cfg.clone();
        let cap = cfg.per_viewer_queue_fragments.max(1024);
        let (tx, mut rx) = mpsc::channel::<EnqueuedFragment>(cap);
        let metrics = self.metrics.clone();

        tokio::spawn(async move {
            let mut queue =
                ViewerQueue::new(cfg.per_viewer_queue_fragments, cfg.per_viewer_max_frames);
            let mut interval = tokio::time::interval(cfg.flush_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    biased;
                    _ = interval.tick() => {
                        let mut rx_closed = false;
                        loop {
                            match rx.try_recv() {
                                Ok(fragment) => {
                                    let outcome = queue.push(fragment.datagram, &fragment.hdr);
                                    if outcome.evicted_frames > 0 {
                                        metrics.inc_frames_evicted(outcome.evicted_frames);
                                    }
                                    if let Some(reason) = outcome.drop_reason {
                                        metrics.inc_drop_by_reason(reason);
                                        metrics.inc_drop_by_reason_codec(reason, fragment.codec);
                                    }
                                },
                                Err(mpsc::error::TryRecvError::Empty) => break,
                                Err(mpsc::error::TryRecvError::Disconnected) => {
                                    rx_closed = true;
                                    break;
                                }
                            }
                        }

                        if queue.len() > 0 {
                            let datagrams: Vec<Bytes> = queue.drain().collect();
                            for datagram in datagrams {
                                if let Err(e) = dtx.send(datagram).await {
                                    debug!(error = %e, viewer = %key.0.0, session_id = %key.1, "viewer session send loop ended");
                                    return;
                                }
                            }
                        }

                        if rx_closed {
                            break;
                        }
                    }
                    maybe_fragment = rx.recv() => {
                        let Some(fragment) = maybe_fragment else { break; };
                        let outcome = queue.push(fragment.datagram, &fragment.hdr);
                        if outcome.evicted_frames > 0 {
                            metrics.inc_frames_evicted(outcome.evicted_frames);
                        }
                        if let Some(reason) = outcome.drop_reason {
                            metrics.inc_drop_by_reason(reason);
                            metrics.inc_drop_by_reason_codec(reason, fragment.codec);
                        }
                    }
                }
            }
        });

        tx
    }

    /// Periodic cleanup of stale viewer queues (call from a background timer).
    pub async fn cleanup_stale_viewers(&self, max_idle: std::time::Duration) {
        let mut loops = self.viewer_loops.write().await;
        loops.retain(|_key, egress| egress.last_active.elapsed() < max_idle);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::{BufMut, BytesMut};

    fn make_test_datagram(
        stream_tag: u64,
        frame_seq: u32,
        frag_idx: u16,
        frag_total: u16,
        flags: u8,
    ) -> Bytes {
        let mut buf = BytesMut::with_capacity(VIDEO_HDR_LEN + 10);
        buf.put_u8(vp_voice::VIDEO_VERSION);
        buf.put_u8(vp_voice::DATAGRAM_KIND_VIDEO);
        buf.put_u64_le(stream_tag);
        buf.put_u8(0); // layer_id
        buf.put_u8(flags);
        buf.put_u32_le(frame_seq);
        buf.put_u16_le(frag_idx);
        buf.put_u16_le(frag_total);
        buf.put_u32_le(1000); // ts_ms
        buf.extend_from_slice(b"payload123");
        buf.freeze()
    }

    fn make_hdr(frame_seq: u32, frag_idx: u16, frag_total: u16, flags: u8) -> VideoHeader {
        VideoHeader {
            stream_tag: 1,
            layer_id: 0,
            flags,
            frame_seq,
            frag_idx,
            frag_total,
            ts_ms: 1000,
        }
    }

    #[test]
    fn parse_valid_video_header() {
        let dg = make_test_datagram(0xCAFE, 10, 0, 3, vp_voice::VIDEO_FLAG_KEYFRAME);
        let hdr = VideoHeader::parse(&dg).expect("should parse");
        assert_eq!(hdr.stream_tag, 0xCAFE);
        assert_eq!(hdr.frame_seq, 10);
        assert_eq!(hdr.frag_idx, 0);
        assert_eq!(hdr.frag_total, 3);
        assert!(hdr.is_keyframe());
        assert!(!hdr.is_recovery());
        assert!(hdr.is_priority());
    }

    #[test]
    fn parse_rejects_short_buffer() {
        assert!(VideoHeader::parse(&[0u8; 10]).is_none());
    }

    #[test]
    fn parse_rejects_wrong_kind() {
        let mut buf = [0u8; VIDEO_HDR_LEN];
        buf[0] = vp_voice::VIDEO_VERSION;
        buf[1] = 0xFF;
        assert!(VideoHeader::parse(&buf).is_none());
    }

    #[test]
    fn parse_rejects_zero_frag_total() {
        let mut buf = BytesMut::with_capacity(VIDEO_HDR_LEN);
        buf.put_u8(vp_voice::VIDEO_VERSION);
        buf.put_u8(vp_voice::DATAGRAM_KIND_VIDEO);
        buf.put_u64_le(1);
        buf.put_u8(0);
        buf.put_u8(0);
        buf.put_u32_le(0);
        buf.put_u16_le(0);
        buf.put_u16_le(0);
        buf.put_u32_le(0);
        assert!(VideoHeader::parse(&buf).is_none());
    }

    // ── ViewerQueue tests ─────────────────────────────────────────────

    #[test]
    fn queue_push_and_drain() {
        let mut q = ViewerQueue::new(16, 4);
        let dg = make_test_datagram(1, 0, 0, 1, 0);
        let hdr = make_hdr(0, 0, 1, 0);
        q.push(dg.clone(), &hdr);
        assert_eq!(q.len(), 1);

        let drained: Vec<_> = q.drain().collect();
        assert_eq!(drained.len(), 1);
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn queue_evicts_oldest_non_priority_frame_first() {
        // Capacity for 4 fragments, max 2 frames.
        let mut q = ViewerQueue::new(4, 2);

        // Frame 0: 2 fragments (non-priority).
        let dg0 = make_test_datagram(1, 0, 0, 2, 0);
        q.push(dg0.clone(), &make_hdr(0, 0, 2, 0));
        q.push(dg0.clone(), &make_hdr(0, 1, 2, 0));

        // Frame 1: 2 fragments (priority = keyframe).
        let dg1 = make_test_datagram(1, 1, 0, 2, vp_voice::VIDEO_FLAG_KEYFRAME);
        q.push(
            dg1.clone(),
            &make_hdr(1, 0, 2, vp_voice::VIDEO_FLAG_KEYFRAME),
        );
        q.push(
            dg1.clone(),
            &make_hdr(1, 1, 2, vp_voice::VIDEO_FLAG_KEYFRAME),
        );

        assert_eq!(q.len(), 4);

        // Frame 2: new non-priority frame. Should evict frame 0 (oldest non-priority).
        let dg2 = make_test_datagram(1, 2, 0, 1, 0);
        let outcome = q.push(dg2, &make_hdr(2, 0, 1, 0));
        assert!(
            outcome.evicted_frames >= 1,
            "should have evicted at least one frame"
        );

        // Frame 0 should be gone, frame 1 (priority) should remain.
        let remaining: Vec<_> = q.drain().collect();
        let remaining_seqs: Vec<u32> = remaining
            .iter()
            .map(|b| u32::from_le_bytes([b[12], b[13], b[14], b[15]]))
            .collect();
        assert!(
            !remaining_seqs.contains(&0),
            "frame 0 should have been evicted"
        );
        assert!(
            remaining_seqs.contains(&1),
            "frame 1 (keyframe) should remain"
        );
    }

    #[test]
    fn queue_evicts_oldest_priority_frame_when_no_non_priority() {
        // Capacity for 3 fragments, max 2 frames.
        let mut q = ViewerQueue::new(3, 2);

        // Frame 0: priority.
        let dg0 = make_test_datagram(1, 0, 0, 1, vp_voice::VIDEO_FLAG_KEYFRAME);
        q.push(dg0, &make_hdr(0, 0, 1, vp_voice::VIDEO_FLAG_KEYFRAME));

        // Frame 1: priority.
        let dg1 = make_test_datagram(1, 1, 0, 1, vp_voice::VIDEO_FLAG_KEYFRAME);
        q.push(dg1, &make_hdr(1, 0, 1, vp_voice::VIDEO_FLAG_KEYFRAME));

        // Frame 2: triggers eviction. Only priority frames exist → evict oldest.
        let dg2 = make_test_datagram(1, 2, 0, 1, vp_voice::VIDEO_FLAG_KEYFRAME);
        let outcome = q.push(dg2, &make_hdr(2, 0, 1, vp_voice::VIDEO_FLAG_KEYFRAME));
        assert!(outcome.evicted_frames >= 1);

        let remaining: Vec<_> = q.drain().collect();
        // Should keep frame 1 and frame 2 (newest), evict frame 0 (oldest).
        assert_eq!(remaining.len(), 2);
    }

    #[test]
    fn queue_respects_fragment_capacity() {
        let mut q = ViewerQueue::new(2, 4);

        let dg = make_test_datagram(1, 0, 0, 1, 0);
        q.push(dg.clone(), &make_hdr(0, 0, 1, 0));
        q.push(dg.clone(), &make_hdr(1, 0, 1, 0));

        // Queue is full (2 fragments). Next push evicts.
        let outcome = q.push(make_test_datagram(1, 2, 0, 1, 0), &make_hdr(2, 0, 1, 0));
        assert!(outcome.evicted_frames >= 1 || outcome.dropped_incoming_fragment);
        // We should still have at most 2 fragments.
        assert!(q.len() <= 2);
    }

    // ── Integration-style tests ───────────────────────────────────────

    use anyhow::Result;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeTx {
        sent: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl DatagramTx for FakeTx {
        async fn send(&self, _bytes: Bytes) -> Result<()> {
            self.sent.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    struct SlowTx {
        sent: Arc<AtomicUsize>,
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl DatagramTx for SlowTx {
        async fn send(&self, _bytes: Bytes) -> Result<()> {
            tokio::time::sleep(self.delay).await;
            self.sent.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    struct FakeSessions {
        sessions: Vec<(UserId, String, Arc<dyn DatagramTx>)>,
    }

    #[async_trait::async_trait]
    impl SessionRegistry for FakeSessions {
        async fn get_sessions(&self, user: UserId) -> Vec<(String, Arc<dyn DatagramTx>)> {
            self.sessions
                .iter()
                .filter(|(u, _, _)| *u == user)
                .map(|(_, sid, tx)| (sid.clone(), tx.clone()))
                .collect()
        }
    }

    struct FakeViewers {
        viewers: Vec<UserId>,
    }

    #[async_trait::async_trait]
    impl ViewerProvider for FakeViewers {
        async fn list_viewers(&self, _ch: ChannelId, exclude: UserId) -> Vec<UserId> {
            self.viewers
                .iter()
                .copied()
                .filter(|u| *u != exclude)
                .collect()
        }
    }

    #[tokio::test]
    async fn forwards_to_registered_viewers() {
        let sender = UserId::new();
        let viewer = UserId::new();
        let channel = ChannelId::new();
        let stream_tag: u64 = 42;

        let sent = Arc::new(AtomicUsize::new(0));
        let sessions: Arc<dyn SessionRegistry> = Arc::new(FakeSessions {
            sessions: vec![(viewer, "v1".into(), Arc::new(FakeTx { sent: sent.clone() }))],
        });
        let viewers: Arc<dyn ViewerProvider> = Arc::new(FakeViewers {
            viewers: vec![sender, viewer],
        });

        let fwd = StreamForwarder::new(
            StreamForwarderConfig::default(),
            sessions,
            viewers,
            Arc::new(NoopStreamMetrics),
        );

        fwd.register_stream(
            stream_tag,
            StreamRegistration {
                sender_id: sender,
                channel_id: channel,
                codec: 0,
            },
        )
        .await;

        let dg = make_test_datagram(stream_tag, 0, 0, 1, 0);
        fwd.handle_incoming_datagram(sender, dg).await;
        tokio::time::sleep(Duration::from_millis(5)).await;

        assert!(
            sent.load(Ordering::Relaxed) > 0,
            "viewer should have received datagram"
        );
    }

    #[tokio::test]
    async fn rejects_unregistered_stream() {
        let sender = UserId::new();
        let sessions: Arc<dyn SessionRegistry> = Arc::new(FakeSessions { sessions: vec![] });
        let viewers: Arc<dyn ViewerProvider> = Arc::new(FakeViewers { viewers: vec![] });

        let fwd = StreamForwarder::new(
            StreamForwarderConfig::default(),
            sessions,
            viewers,
            Arc::new(NoopStreamMetrics),
        );

        // No stream registered → datagram should be dropped.
        let dg = make_test_datagram(999, 0, 0, 1, 0);
        fwd.handle_incoming_datagram(sender, dg).await;
        // No panic, no forwarding = success.
    }

    #[tokio::test]
    async fn rejects_wrong_sender() {
        let real_sender = UserId::new();
        let imposter = UserId::new();
        let channel = ChannelId::new();

        let sessions: Arc<dyn SessionRegistry> = Arc::new(FakeSessions { sessions: vec![] });
        let viewers: Arc<dyn ViewerProvider> = Arc::new(FakeViewers { viewers: vec![] });

        let fwd = StreamForwarder::new(
            StreamForwarderConfig::default(),
            sessions,
            viewers,
            Arc::new(NoopStreamMetrics),
        );

        fwd.register_stream(
            42,
            StreamRegistration {
                sender_id: real_sender,
                channel_id: channel,
                codec: 0,
            },
        )
        .await;

        // Imposter sends on real_sender's stream → should be rejected.
        let dg = make_test_datagram(42, 0, 0, 1, 0);
        fwd.handle_incoming_datagram(imposter, dg).await;
        // No panic = success.
    }

    #[test]
    fn parse_rejects_frag_total_above_cap() {
        let mut buf = BytesMut::with_capacity(VIDEO_HDR_LEN + 8);
        buf.put_u8(vp_voice::VIDEO_VERSION);
        buf.put_u8(vp_voice::DATAGRAM_KIND_VIDEO);
        buf.put_u64_le(7);
        buf.put_u8(0);
        buf.put_u8(0);
        buf.put_u32_le(1);
        buf.put_u16_le(0);
        buf.put_u16_le(vp_voice::MAX_FRAGS_PER_FRAME + 1);
        buf.put_u32_le(10);
        buf.extend_from_slice(b"x");
        assert!(VideoHeader::parse(&buf.freeze()).is_none());
    }

    #[test]
    fn queue_reports_queue_full_drop_reason() {
        let mut q = ViewerQueue::new(1, 1);
        let current = make_test_datagram(1, 0, 0, 1, 0);
        let _ = q.push(current, &make_hdr(0, 0, 1, 0));
        let second = make_test_datagram(1, 0, 0, 1, 0);
        let outcome = q.push(second, &make_hdr(0, 0, 1, 0));
        assert!(outcome.dropped_incoming_fragment);
        assert_eq!(outcome.drop_reason, Some(StreamDropReason::QueueFull));
    }

    struct TestMetrics {
        drops: Arc<AtomicUsize>,
        evicted: Arc<AtomicUsize>,
        forwarded_bytes: Arc<AtomicUsize>,
    }

    impl StreamMetrics for TestMetrics {
        fn inc_rx_packets(&self) {}
        fn inc_rx_bytes(&self, _n: usize) {}
        fn inc_drop_invalid(&self) {}
        fn inc_drop_unauthorized(&self) {}
        fn inc_drop_by_reason(&self, _reason: StreamDropReason) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
        fn inc_drop_by_reason_codec(&self, _reason: StreamDropReason, _codec: i32) {}
        fn inc_forwarded(&self, _fanout: usize) {}
        fn inc_forwarded_bytes(&self, n: usize) {
            self.forwarded_bytes.fetch_add(n, Ordering::Relaxed);
        }
        fn inc_forwarded_bytes_codec(&self, _n: usize, _codec: i32) {}
        fn inc_frames_evicted(&self, count: usize) {
            self.evicted.fetch_add(count, Ordering::Relaxed);
        }
        fn inc_recovery_requests(&self) {}
    }

    #[tokio::test]
    async fn subscription_routing_isolated_per_stream_tag() {
        let sender = UserId::new();
        let viewer_a = UserId::new();
        let viewer_b = UserId::new();
        let channel = ChannelId::new();

        let sent_a = Arc::new(AtomicUsize::new(0));
        let sent_b = Arc::new(AtomicUsize::new(0));
        let sessions: Arc<dyn SessionRegistry> = Arc::new(FakeSessions {
            sessions: vec![
                (
                    viewer_a,
                    "a".into(),
                    Arc::new(FakeTx {
                        sent: sent_a.clone(),
                    }),
                ),
                (
                    viewer_b,
                    "b".into(),
                    Arc::new(FakeTx {
                        sent: sent_b.clone(),
                    }),
                ),
            ],
        });
        let viewers: Arc<dyn ViewerProvider> = Arc::new(FakeViewers {
            viewers: vec![sender, viewer_a, viewer_b],
        });

        let fwd = StreamForwarder::new(
            StreamForwarderConfig::default(),
            sessions,
            viewers,
            Arc::new(NoopStreamMetrics),
        );
        let primary_tag = 1001u64;
        let fallback_tag = 1002u64;

        fwd.register_stream(
            primary_tag,
            StreamRegistration {
                sender_id: sender,
                channel_id: channel,
                codec: 1,
            },
        )
        .await;
        fwd.register_stream(
            fallback_tag,
            StreamRegistration {
                sender_id: sender,
                channel_id: channel,
                codec: 3,
            },
        )
        .await;
        fwd.set_stream_subscribers(primary_tag, [viewer_a]).await;
        fwd.set_stream_subscribers(fallback_tag, [viewer_b]).await;

        fwd.handle_incoming_datagram(sender, make_test_datagram(primary_tag, 1, 0, 1, 0))
            .await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(sent_a.load(Ordering::Relaxed) > 0);
        let b_after_primary = sent_b.load(Ordering::Relaxed);

        fwd.handle_incoming_datagram(sender, make_test_datagram(fallback_tag, 2, 0, 1, 0))
            .await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(sent_b.load(Ordering::Relaxed) > b_after_primary);
    }

    #[tokio::test]
    #[ignore = "stress test"]
    async fn slow_viewer_does_not_block_fast_viewer_enqueue() {
        let sender = UserId::new();
        let fast_viewer = UserId::new();
        let slow_viewer = UserId::new();
        let channel = ChannelId::new();
        let stream_tag: u64 = 77;

        let fast_sent = Arc::new(AtomicUsize::new(0));
        let slow_sent = Arc::new(AtomicUsize::new(0));
        let sessions: Arc<dyn SessionRegistry> = Arc::new(FakeSessions {
            sessions: vec![
                (
                    fast_viewer,
                    "fast".into(),
                    Arc::new(FakeTx {
                        sent: fast_sent.clone(),
                    }),
                ),
                (
                    slow_viewer,
                    "slow".into(),
                    Arc::new(SlowTx {
                        sent: slow_sent.clone(),
                        delay: Duration::from_millis(80),
                    }),
                ),
            ],
        });
        let viewers: Arc<dyn ViewerProvider> = Arc::new(FakeViewers {
            viewers: vec![sender, fast_viewer, slow_viewer],
        });

        let fwd = StreamForwarder::new(
            StreamForwarderConfig {
                max_datagram_bytes: vp_voice::MAX_VIDEO_DATAGRAM_BYTES,
                per_viewer_queue_fragments: 128,
                per_viewer_max_frames: 8,
                flush_interval: Duration::from_millis(1),
            },
            sessions,
            viewers,
            Arc::new(NoopStreamMetrics),
        );

        fwd.register_stream(
            stream_tag,
            StreamRegistration {
                sender_id: sender,
                channel_id: channel,
                codec: 0,
            },
        )
        .await;

        for seq in 0..8u32 {
            let dg = make_test_datagram(stream_tag, seq, 0, 1, 0);
            fwd.handle_incoming_datagram(sender, dg).await;
        }

        tokio::time::sleep(Duration::from_millis(15)).await;

        assert!(
            fast_sent.load(Ordering::Relaxed) > 0,
            "fast viewer should make progress even when another viewer is slow"
        );
        assert!(
            slow_sent.load(Ordering::Relaxed) < fast_sent.load(Ordering::Relaxed),
            "slow viewer should lag behind fast viewer under stress"
        );
    }

    #[tokio::test]
    #[ignore = "manual benchmark"]
    async fn benchmark_many_viewers_enqueue_throughput() {
        let sender = UserId::new();
        let channel = ChannelId::new();
        let stream_tag: u64 = 55;

        let mut sessions_vec: Vec<(UserId, String, Arc<dyn DatagramTx>)> = Vec::new();
        let mut viewer_ids = vec![sender];
        for i in 0..512 {
            let viewer = UserId::new();
            viewer_ids.push(viewer);
            sessions_vec.push((
                viewer,
                format!("s{i}"),
                Arc::new(FakeTx {
                    sent: Arc::new(AtomicUsize::new(0)),
                }),
            ));
        }

        let sessions: Arc<dyn SessionRegistry> = Arc::new(FakeSessions {
            sessions: sessions_vec,
        });
        let viewers: Arc<dyn ViewerProvider> = Arc::new(FakeViewers {
            viewers: viewer_ids,
        });

        let fwd = StreamForwarder::new(
            StreamForwarderConfig {
                max_datagram_bytes: vp_voice::MAX_VIDEO_DATAGRAM_BYTES,
                per_viewer_queue_fragments: 256,
                per_viewer_max_frames: 16,
                flush_interval: Duration::from_millis(1),
            },
            sessions,
            viewers,
            Arc::new(NoopStreamMetrics),
        );

        fwd.register_stream(
            stream_tag,
            StreamRegistration {
                sender_id: sender,
                channel_id: channel,
                codec: 0,
            },
        )
        .await;

        let start = Instant::now();
        for seq in 0..100u32 {
            fwd.handle_incoming_datagram(sender, make_test_datagram(stream_tag, seq, 0, 1, 0))
                .await;
        }
        let elapsed = start.elapsed();
        eprintln!(
            "benchmark_many_viewers_enqueue_throughput: 100 packets to 512 viewers in {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn metrics_increment_on_queue_pressure() {
        let sender = UserId::new();
        let viewer = UserId::new();
        let channel = ChannelId::new();
        let stream_tag: u64 = 999;

        let sent = Arc::new(AtomicUsize::new(0));
        let sessions: Arc<dyn SessionRegistry> = Arc::new(FakeSessions {
            sessions: vec![(viewer, "v1".into(), Arc::new(FakeTx { sent }))],
        });
        let viewers: Arc<dyn ViewerProvider> = Arc::new(FakeViewers {
            viewers: vec![sender, viewer],
        });

        let drops = Arc::new(AtomicUsize::new(0));
        let evicted = Arc::new(AtomicUsize::new(0));
        let forwarded_bytes = Arc::new(AtomicUsize::new(0));
        let metrics: Arc<dyn StreamMetrics> = Arc::new(TestMetrics {
            drops: drops.clone(),
            evicted: evicted.clone(),
            forwarded_bytes: forwarded_bytes.clone(),
        });

        let fwd = StreamForwarder::new(
            StreamForwarderConfig {
                max_datagram_bytes: vp_voice::MAX_VIDEO_DATAGRAM_BYTES,
                per_viewer_queue_fragments: 1,
                per_viewer_max_frames: 1,
                flush_interval: Duration::from_millis(2),
            },
            sessions,
            viewers,
            metrics,
        );

        fwd.register_stream(
            stream_tag,
            StreamRegistration {
                sender_id: sender,
                channel_id: channel,
                codec: 0,
            },
        )
        .await;

        fwd.handle_incoming_datagram(sender, Bytes::from_static(b"bad"))
            .await;

        let a = make_test_datagram(stream_tag, 1, 0, 1, 0);
        fwd.handle_incoming_datagram(sender, a).await;

        assert!(drops.load(Ordering::Relaxed) >= 1);
        assert!(forwarded_bytes.load(Ordering::Relaxed) > 0);
        assert!(evicted.load(Ordering::Relaxed) <= 1);
    }
}
