use metrics::{counter, histogram};

use crate::labels::LabelPolicy;

/// Metric names under: {ns}_voice_*
pub struct VoiceMetricsImpl {
    rx_packets_name: &'static str,
    rx_bytes_name: &'static str,
    forwarded_name: &'static str,
    fanout_name: &'static str,
    drops_name: &'static str,
    send_queue_drops_name: &'static str,
    rx_by_channel_name: &'static str,
    session_lookup_us_name: &'static str,
    recipient_enumeration_us_name: &'static str,
    packet_fanout_us_name: &'static str,
    handle_incoming_us_name: &'static str,
    policy: LabelPolicy,
}

impl VoiceMetricsImpl {
    pub fn new(namespace: &'static str, policy: LabelPolicy) -> Self {
        Self {
            rx_packets_name: Box::leak(
                format!("{namespace}_voice_rx_packets_total").into_boxed_str(),
            ),
            rx_bytes_name: Box::leak(format!("{namespace}_voice_rx_bytes_total").into_boxed_str()),
            forwarded_name: Box::leak(
                format!("{namespace}_voice_forwarded_total").into_boxed_str(),
            ),
            fanout_name: Box::leak(format!("{namespace}_voice_fanout").into_boxed_str()),
            drops_name: Box::leak(format!("{namespace}_voice_drops_total").into_boxed_str()),
            send_queue_drops_name: Box::leak(
                format!("{namespace}_voice_send_queue_drops_total").into_boxed_str(),
            ),
            rx_by_channel_name: Box::leak(
                format!("{namespace}_voice_rx_packets_by_channel_total").into_boxed_str(),
            ),
            session_lookup_us_name: Box::leak(
                format!("{namespace}_voice_session_lookup_us").into_boxed_str(),
            ),
            recipient_enumeration_us_name: Box::leak(
                format!("{namespace}_voice_recipient_enumeration_us").into_boxed_str(),
            ),
            packet_fanout_us_name: Box::leak(
                format!("{namespace}_voice_packet_fanout_us").into_boxed_str(),
            ),
            handle_incoming_us_name: Box::leak(
                format!("{namespace}_voice_handle_incoming_us").into_boxed_str(),
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
    pub fn drop_reason(&self, reason: &'static str) {
        counter!(self.drops_name, "reason" => reason).increment(1);
    }

    #[inline]
    pub fn enqueue_drop(&self) {
        self.drop_reason("send_queue_full");
        counter!(self.send_queue_drops_name).increment(1);
    }

    #[inline]
    pub fn per_channel_rx(&self, channel_route_hash: u32) {
        counter!(
            self.rx_by_channel_name,
            "ch" => self.policy.channel_bucket(channel_route_hash).into_static()
        )
        .increment(1);
    }

    #[inline]
    pub fn session_lookup_us(&self, micros: u64) {
        histogram!(self.session_lookup_us_name).record(micros as f64);
    }

    #[inline]
    pub fn recipient_enumeration_us(&self, micros: u64) {
        histogram!(self.recipient_enumeration_us_name).record(micros as f64);
    }

    #[inline]
    pub fn packet_fanout_us(&self, micros: u64) {
        histogram!(self.packet_fanout_us_name).record(micros as f64);
    }

    #[inline]
    pub fn handle_incoming_us(&self, micros: u64) {
        histogram!(self.handle_incoming_us_name).record(micros as f64);
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
        fn observe_session_lookup_us(&self, micros: u64);
        fn observe_recipient_enumeration_us(&self, micros: u64);
        fn observe_packet_fanout_us(&self, micros: u64);
        fn observe_handle_incoming_us(&self, micros: u64);
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
            self.enqueue_drop();
        }
        fn inc_forwarded(&self, fanout: usize) {
            self.forwarded(fanout);
        }
        fn observe_session_lookup_us(&self, micros: u64) {
            self.session_lookup_us(micros);
        }
        fn observe_recipient_enumeration_us(&self, micros: u64) {
            self.recipient_enumeration_us(micros);
        }
        fn observe_packet_fanout_us(&self, micros: u64) {
            self.packet_fanout_us(micros);
        }
        fn observe_handle_incoming_us(&self, micros: u64) {
            self.handle_incoming_us(micros);
        }
    }
}
