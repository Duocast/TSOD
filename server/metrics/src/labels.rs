use std::borrow::Cow;

/// A label value that is safe to export (bounded cardinality).
#[derive(Clone, Debug)]
pub struct BoundedLabel(Cow<'static, str>);

impl BoundedLabel {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct LabelPolicy {
    /// Maximum distinct channel buckets exported (e.g., top N channels by traffic).
    pub max_channel_buckets: usize,
}

impl Default for LabelPolicy {
    fn default() -> Self {
        Self { max_channel_buckets: 50 }
    }
}

impl LabelPolicy {
    /// Bucket a channel into a bounded label.
    /// In production, you would drive this with a top-N structure updated periodically.
    /// For now we do a simple hash bucket to keep cardinality bounded.
    pub fn channel_bucket(&self, channel_route_hash: u32) -> BoundedLabel {
        let bucket = (channel_route_hash as usize) % self.max_channel_buckets.max(1);
        BoundedLabel(Cow::Owned(format!("ch{:02}", bucket)))
    }

    pub fn reason(reason: &'static str) -> BoundedLabel {
        BoundedLabel(Cow::Borrowed(reason))
    }
}
