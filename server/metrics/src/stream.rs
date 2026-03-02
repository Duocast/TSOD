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
    rx_packets_name: &'static str,
    rx_bytes_name: &'static str,
    forwarded_name: &'static str,
    fanout_name: &'static str,
    forwarded_bytes_name: &'static str,
    drops_name: &'static str,
    frames_evicted_name: &'static str,
    recovery_requests_name: &'static str,
    #[allow(dead_code)]
    policy: LabelPolicy,
}

impl StreamMetricsImpl {
    pub fn new(namespace: &'static str, policy: LabelPolicy) -> Self {
        Self {
            rx_packets_name: Box::leak(
                format!("{namespace}_stream_rx_packets_total").into_boxed_str(),
            ),
            rx_bytes_name: Box::leak(format!("{namespace}_stream_rx_bytes_total").into_boxed_str()),
            forwarded_name: Box::leak(
                format!("{namespace}_stream_forwarded_total").into_boxed_str(),
            ),
            fanout_name: Box::leak(format!("{namespace}_stream_fanout").into_boxed_str()),
            forwarded_bytes_name: Box::leak(
                format!("{namespace}_stream_forwarded_bytes_total").into_boxed_str(),
            ),
            drops_name: Box::leak(format!("{namespace}_stream_drops_total").into_boxed_str()),
            frames_evicted_name: Box::leak(
                format!("{namespace}_stream_frames_evicted_total").into_boxed_str(),
            ),
            recovery_requests_name: Box::leak(
                format!("{namespace}_stream_recovery_requests_total").into_boxed_str(),
            ),
            policy,
        }
    }

    #[inline]
    pub fn rx_packet(&self) {
        counter!(self.rx_packets_name).increment(1);
    }

    #[inline]
    pub fn rx_bytes(&self, n: usize) {
        counter!(self.rx_bytes_name).increment(n as u64);
    }

    #[inline]
    pub fn forwarded(&self, fanout: usize) {
        counter!(self.forwarded_name).increment(1);
        histogram!(self.fanout_name).record(fanout as f64);
    }

    #[inline]
    pub fn forwarded_bytes(&self, n: usize) {
        counter!(self.forwarded_bytes_name).increment(n as u64);
    }

    #[inline]
    pub fn forwarded_bytes_codec(&self, n: usize, codec: i32) {
        counter!(self.forwarded_bytes_name, "codec" => codec_label(codec)).increment(n as u64);
    }

    #[inline]
    pub fn drop_reason(&self, reason: &'static str) {
        counter!(self.drops_name, "reason" => reason).increment(1);
    }

    #[inline]
    pub fn drop_reason_codec(&self, reason: &'static str, codec: i32) {
        counter!(self.drops_name, "reason" => reason, "codec" => codec_label(codec)).increment(1);
    }

    #[inline]
    pub fn frames_evicted(&self, count: usize) {
        counter!(self.frames_evicted_name).increment(count as u64);
    }

    #[inline]
    pub fn recovery_requests(&self) {
        counter!(self.recovery_requests_name).increment(1);
    }
}
