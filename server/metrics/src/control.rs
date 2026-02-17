use metrics::{counter, histogram};

pub struct ControlMetrics {
    ns: &'static str,
}

impl ControlMetrics {
    pub fn new(namespace: &'static str) -> Self {
        Self { ns: namespace }
    }

    pub fn op_total(&self, op: &'static str) {
        counter!(format!("{}_control_ops_total", self.ns), "op" => op).increment(1);
    }

    pub fn perm_denied(&self, cap: &'static str) {
        counter!(format!("{}_control_perm_denied_total", self.ns), "cap" => cap).increment(1);
    }

    pub fn db_seconds(&self, query: &'static str, seconds: f64) {
        histogram!(format!("{}_control_db_seconds", self.ns), "query" => query).record(seconds);
    }

    pub fn outbox_published(&self, topic: &'static str) {
        counter!(format!("{}_control_outbox_published_total", self.ns), "topic" => topic).increment(1);
    }

    pub fn outbox_lag_seconds(&self, seconds: f64) {
        histogram!(format!("{}_control_outbox_lag_seconds", self.ns)).record(seconds);
    }
}
