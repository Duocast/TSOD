#[derive(Clone, Debug)]
pub struct MetricsConfig {
    /// Bind address for Prometheus scrape endpoint.
    ///
    /// Defaults to loopback-only for safety. Set to 0.0.0.0:9100 (or another
    /// non-loopback address) to explicitly opt-in to remote scraping.
    pub listen: String,

    /// Optional namespace prefix, e.g. "vp"
    pub namespace: &'static str,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:9100".to_string(),
            namespace: "vp",
        }
    }
}
