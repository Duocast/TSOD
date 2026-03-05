use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use metrics::{counter, gauge};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tracing::debug;
use vp_media::datagram_send_policy::{now_ms, PruneReason, SessionSendCtx};
use vp_media::stream_forwarder::StreamForwarder;

use crate::state::Sessions;

const PRUNE_TICK_SECS: u64 = 20;
const MAX_BATCH: usize = 128;
const BACKPRESSURE_GRACE_MS: u64 = 3_000;

pub async fn run_pruner(
    sessions: Sessions,
    stream_forwarder: Arc<StreamForwarder>,
    wake_tx: mpsc::Sender<()>,
    mut wake_rx: mpsc::Receiver<()>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(PRUNE_TICK_SECS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = wake_rx.recv() => {}
            _ = tick.tick() => {}
            else => break,
        }

        let mut processed = 0usize;
        let candidates = sessions.pending_sessions(MAX_BATCH * 2);
        for (user_id, session_id, ctx) in candidates.into_iter().take(MAX_BATCH) {
            process_one(&sessions, &stream_forwarder, user_id, &session_id, &ctx).await;
            processed += 1;
        }

        if processed > 0 {
            counter!("vp_gateway_prune_processed_total").increment(processed as u64);
            tokio::task::yield_now().await;
            if sessions.has_pending() {
                let _ = wake_tx.try_send(());
            }
        }
    }
}

async fn process_one(
    sessions: &Sessions,
    stream_forwarder: &Arc<StreamForwarder>,
    user_id: vp_control::ids::UserId,
    session_id: &str,
    ctx: &Arc<SessionSendCtx>,
) {
    let e0 = ctx.prune.epoch.load(Ordering::Relaxed);
    let reason_u8 = ctx.prune.reason.load(Ordering::Relaxed);
    let reason = prune_reason_from_u8(reason_u8);

    counter!("vp_gateway_prune_reason_total", "reason" => reason_label(reason)).increment(1);

    match reason {
        PruneReason::Backpressure => {
            stream_forwarder
                .unregister_session(user_id, session_id)
                .await;
            let now = now_ms();
            let suspect = ctx.prune.suspect_since_ms.load(Ordering::Relaxed);
            if suspect == 0 {
                let _ = ctx.prune.suspect_since_ms.compare_exchange(
                    0,
                    now,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
            } else if now.saturating_sub(suspect) >= BACKPRESSURE_GRACE_MS {
                debug!(session_id, user_id = %user_id.0, "backpressure grace exceeded; keeping transport alive and waiting for definitive signal");
            }
        }
        PruneReason::ProtocolError
        | PruneReason::HeartbeatTimeout
        | PruneReason::TransportClosed => {
            stream_forwarder
                .unregister_session(user_id, session_id)
                .await;
            sessions.unregister(user_id, session_id);
            ctx.prune.suspect_since_ms.store(0, Ordering::Relaxed);
        }
    }

    if ctx.prune.epoch.load(Ordering::Relaxed) == e0 {
        ctx.prune.pending.store(false, Ordering::Relaxed);
        if reason.is_definitive() || reason == PruneReason::ProtocolError {
            ctx.prune
                .reason
                .store(PruneReason::Backpressure as u8, Ordering::Relaxed);
            ctx.prune.suspect_since_ms.store(0, Ordering::Relaxed);
        }
    }

    gauge!("vp_gateway_prune_pending_count").set(sessions.pending_count() as f64);
}

fn prune_reason_from_u8(v: u8) -> PruneReason {
    match v {
        4 => PruneReason::TransportClosed,
        3 => PruneReason::HeartbeatTimeout,
        2 => PruneReason::ProtocolError,
        _ => PruneReason::Backpressure,
    }
}

fn reason_label(reason: PruneReason) -> &'static str {
    match reason {
        PruneReason::Backpressure => "backpressure",
        PruneReason::ProtocolError => "protocol_error",
        PruneReason::HeartbeatTimeout => "heartbeat_timeout",
        PruneReason::TransportClosed => "transport_closed",
    }
}
