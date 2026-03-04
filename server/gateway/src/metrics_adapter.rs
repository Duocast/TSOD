use std::sync::Arc;

use vp_media::{
    datagram_send_policy::DatagramSendPolicyMetrics,
    stream_forwarder::{StreamDropReason, StreamMetrics},
    voice_forwarder::VoiceMetrics,
};
use vp_metrics::{labels::LabelPolicy, stream::StreamMetricsImpl, voice::VoiceMetricsImpl};

pub fn voice_metrics() -> Arc<dyn VoiceMetrics> {
    Arc::new(GatewayVoiceMetrics {
        inner: VoiceMetricsImpl::new("vp", LabelPolicy::default()),
    })
}

struct GatewayVoiceMetrics {
    inner: VoiceMetricsImpl,
}

impl VoiceMetrics for GatewayVoiceMetrics {
    fn inc_rx_packets(&self) {
        self.inner.rx_packet();
    }
    fn inc_rx_bytes(&self, n: usize) {
        self.inner.rx_bytes(n);
    }
    fn inc_drop_invalid(&self) {
        self.inner.drop_reason("invalid");
    }
    fn inc_drop_rate_limited(&self) {
        self.inner.drop_reason("rate_limited");
    }
    fn inc_drop_not_member(&self) {
        self.inner.drop_reason("not_member");
    }
    fn inc_drop_muted(&self) {
        self.inner.drop_reason("muted");
    }
    fn inc_drop_talker_limit(&self) {
        self.inner.drop_reason("talker_limit");
    }
    fn inc_drop_send_queue_full(&self) {
        self.inner.enqueue_drop();
    }
    fn inc_forwarded(&self, fanout: usize) {
        self.inner.forwarded(fanout);
    }
}

impl DatagramSendPolicyMetrics for GatewayVoiceMetrics {
    fn inc_no_datagrams(&self) {
        self.inner.drop_reason("no_datagrams");
    }
    fn inc_oversize_drop(&self) {
        self.inner.drop_reason("oversize_drop");
    }
    fn inc_conn_lost(&self) {
        self.inner.drop_reason("conn_lost");
    }
    fn inc_send_err_other(&self) {
        self.inner.drop_reason("send_err_other");
    }
    fn inc_prune_evt_dropped(&self) {
        self.inner.drop_reason("prune_evt_dropped");
    }
    fn inc_video_dropped_due_to_space(&self) {
        self.inner.drop_reason("video_dropped_due_to_space");
    }
}
pub fn stream_metrics() -> Arc<dyn StreamMetrics> {
    Arc::new(GatewayStreamMetrics {
        inner: StreamMetricsImpl::new("vp", LabelPolicy::default()),
    })
}

struct GatewayStreamMetrics {
    inner: StreamMetricsImpl,
}

impl StreamMetrics for GatewayStreamMetrics {
    fn inc_rx_packets(&self) {
        self.inner.rx_packet();
    }
    fn inc_rx_bytes(&self, n: usize) {
        self.inner.rx_bytes(n);
    }
    fn inc_drop_invalid(&self) {
        self.inner.drop_reason("invalid");
    }
    fn inc_drop_unauthorized(&self) {
        self.inner.drop_reason("unauthorized");
    }
    fn inc_drop_by_reason(&self, reason: StreamDropReason) {
        self.inner.drop_reason(reason.as_label());
    }
    fn inc_drop_by_reason_codec(&self, reason: StreamDropReason, codec: i32) {
        self.inner.drop_reason_codec(reason.as_label(), codec);
    }
    fn inc_forwarded(&self, fanout: usize) {
        self.inner.forwarded(fanout);
    }
    fn inc_forwarded_bytes(&self, n: usize) {
        self.inner.forwarded_bytes(n);
    }
    fn inc_forwarded_bytes_codec(&self, n: usize, codec: i32) {
        self.inner.forwarded_bytes_codec(n, codec);
    }
    fn inc_frames_evicted(&self, count: usize) {
        self.inner.frames_evicted(count);
    }
    fn inc_recovery_requests(&self) {
        self.inner.recovery_requests();
    }
}
