use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use bytes::Bytes;
use tokio::{sync::Notify, time::timeout};
use tracing::warn;

type FrameKey = (u64, u32);

#[derive(Default)]
pub struct EgressStats {
    pub blocked_events: AtomicU64,
    pub drop_queue_full: AtomicU64,
    pub drop_deadline: AtomicU64,
    pub fatal_errors: AtomicU64,
}

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
        if let Some(f) = self.frames.get_mut(&key) {
            f.fragments.push_back(bytes);
            f.deadline = deadline;
            f.is_keyframe = is_keyframe;
            self.total_frags += 1;
        }
    }
    fn pop_next(&mut self, now: Instant) -> Option<Bytes> {
        self.drop_expired(now);
        let key = self.order.front().copied()?;
        let frame = self.frames.get_mut(&key)?;
        let b = frame.fragments.pop_front()?;
        self.total_frags = self.total_frags.saturating_sub(1);
        if frame.fragments.is_empty() {
            self.frames.remove(&key);
            self.order.pop_front();
        }
        Some(b)
    }
    fn drop_expired(&mut self, now: Instant) -> usize {
        let mut dropped = 0;
        loop {
            let Some(k) = self.order.front().copied() else {
                break;
            };
            let expired = self
                .frames
                .get(&k)
                .map(|f| f.deadline <= now)
                .unwrap_or(true);
            if !expired {
                break;
            }
            dropped += self.drop_frame(k);
        }
        dropped
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
    fn drop_frame(&mut self, key: FrameKey) -> usize {
        self.order.retain(|k| *k != key);
        if let Some(f) = self.frames.remove(&key) {
            let n = f.fragments.len();
            self.total_frags = self.total_frags.saturating_sub(n);
            n
        } else {
            0
        }
    }
}

pub struct EgressScheduler {
    conn: quinn::Connection,
    queue: Mutex<VideoFrameQueue>,
    notify: Notify,
    stats: Arc<EgressStats>,
}

impl EgressScheduler {
    pub fn new(conn: quinn::Connection) -> Arc<Self> {
        let this = Arc::new(Self {
            conn,
            queue: Mutex::new(VideoFrameQueue::new(8, 2048)),
            notify: Notify::new(),
            stats: Arc::new(EgressStats::default()),
        });
        tokio::spawn(this.clone().run());
        this
    }

    pub fn stats(&self) -> Arc<EgressStats> {
        self.stats.clone()
    }

    pub fn enqueue_video(
        &self,
        bytes: Bytes,
        stream_tag: u64,
        frame_seq: u32,
        is_keyframe: bool,
        deadline: Instant,
    ) {
        let mut q = self.queue.lock().expect("queue poisoned");
        if deadline <= Instant::now() {
            self.stats.drop_deadline.fetch_add(1, Ordering::Relaxed);
            return;
        }
        q.push(stream_tag, frame_seq, is_keyframe, deadline, bytes);
        let n = q.enforce_bounds();
        if n > 0 {
            self.stats
                .drop_queue_full
                .fetch_add(n as u64, Ordering::Relaxed);
        }
        drop(q);
        self.notify.notify_one();
    }

    pub fn enqueue_voice(&self, bytes: Bytes) {
        let mut q = self.queue.lock().expect("queue poisoned");
        q.push(
            0,
            0,
            false,
            Instant::now() + Duration::from_millis(200),
            bytes,
        );
        drop(q);
        self.notify.notify_one();
    }

    async fn run(self: Arc<Self>) {
        loop {
            let maybe = {
                let mut q = self.queue.lock().expect("queue poisoned");
                let dropped = q.drop_expired(Instant::now());
                if dropped > 0 {
                    self.stats
                        .drop_deadline
                        .fetch_add(dropped as u64, Ordering::Relaxed);
                }
                q.pop_next(Instant::now())
            };
            let Some(bytes) = maybe else {
                let _ = timeout(Duration::from_millis(10), self.notify.notified()).await;
                continue;
            };
            if self.conn.datagram_send_buffer_space() < bytes.len() {
                self.stats.blocked_events.fetch_add(1, Ordering::Relaxed);
            }
            if let Err(e) = self.conn.send_datagram_wait(bytes.clone()).await {
                self.stats.fatal_errors.fetch_add(1, Ordering::Relaxed);
                warn!("[gateway-egress] exiting: send_datagram_wait failed ({e:?})");
                return;
            }
        }
    }
}

#[async_trait::async_trait]
pub trait EgressDatagramTx: Send + Sync {
    async fn send(&self, bytes: Bytes) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_eviction_prefers_non_keyframes() {
        let mut q = VideoFrameQueue::new(2, 10);
        let d = Instant::now() + Duration::from_secs(1);
        q.push(1, 1, true, d, Bytes::from_static(b"a"));
        q.push(1, 2, false, d, Bytes::from_static(b"b"));
        q.push(1, 3, true, d, Bytes::from_static(b"c"));
        q.enforce_bounds();
        assert!(q.frames.contains_key(&(1, 1)));
        assert!(!q.frames.contains_key(&(1, 2)));
    }
}
