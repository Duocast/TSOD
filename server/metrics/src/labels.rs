use once_cell::sync::Lazy;
use std::borrow::Cow;

/// A label value that is safe to export (bounded cardinality).
#[derive(Clone, Debug)]
pub struct BoundedLabel(Cow<'static, str>);

impl BoundedLabel {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_static(self) -> &'static str {
        match self.0 {
            Cow::Borrowed(s) => s,
            Cow::Owned(s) => Box::leak(s.into_boxed_str()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LabelPolicy {
    /// Maximum distinct channel buckets exported (e.g., top N channels by traffic).
    pub max_channel_buckets: usize,
}

impl Default for LabelPolicy {
    fn default() -> Self {
        Self {
            max_channel_buckets: 50,
        }
    }
}

const MAX_PREBUILT_CHANNEL_BUCKETS: usize = 256;
static CHANNEL_BUCKET_LABELS: Lazy<Vec<&'static str>> = Lazy::new(|| {
    (0..MAX_PREBUILT_CHANNEL_BUCKETS)
        .map(|bucket| Box::leak(format!("ch{bucket:02}").into_boxed_str()) as &'static str)
        .collect()
});

impl LabelPolicy {
    /// Bucket a channel into a bounded label.
    /// In production, you would drive this with a top-N structure updated periodically.
    /// For now we do a simple hash bucket to keep cardinality bounded.
    pub fn channel_bucket(&self, channel_route_hash: u32) -> BoundedLabel {
        let max_buckets = self
            .max_channel_buckets
            .max(1)
            .min(MAX_PREBUILT_CHANNEL_BUCKETS);
        let bucket = (channel_route_hash as usize) % max_buckets;
        BoundedLabel(Cow::Borrowed(CHANNEL_BUCKET_LABELS[bucket]))
    }

    pub fn reason(reason: &'static str) -> BoundedLabel {
        BoundedLabel(Cow::Borrowed(reason))
    }
}
