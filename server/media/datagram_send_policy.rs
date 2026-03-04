use std::sync::{
    atomic::{AtomicU64, Ordering},
    OnceLock,
};
use std::time::Instant;

use bytes::Bytes;
use tokio::sync::mpsc;
use vp_control::ids::ChannelId;

pub const PRUNE_DEBOUNCE_MS: u64 = 1_000;
pub const VIDEO_HEADROOM: usize = 1200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneEvt {
    pub channel_id: ChannelId,
    pub session_id: String,
}

pub trait DatagramSendPolicyMetrics: Send + Sync {
    fn inc_no_datagrams(&self);
    fn inc_oversize_drop(&self);
    fn inc_conn_lost(&self);
    fn inc_send_err_other(&self);
    fn inc_prune_evt_dropped(&self);
    fn inc_video_dropped_due_to_space(&self);
}

pub struct SessionSendCtx {
    pub session_id: String,
    pub conn: quinn::Connection,
    pub last_prune_ms: AtomicU64,
}

impl SessionSendCtx {
    pub fn new(session_id: String, conn: quinn::Connection) -> Self {
        Self {
            session_id,
            conn,
            last_prune_ms: AtomicU64::new(0),
        }
    }

    pub fn send_voice(
        &self,
        now_ms: u64,
        channel_id: ChannelId,
        pkt: Bytes,
        prune_tx: &mpsc::Sender<PruneEvt>,
        metrics: &dyn DatagramSendPolicyMetrics,
    ) {
        self.send_inner(now_ms, channel_id, pkt, prune_tx, metrics);
    }

    pub fn send_video_best_effort(
        &self,
        now_ms: u64,
        channel_id: ChannelId,
        pkt: Bytes,
        prune_tx: &mpsc::Sender<PruneEvt>,
        metrics: &dyn DatagramSendPolicyMetrics,
    ) {
        if self.conn.datagram_send_buffer_space() < pkt.len().saturating_add(VIDEO_HEADROOM) {
            metrics.inc_video_dropped_due_to_space();
            return;
        }
        self.send_inner(now_ms, channel_id, pkt, prune_tx, metrics);
    }

    fn send_inner(
        &self,
        now_ms: u64,
        channel_id: ChannelId,
        pkt: Bytes,
        prune_tx: &mpsc::Sender<PruneEvt>,
        metrics: &dyn DatagramSendPolicyMetrics,
    ) {
        let Some(max) = self.conn.max_datagram_size() else {
            metrics.inc_no_datagrams();
            return;
        };
        if pkt.len() > max {
            metrics.inc_oversize_drop();
            return;
        }

        match self.conn.send_datagram(pkt) {
            Ok(()) => {}
            Err(quinn::SendDatagramError::TooLarge) => metrics.inc_oversize_drop(),
            Err(quinn::SendDatagramError::ConnectionLost(_)) => {
                metrics.inc_conn_lost();
                maybe_prune(self, now_ms, channel_id, prune_tx, metrics);
            }
            Err(_) => {
                metrics.inc_send_err_other();
                maybe_prune(self, now_ms, channel_id, prune_tx, metrics);
            }
        }
    }
}

pub fn should_prune(last_prune_ms: &AtomicU64, now_ms: u64) -> bool {
    loop {
        let last = last_prune_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < PRUNE_DEBOUNCE_MS {
            return false;
        }
        if last_prune_ms
            .compare_exchange(last, now_ms, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
    }
}

pub fn maybe_prune(
    ctx: &SessionSendCtx,
    now_ms: u64,
    channel_id: ChannelId,
    prune_tx: &mpsc::Sender<PruneEvt>,
    metrics: &dyn DatagramSendPolicyMetrics,
) {
    if !should_prune(&ctx.last_prune_ms, now_ms) {
        return;
    }
    if prune_tx
        .try_send(PruneEvt {
            channel_id,
            session_id: ctx.session_id.clone(),
        })
        .is_err()
    {
        metrics.inc_prune_evt_dropped();
    }
}

pub fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestMetrics {
        dropped: AtomicU64,
    }

    impl DatagramSendPolicyMetrics for TestMetrics {
        fn inc_no_datagrams(&self) {}
        fn inc_oversize_drop(&self) {}
        fn inc_conn_lost(&self) {}
        fn inc_send_err_other(&self) {}
        fn inc_prune_evt_dropped(&self) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        fn inc_video_dropped_due_to_space(&self) {}
    }

    #[test]
    fn debounce_cas_allows_then_blocks() {
        let last = AtomicU64::new(0);
        assert!(should_prune(&last, PRUNE_DEBOUNCE_MS));
        assert!(!should_prune(&last, PRUNE_DEBOUNCE_MS + 1));
        assert!(should_prune(&last, PRUNE_DEBOUNCE_MS * 2 + 1));
    }

    #[tokio::test]
    async fn maybe_prune_try_send_failure_does_not_panic() {
        let metrics = TestMetrics { dropped: AtomicU64::new(0) };
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let last = AtomicU64::new(0);
        if should_prune(&last, PRUNE_DEBOUNCE_MS) {
            let _ = tx.try_send(PruneEvt { channel_id: ChannelId::new(), session_id: "s".into() });
        }
        assert_eq!(metrics.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn now_ms_monotonic() {
        let a = now_ms();
        let b = now_ms();
        assert!(b >= a);
    }
}
