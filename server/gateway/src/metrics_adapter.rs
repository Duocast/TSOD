use std::sync::Arc;

use vp_media::voice_forwarder::VoiceMetrics;
use vp_metrics::{labels::LabelPolicy, voice::VoiceMetricsImpl};

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
        self.inner.drop_reason("send_queue_full");
    }
    fn inc_forwarded(&self, fanout: usize) {
        self.inner.forwarded(fanout);
    }
}
