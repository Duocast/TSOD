#[derive(Clone, Debug)]
pub struct MetricsConfig {
    /// Bind address for Prometheus scrape endpoint, e.g. 0.0.0.0:9100
    pub listen: String,

    /// Optional namespace prefix, e.g. "vp"
    pub namespace: &'static str,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:9100".to_string(),
            namespace: "vp",
        }
    }
}
