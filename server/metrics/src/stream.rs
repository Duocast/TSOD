use metrics::{counter, histogram};

use crate::labels::LabelPolicy;

/// Metric names under: {ns}_stream_*
pub struct StreamMetricsImpl {
    ns: &'static str,
    #[allow(dead_code)]
    policy: LabelPolicy,
}

impl StreamMetricsImpl {
    pub fn new(namespace: &'static str, policy: LabelPolicy) -> Self {
        Self {
            ns: namespace,
            policy,
        }
    }

    #[inline]
    pub fn rx_packet(&self) {
        counter!(format!("{}_stream_rx_packets_total", self.ns)).increment(1);
    }

    #[inline]
    pub fn rx_bytes(&self, n: usize) {
        counter!(format!("{}_stream_rx_bytes_total", self.ns)).increment(n as u64);
    }

    #[inline]
    pub fn forwarded(&self, fanout: usize) {
        counter!(format!("{}_stream_forwarded_total", self.ns)).increment(1);
        histogram!(format!("{}_stream_fanout", self.ns)).record(fanout as f64);
    }

    #[inline]
    pub fn drop_reason(&self, reason: &'static str) {
        counter!(
            format!("{}_stream_drops_total", self.ns),
            "reason" => reason
        )
        .increment(1);
    }

    #[inline]
    pub fn frames_evicted(&self, count: usize) {
        counter!(format!("{}_stream_frames_evicted_total", self.ns)).increment(count as u64);
    }
}
