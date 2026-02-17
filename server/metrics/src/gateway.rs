use metrics::{counter, histogram};

pub struct GatewayMetrics {
    ns: &'static str,
}

impl GatewayMetrics {
    pub fn new(namespace: &'static str) -> Self {
        Self { ns: namespace }
    }

    #[inline]
    pub fn conn_accepted(&self) {
        counter!(format!("{}_gateway_connections_total", self.ns)).increment(1);
    }

    #[inline]
    pub fn conn_closed(&self) {
        counter!(format!("{}_gateway_connections_closed_total", self.ns)).increment(1);
    }

    #[inline]
    pub fn auth_success(&self) {
        counter!(format!("{}_gateway_auth_success_total", self.ns)).increment(1);
    }

    #[inline]
    pub fn auth_failed(&self) {
        counter!(format!("{}_gateway_auth_failed_total", self.ns)).increment(1);
    }

    #[inline]
    pub fn control_msg_rx(&self, kind: &'static str) {
        counter!(format!("{}_gateway_control_rx_total", self.ns), "kind" => kind).increment(1);
    }

    #[inline]
    pub fn control_msg_tx(&self, kind: &'static str) {
        counter!(format!("{}_gateway_control_tx_total", self.ns), "kind" => kind).increment(1);
    }

    #[inline]
    pub fn handshake_seconds(&self, seconds: f64) {
        histogram!(format!("{}_gateway_handshake_seconds", self.ns)).record(seconds);
    }
}
