use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy)]
pub struct RecoveryPolicyConfig {
    pub freeze_threshold: Duration,
    pub keyframe_cooldown: Duration,
    pub recovery_cooldown: Duration,
    pub recovery_escalation_intervals: u32,
}

impl Default for RecoveryPolicyConfig {
    fn default() -> Self {
        Self {
            freeze_threshold: Duration::from_millis(200),
            keyframe_cooldown: Duration::from_millis(500),
            recovery_cooldown: Duration::from_millis(1750),
            recovery_escalation_intervals: 2,
        }
    }
}

#[derive(Debug, Clone)]
struct StreamRecoveryState {
    last_frame_received_at: Instant,
    last_keyframe_request_at: Option<Instant>,
    last_recovery_request_at: Option<Instant>,
    consecutive_freeze_intervals: u32,
    unanswered_keyframe_requests: u32,
}

impl StreamRecoveryState {
    fn new(now: Instant) -> Self {
        Self {
            last_frame_received_at: now,
            last_keyframe_request_at: None,
            last_recovery_request_at: None,
            consecutive_freeze_intervals: 0,
            unanswered_keyframe_requests: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RecoveryTickActions {
    pub request_keyframe: bool,
    pub request_recovery: bool,
}

pub struct ViewerRecoveryPolicy {
    cfg: RecoveryPolicyConfig,
    streams: HashMap<u64, StreamRecoveryState>,
}

impl ViewerRecoveryPolicy {
    pub fn new(cfg: RecoveryPolicyConfig) -> Self {
        Self {
            cfg,
            streams: HashMap::new(),
        }
    }

    pub fn register_stream(&mut self, stream_tag: u64, now: Instant) {
        self.streams
            .entry(stream_tag)
            .or_insert_with(|| StreamRecoveryState::new(now));
    }

    pub fn unregister_stream(&mut self, stream_tag: u64) {
        self.streams.remove(&stream_tag);
    }

    pub fn on_frame_received(&mut self, stream_tag: u64, now: Instant) {
        let state = self
            .streams
            .entry(stream_tag)
            .or_insert_with(|| StreamRecoveryState::new(now));
        state.last_frame_received_at = now;
        state.consecutive_freeze_intervals = 0;
        state.unanswered_keyframe_requests = 0;
    }

    pub fn evaluate_stream(&mut self, stream_tag: u64, now: Instant) -> RecoveryTickActions {
        let state = self
            .streams
            .entry(stream_tag)
            .or_insert_with(|| StreamRecoveryState::new(now));

        if now.duration_since(state.last_frame_received_at) < self.cfg.freeze_threshold {
            state.consecutive_freeze_intervals = 0;
            return RecoveryTickActions::default();
        }

        state.consecutive_freeze_intervals = state.consecutive_freeze_intervals.saturating_add(1);
        let mut actions = RecoveryTickActions::default();

        let keyframe_ready = state
            .last_keyframe_request_at
            .map(|ts| now.duration_since(ts) >= self.cfg.keyframe_cooldown)
            .unwrap_or(true);
        if keyframe_ready {
            state.last_keyframe_request_at = Some(now);
            state.unanswered_keyframe_requests =
                state.unanswered_keyframe_requests.saturating_add(1);
            actions.request_keyframe = true;
        }

        let recovery_ready = state
            .last_recovery_request_at
            .map(|ts| now.duration_since(ts) >= self.cfg.recovery_cooldown)
            .unwrap_or(true);
        let should_escalate = state.unanswered_keyframe_requests > 0
            && state.consecutive_freeze_intervals >= self.cfg.recovery_escalation_intervals;
        if recovery_ready && should_escalate {
            state.last_recovery_request_at = Some(now);
            actions.request_recovery = true;
        }

        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_before_recovery() {
        let now = Instant::now();
        let mut policy = ViewerRecoveryPolicy::new(RecoveryPolicyConfig::default());
        policy.register_stream(7, now - Duration::from_secs(1));

        let first = policy.evaluate_stream(7, now);
        assert!(first.request_keyframe);
        assert!(!first.request_recovery);

        let second = policy.evaluate_stream(7, now + Duration::from_millis(250));
        assert!(!second.request_keyframe);
        assert!(!second.request_recovery);

        let third = policy.evaluate_stream(7, now + Duration::from_millis(500));
        assert!(third.request_keyframe);
        assert!(third.request_recovery);
    }
}
