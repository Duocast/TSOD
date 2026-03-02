use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::{sync::Notify, task::JoinHandle, time::timeout};
use tracing::warn;

use crate::net::UiLogTx;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatagramKind {
    Voice,
    Video,
    Control,
}

#[derive(Clone)]
pub struct EgressItem {
    pub kind: DatagramKind,
    pub stream_tag: Option<u64>,
    pub frame_seq: Option<u32>,
    pub deadline: Option<Instant>,
    pub bytes: Bytes,
    pub is_keyframe: bool,
}

#[derive(Clone, Debug)]
pub struct EgressConfig {
    pub mtu_bytes: usize,
    pub video_target_bitrate_bps: u32,
    pub video_max_burst_datagrams: u32,
    pub max_queue_voice: usize,
    pub max_queue_video_frames: usize,
    pub max_queue_video_frags: usize,
    pub frame_deadline_ms: u64,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            mtu_bytes: 1200,
            video_target_bitrate_bps: 8_000_000,
            video_max_burst_datagrams: 8,
            max_queue_voice: 512,
            max_queue_video_frames: 8,
            max_queue_video_frags: 2048,
            frame_deadline_ms: 80,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DropReason {
    QueueFullVoice,
    QueueFullVideo,
    DeadlineVideo,
}

#[derive(Default)]
pub struct EgressStats {
    pub tx_datagrams: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub tx_voice: AtomicU64,
    pub tx_video: AtomicU64,
    pub tx_control: AtomicU64,
    pub blocked_events: AtomicU64,
    pub drop_queue_full_voice: AtomicU64,
    pub drop_queue_full_video: AtomicU64,
    pub drop_deadline_video: AtomicU64,
    pub drop_too_large_voice: AtomicU64,
    pub drop_too_large_video: AtomicU64,
    pub fatal_errors: AtomicU64,
}

type FrameKey = (u64, u32);

#[derive(Clone)]
struct FrameBucket {
    deadline: Instant,
    is_keyframe: bool,
    fragments: VecDeque<Bytes>,
}

struct VideoFrameQueue {
    frames: HashMap<FrameKey, FrameBucket>,
    order: VecDeque<FrameKey>,
    total_frags: usize,
    max_frames: usize,
    max_frags: usize,
}

impl VideoFrameQueue {
    fn new(max_frames: usize, max_frags: usize) -> Self {
        Self {
            frames: HashMap::new(),
            order: VecDeque::new(),
            total_frags: 0,
            max_frames,
            max_frags,
        }
    }

    fn push(
        &mut self,
        stream_tag: u64,
        frame_seq: u32,
        is_keyframe: bool,
        deadline: Instant,
        bytes: Bytes,
    ) {
        let key = (stream_tag, frame_seq);
        if !self.frames.contains_key(&key) {
            self.order.push_back(key);
            self.frames.insert(
                key,
                FrameBucket {
                    deadline,
                    is_keyframe,
                    fragments: VecDeque::new(),
                },
            );
        }
        if let Some(frame) = self.frames.get_mut(&key) {
            frame.fragments.push_back(bytes);
            frame.deadline = deadline;
            frame.is_keyframe = is_keyframe;
            self.total_frags += 1;
        }
    }

    fn enforce_bounds(&mut self) -> usize {
        let mut dropped = 0;
        while self.frames.len() > self.max_frames || self.total_frags > self.max_frags {
            let key = self
                .order
                .iter()
                .copied()
                .find(|k| self.frames.get(k).map(|f| !f.is_keyframe).unwrap_or(false))
                .or_else(|| self.order.front().copied());
            if let Some(k) = key {
                dropped += self.drop_frame(k);
            } else {
                break;
            }
        }
        dropped
    }

    fn drop_expired(&mut self, now: Instant) -> usize {
        let mut dropped = 0;
        loop {
            let Some(key) = self.order.front().copied() else {
                break;
            };
            let expired = self
                .frames
                .get(&key)
                .map(|f| f.deadline <= now)
                .unwrap_or(true);
            if !expired {
                break;
            }
            dropped += self.drop_frame(key);
        }
        dropped
    }

    fn pop_next_fragment(&mut self, now: Instant) -> Option<EgressItem> {
        self.drop_expired(now);
        let key = self.order.front().copied()?;
        let frame = self.frames.get_mut(&key)?;
        let bytes = frame.fragments.pop_front()?;
        self.total_frags = self.total_frags.saturating_sub(1);
        let out = EgressItem {
            kind: DatagramKind::Video,
            stream_tag: Some(key.0),
            frame_seq: Some(key.1),
            deadline: Some(frame.deadline),
            bytes,
            is_keyframe: frame.is_keyframe,
        };
        if frame.fragments.is_empty() {
            self.frames.remove(&key);
            self.order.pop_front();
        }
        Some(out)
    }

    fn drop_frame(&mut self, key: FrameKey) -> usize {
        self.order.retain(|k| *k != key);
        if let Some(frame) = self.frames.remove(&key) {
            let n = frame.fragments.len();
            self.total_frags = self.total_frags.saturating_sub(n);
            n
        } else {
            0
        }
    }
}

struct QueueState {
    voice: VecDeque<Bytes>,
    video: VideoFrameQueue,
}

pub struct EgressScheduler {
    conn: quinn::Connection,
    ui_log_tx: UiLogTx,
    cfg: EgressConfig,
    state: Mutex<QueueState>,
    notify: Notify,
    stats: Arc<EgressStats>,
    voice_burst: Mutex<u32>,
}

impl EgressScheduler {
    pub fn new(conn: quinn::Connection, cfg: EgressConfig, ui_log_tx: UiLogTx) -> Arc<Self> {
        Arc::new(Self {
            conn,
            ui_log_tx,
            state: Mutex::new(QueueState {
                voice: VecDeque::new(),
                video: VideoFrameQueue::new(cfg.max_queue_video_frames, cfg.max_queue_video_frags),
            }),
            notify: Notify::new(),
            stats: Arc::new(EgressStats::default()),
            voice_burst: Mutex::new(0),
            cfg,
        })
    }

    pub fn start(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move { self.egress_loop().await })
    }

    pub fn stats(&self) -> Arc<EgressStats> {
        self.stats.clone()
    }

    pub fn enqueue_voice(&self, bytes: Bytes) -> Result<(), DropReason> {
        let mut state = self.state.lock().expect("egress queue poisoned");
        if state.voice.len() >= self.cfg.max_queue_voice {
            state.voice.pop_front();
            self.stats
                .drop_queue_full_voice
                .fetch_add(1, Ordering::Relaxed);
        }
        state.voice.push_back(bytes);
        drop(state);
        self.notify.notify_one();
        Ok(())
    }

    pub fn enqueue_video_fragment(
        &self,
        stream_tag: u64,
        frame_seq: u32,
        is_keyframe: bool,
        capture_ts: Instant,
        bytes: Bytes,
    ) -> Result<(), DropReason> {
        let deadline = capture_ts + Duration::from_millis(self.cfg.frame_deadline_ms);
        if deadline <= Instant::now() {
            self.stats
                .drop_deadline_video
                .fetch_add(1, Ordering::Relaxed);
            return Err(DropReason::DeadlineVideo);
        }
        let mut state = self.state.lock().expect("egress queue poisoned");
        state
            .video
            .push(stream_tag, frame_seq, is_keyframe, deadline, bytes);
        let dropped = state.video.enforce_bounds();
        if dropped > 0 {
            self.stats
                .drop_queue_full_video
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
        drop(state);
        self.notify.notify_one();
        Ok(())
    }

    fn next_item(&self) -> Option<EgressItem> {
        const VOICE_BURST: u32 = 6;
        let mut state = self.state.lock().expect("egress queue poisoned");
        let mut voice_burst = self
            .voice_burst
            .lock()
            .expect("egress voice burst poisoned");
        let expired = state.video.drop_expired(Instant::now());
        if expired > 0 {
            self.stats
                .drop_deadline_video
                .fetch_add(expired as u64, Ordering::Relaxed);
        }
        if !state.voice.is_empty() && *voice_burst < VOICE_BURST {
            let bytes = state
                .voice
                .pop_front()
                .expect("voice queue unexpectedly empty");
            *voice_burst += 1;
            return Some(EgressItem {
                kind: DatagramKind::Voice,
                stream_tag: None,
                frame_seq: None,
                deadline: None,
                bytes,
                is_keyframe: false,
            });
        }
        if let Some(video_item) = state.video.pop_next_fragment(Instant::now()) {
            *voice_burst = 0;
            return Some(video_item);
        }
        if let Some(bytes) = state.voice.pop_front() {
            if *voice_burst >= VOICE_BURST {
                *voice_burst = 0;
            }
            *voice_burst += 1;
            return Some(EgressItem {
                kind: DatagramKind::Voice,
                stream_tag: None,
                frame_seq: None,
                deadline: None,
                bytes,
                is_keyframe: false,
            });
        }
        None
    }

    async fn egress_loop(self: Arc<Self>) {
        let bytes_per_sec = self.cfg.video_target_bitrate_bps as f64 / 8.0;
        let burst_bytes = (self.cfg.video_max_burst_datagrams as usize * self.cfg.mtu_bytes) as f64;
        let mut tokens = burst_bytes;
        let mut last_refill = Instant::now();

        loop {
            let Some(item) = self.next_item() else {
                let _ = timeout(Duration::from_millis(10), self.notify.notified()).await;
                continue;
            };

            if item.kind == DatagramKind::Video {
                let now = Instant::now();
                let elapsed = now.saturating_duration_since(last_refill).as_secs_f64();
                tokens = (tokens + elapsed * bytes_per_sec).min(burst_bytes);
                last_refill = now;
                let need = item.bytes.len() as f64;
                if tokens < need {
                    let wait_s = ((need - tokens) / bytes_per_sec).max(0.0);
                    tokio::time::sleep(Duration::from_secs_f64(wait_s.min(0.02))).await;
                    self.requeue_front(item);
                    continue;
                }
                tokens -= need;
                if let Some(deadline) = item.deadline {
                    if Instant::now() > deadline {
                        self.stats
                            .drop_deadline_video
                            .fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
            }

            if self.conn.datagram_send_buffer_space() < item.bytes.len() {
                self.stats.blocked_events.fetch_add(1, Ordering::Relaxed);
            }
            let sent = match self.conn.send_datagram(item.bytes.clone()) {
                Ok(()) => true,
                Err(quinn::SendDatagramError::TooLarge) => {
                    match item.kind {
                        DatagramKind::Voice => self
                            .stats
                            .drop_too_large_voice
                            .fetch_add(1, Ordering::Relaxed),
                        DatagramKind::Video => self
                            .stats
                            .drop_too_large_video
                            .fetch_add(1, Ordering::Relaxed),
                        DatagramKind::Control => {}
                    };
                    let _ = self.ui_log_tx.send(format!(
                        "[egress] drop TooLarge kind={:?} len={} mtu={}",
                        item.kind,
                        item.bytes.len(),
                        self.cfg.mtu_bytes
                    ));
                    continue;
                }
                Err(quinn::SendDatagramError::UnsupportedByPeer) => {
                    self.stats.fatal_errors.fetch_add(1, Ordering::Relaxed);
                    let line = "[egress] exiting: send_datagram(_wait) failed: UnsupportedByPeer"
                        .to_string();
                    let _ = self.ui_log_tx.send(line.clone());
                    warn!("{line}");
                    return;
                }
                Err(quinn::SendDatagramError::Disabled) => {
                    self.stats.fatal_errors.fetch_add(1, Ordering::Relaxed);
                    let line =
                        "[egress] exiting: send_datagram(_wait) failed: Disabled".to_string();
                    let _ = self.ui_log_tx.send(line.clone());
                    warn!("{line}");
                    return;
                }
                Err(quinn::SendDatagramError::ConnectionLost(_)) => {
                    self.stats.fatal_errors.fetch_add(1, Ordering::Relaxed);
                    let line =
                        "[egress] exiting: send_datagram(_wait) failed: ConnectionLost".to_string();
                    let _ = self.ui_log_tx.send(line.clone());
                    warn!("{line}");
                    return;
                }
            };
            if !sent {
                continue;
            }
            self.stats.tx_datagrams.fetch_add(1, Ordering::Relaxed);
            self.stats
                .tx_bytes
                .fetch_add(item.bytes.len() as u64, Ordering::Relaxed);
            match item.kind {
                DatagramKind::Voice => self.stats.tx_voice.fetch_add(1, Ordering::Relaxed),
                DatagramKind::Video => self.stats.tx_video.fetch_add(1, Ordering::Relaxed),
                DatagramKind::Control => self.stats.tx_control.fetch_add(1, Ordering::Relaxed),
            };
        }
    }

    fn requeue_front(&self, item: EgressItem) {
        let mut state = self.state.lock().expect("egress queue poisoned");
        match item.kind {
            DatagramKind::Voice => state.voice.push_front(item.bytes),
            DatagramKind::Video => {
                if let (Some(stream_tag), Some(frame_seq), Some(deadline)) =
                    (item.stream_tag, item.frame_seq, item.deadline)
                {
                    state.video.push(
                        stream_tag,
                        frame_seq,
                        item.is_keyframe,
                        deadline,
                        item.bytes,
                    );
                }
            }
            DatagramKind::Control => state.voice.push_front(item.bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_prefers_non_keyframe() {
        let mut q = VideoFrameQueue::new(2, 10);
        let now = Instant::now() + Duration::from_secs(1);
        q.push(1, 1, true, now, Bytes::from_static(b"k"));
        q.push(1, 2, false, now, Bytes::from_static(b"n"));
        q.push(1, 3, true, now, Bytes::from_static(b"k2"));
        q.enforce_bounds();
        assert!(q.frames.contains_key(&(1, 1)));
        assert!(q.frames.contains_key(&(1, 3)));
        assert!(!q.frames.contains_key(&(1, 2)));
    }

    #[test]
    fn deadline_drop_removes_old_buckets() {
        let mut q = VideoFrameQueue::new(4, 10);
        let now = Instant::now();
        q.push(
            1,
            1,
            false,
            now - Duration::from_millis(1),
            Bytes::from_static(b"a"),
        );
        q.push(
            1,
            2,
            false,
            now + Duration::from_secs(1),
            Bytes::from_static(b"b"),
        );
        q.drop_expired(now);
        assert!(!q.frames.contains_key(&(1, 1)));
        assert!(q.frames.contains_key(&(1, 2)));
    }

    #[test]
    fn bounded_frags_and_frames_enforced() {
        let mut q = VideoFrameQueue::new(2, 2);
        let now = Instant::now() + Duration::from_secs(1);
        q.push(1, 1, false, now, Bytes::from_static(b"a"));
        q.push(1, 1, false, now, Bytes::from_static(b"b"));
        q.push(1, 2, false, now, Bytes::from_static(b"c"));
        q.enforce_bounds();
        assert!(q.frames.len() <= 2);
        assert!(q.total_frags <= 2);
    }
}
