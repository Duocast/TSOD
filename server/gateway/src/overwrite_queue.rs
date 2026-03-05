use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Mutex,
};

use bytes::Bytes;
use tokio::sync::Notify;
use tokio::time::{Duration, Instant};

pub type StampedBytes = (Instant, Bytes);

pub struct OverwriteQueue<T> {
    cap: usize,
    q: Mutex<VecDeque<T>>,
    notify: Notify,
    closed: AtomicBool,
    overflow_evictions_total: AtomicU64,
}

impl<T> OverwriteQueue<T> {
    pub fn new(cap: usize) -> Self {
        assert!(cap > 0, "overwrite queue cap must be > 0");
        Self {
            cap,
            q: Mutex::new(VecDeque::with_capacity(cap)),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
            overflow_evictions_total: AtomicU64::new(0),
        }
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    pub fn push(&self, item: T) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }

        let mut q = self.q.lock().expect("overwrite queue mutex poisoned");
        let was_empty = q.is_empty();
        if q.len() >= self.cap {
            q.pop_front();
            self.overflow_evictions_total
                .fetch_add(1, Ordering::Relaxed);
        }
        q.push_back(item);
        drop(q);

        if was_empty {
            self.notify.notify_one();
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        let q = self.q.lock().expect("overwrite queue mutex poisoned");
        q.len()
    }

    pub fn overflow_evictions_total(&self) -> u64 {
        self.overflow_evictions_total.load(Ordering::Relaxed)
    }

    pub fn overflow_evictions_swap(&self) -> u64 {
        self.overflow_evictions_total.swap(0, Ordering::Relaxed)
    }

    pub async fn pop_wait(&self) -> Option<T> {
        loop {
            if let Some(item) = {
                let mut q = self.q.lock().expect("overwrite queue mutex poisoned");
                q.pop_front()
            } {
                return Some(item);
            }

            if self.is_closed() {
                return None;
            }

            self.notify.notified().await;
        }
    }
}

pub async fn pop_voice_realtime(
    queue: &OverwriteQueue<StampedBytes>,
    max_age: Duration,
    keep_latest: usize,
    drain_trigger_age: Duration,
    stale_drops_total: &AtomicU64,
    drain_drops_total: &AtomicU64,
) -> Option<Bytes> {
    loop {
        let mut stale_drops = 0u64;
        let mut drain_drops = 0u64;
        let mut should_wait = false;
        let mut item = None;

        {
            let mut q = queue.q.lock().expect("overwrite queue mutex poisoned");
            if q.is_empty() {
                if queue.is_closed() {
                    return None;
                }
                should_wait = true;
            } else {
                let now = Instant::now();
                while let Some((enqueued_at, _)) = q.front() {
                    if now.duration_since(*enqueued_at) > max_age {
                        q.pop_front();
                        stale_drops = stale_drops.saturating_add(1);
                    } else {
                        break;
                    }
                }

                if !q.is_empty() {
                    let oldest_age = q
                        .front()
                        .map(|(enqueued_at, _)| now.duration_since(*enqueued_at))
                        .unwrap_or_default();
                    let trigger_drain =
                        q.len() >= (queue.cap().max(2) / 2) || oldest_age > drain_trigger_age;
                    if keep_latest > 0 && trigger_drain && q.len() > keep_latest {
                        let drop_count = q.len() - keep_latest;
                        for _ in 0..drop_count {
                            q.pop_front();
                        }
                        drain_drops = drop_count as u64;
                    }

                    while let Some((enqueued_at, _)) = q.front() {
                        if now.duration_since(*enqueued_at) > max_age {
                            q.pop_front();
                            stale_drops = stale_drops.saturating_add(1);
                        } else {
                            break;
                        }
                    }

                    item = q.pop_front();
                } else if queue.is_closed() {
                    return None;
                } else {
                    should_wait = true;
                }
            }
        }

        if stale_drops > 0 {
            stale_drops_total.fetch_add(stale_drops, Ordering::Relaxed);
        }
        if drain_drops > 0 {
            drain_drops_total.fetch_add(drain_drops, Ordering::Relaxed);
        }

        if let Some((enqueued_at, bytes)) = item {
            if Instant::now().duration_since(enqueued_at) > max_age {
                stale_drops_total.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            return Some(bytes);
        }

        if should_wait {
            queue.notify.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{advance, pause};

    #[tokio::test]
    async fn overwrite_queue_evicts_oldest_when_full() {
        let q = OverwriteQueue::new(2);
        q.push(1);
        q.push(2);
        q.push(3);

        assert_eq!(q.overflow_evictions_total(), 1);
        assert_eq!(q.pop_wait().await, Some(2));
        assert_eq!(q.pop_wait().await, Some(3));
    }

    #[tokio::test]
    async fn stall_consumer_keeps_processed_age_bounded() {
        pause();
        let q = OverwriteQueue::new(16);
        let max_age = Duration::from_millis(250);
        let stale = AtomicU64::new(0);
        let drain = AtomicU64::new(0);

        let mut enqueued = std::collections::HashMap::new();
        for i in 0..250u16 {
            enqueued.insert(i, Instant::now());
            q.push((Instant::now(), Bytes::from(i.to_le_bytes().to_vec())));
            advance(Duration::from_millis(20)).await;
        }

        assert!(q.overflow_evictions_total() > 0);

        let mut max_processed_age = Duration::ZERO;
        let mut processed = 0usize;
        while let Some(bytes) =
            pop_voice_realtime(&q, max_age, 4, max_age / 2, &stale, &drain).await
        {
            processed += 1;
            let seq = u16::from_le_bytes([bytes[0], bytes[1]]);
            let enqueued_at = enqueued
                .get(&seq)
                .copied()
                .expect("enqueued instant missing");
            let age = Instant::now().duration_since(enqueued_at);
            max_processed_age = max_processed_age.max(age);
            if q.len() == 0 {
                break;
            }
        }

        assert!(processed > 0);
        assert!(max_processed_age <= max_age + Duration::from_millis(10));
        assert!(q.len() <= 4);
    }
}
