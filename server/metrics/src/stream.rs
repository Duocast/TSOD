fn codec_label(codec: i32) -> &'static str {
    match codec {
        1 => "av1",
        2 => "vp9",
        3 => "vp8",
        _ => "unknown",
    }
}

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
    pub fn forwarded_bytes(&self, n: usize) {
        counter!(format!("{}_stream_forwarded_bytes_total", self.ns)).increment(n as u64);
    }

    #[inline]
    pub fn forwarded_bytes_codec(&self, n: usize, codec: i32) {
        counter!(
            format!("{}_stream_forwarded_bytes_total", self.ns),
            "codec" => codec_label(codec)
        )
        .increment(n as u64);
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
    pub fn drop_reason_codec(&self, reason: &'static str, codec: i32) {
        counter!(
            format!("{}_stream_drops_total", self.ns),
            "reason" => reason,
            "codec" => codec_label(codec)
        )
        .increment(1);
    }

    #[inline]
    pub fn frames_evicted(&self, count: usize) {
        counter!(format!("{}_stream_frames_evicted_total", self.ns)).increment(count as u64);
    }

    #[inline]
    pub fn recovery_requests(&self) {
        counter!(format!("{}_stream_recovery_requests_total", self.ns)).increment(1);
    }
}
