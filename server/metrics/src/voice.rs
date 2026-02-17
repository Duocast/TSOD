use metrics::{counter, histogram};

use crate::labels::LabelPolicy;

/// Metric names under: {ns}_voice_*
pub struct VoiceMetricsImpl {
    ns: &'static str,
    policy: LabelPolicy,
}

impl VoiceMetricsImpl {
    pub fn new(namespace: &'static str, policy: LabelPolicy) -> Self {
        Self { ns: namespace, policy }
    }

    #[inline]
    pub fn rx_packet(&self) {
        counter!(format!("{}_voice_rx_packets_total", self.ns)).increment(1);
    }

    #[inline]
    pub fn rx_bytes(&self, n: usize) {
        counter!(format!("{}_voice_rx_bytes_total", self.ns)).increment(n as u64);
    }

    #[inline]
    pub fn forwarded(&self, fanout: usize) {
        counter!(format!("{}_voice_forwarded_total", self.ns)).increment(1);
        histogram!(format!("{}_voice_fanout", self.ns)).record(fanout as f64);
    }

    #[inline]
    pub fn drop_reason(&self, reason: &'static str) {
        counter!(
            format!("{}_voice_drops_total", self.ns),
            "reason" => self.policy.reason(reason).as_str().to_string()
        )
        .increment(1);
    }

    #[inline]
    pub fn enqueue_drop(&self) {
        self.drop_reason("send_queue_full");
    }

    #[inline]
    pub fn per_channel_rx(&self, channel_route_hash: u32) {
        counter!(
            format!("{}_voice_rx_packets_by_channel_total", self.ns),
            "ch" => self.policy.channel_bucket(channel_route_hash).as_str().to_string()
        )
        .increment(1);
    }
}

/// Adapter implementing the `VoiceMetrics` trait used by voice_forwarder.rs
/// Copy/paste the trait path to match your module.
pub mod adapter {
    use super::VoiceMetricsImpl;

    // This must match the trait in your media/voice_forwarder.rs.
    // If your path is different, adjust imports.
    pub trait VoiceMetrics: Send + Sync {
        fn inc_rx_packets(&self);
        fn inc_rx_bytes(&self, n: usize);
        fn inc_drop_invalid(&self);
        fn inc_drop_rate_limited(&self);
        fn inc_drop_not_member(&self);
        fn inc_drop_muted(&self);
        fn inc_drop_talker_limit(&self);
        fn inc_drop_send_queue_full(&self);
        fn inc_forwarded(&self, fanout: usize);
    }

    impl VoiceMetrics for VoiceMetricsImpl {
        fn inc_rx_packets(&self) {
            self.rx_packet();
        }
        fn inc_rx_bytes(&self, n: usize) {
            self.rx_bytes(n);
        }
        fn inc_drop_invalid(&self) {
            self.drop_reason("invalid");
        }
        fn inc_drop_rate_limited(&self) {
            self.drop_reason("rate_limited");
        }
        fn inc_drop_not_member(&self) {
            self.drop_reason("not_member");
        }
        fn inc_drop_muted(&self) {
            self.drop_reason("muted");
        }
        fn inc_drop_talker_limit(&self) {
            self.drop_reason("talker_limit");
        }
        fn inc_drop_send_queue_full(&self) {
            self.drop_reason("send_queue_full");
        }
        fn inc_forwarded(&self, fanout: usize) {
            self.forwarded(fanout);
        }
    }
}
