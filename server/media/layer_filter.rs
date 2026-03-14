use std::collections::HashMap;

use vp_control::ids::UserId;

#[derive(Default)]
pub struct LayerFilter {
    preferred_by_viewer: HashMap<(u64, UserId), u8>,
}

impl LayerFilter {
    pub fn set_preferred_layer(&mut self, stream_tag: u64, viewer: UserId, layer_id: u8) {
        self.preferred_by_viewer
            .insert((stream_tag, viewer), layer_id);
    }

    pub fn remove_stream(&mut self, stream_tag: u64) {
        self.preferred_by_viewer
            .retain(|(tag, _), _| *tag != stream_tag);
    }

    pub fn preferred_layer(&self, stream_tag: u64, viewer: UserId) -> Option<u8> {
        self.preferred_by_viewer.get(&(stream_tag, viewer)).copied()
    }

    pub fn should_forward(
        &self,
        stream_tag: u64,
        viewer: UserId,
        datagram_layer_id: u8,
        is_priority: bool,
    ) -> bool {
        let preferred = self.preferred_layer(stream_tag, viewer).unwrap_or(0);
        datagram_layer_id == preferred || is_priority
    }
}

#[cfg(test)]
mod tests {
    use super::LayerFilter;
    use vp_control::ids::UserId;

    #[test]
    fn selection_affects_forwarding() {
        let viewer = UserId::new();
        let mut filter = LayerFilter::default();
        filter.set_preferred_layer(7, viewer, 2);

        assert!(!filter.should_forward(7, viewer, 1, false));
        assert!(filter.should_forward(7, viewer, 2, false));
        assert!(filter.should_forward(7, viewer, 1, true));
    }

    #[test]
    fn primary_only_preference_applied() {
        let viewer = UserId::new();
        let primary_tag = 100u64;
        let mut filter = LayerFilter::default();

        // Simulate what gateway does: apply preference to the single returned tag
        filter.set_preferred_layer(primary_tag, viewer, 1);

        assert!(filter.should_forward(primary_tag, viewer, 1, false));
        assert!(!filter.should_forward(primary_tag, viewer, 0, false));
    }

    #[test]
    fn fallback_viewer_gets_correct_layer_when_preference_applied_to_both_tags() {
        let viewer = UserId::new();
        let primary_tag = 200u64;
        let fallback_tag = 201u64;
        let mut filter = LayerFilter::default();

        // Simulate gateway applying preference to both primary and fallback tags
        filter.set_preferred_layer(primary_tag, viewer, 2);
        filter.set_preferred_layer(fallback_tag, viewer, 2);

        // Viewer on primary stream gets correct layer
        assert!(filter.should_forward(primary_tag, viewer, 2, false));
        assert!(!filter.should_forward(primary_tag, viewer, 1, false));

        // Viewer on fallback stream also gets correct layer (was broken before fix)
        assert!(filter.should_forward(fallback_tag, viewer, 2, false));
        assert!(!filter.should_forward(fallback_tag, viewer, 1, false));
    }

    #[test]
    fn fallback_viewer_without_preference_defaults_to_layer_zero() {
        let viewer = UserId::new();
        let fallback_tag = 300u64;
        let filter = LayerFilter::default();

        // No preference set → defaults to layer 0
        assert!(filter.should_forward(fallback_tag, viewer, 0, false));
        assert!(!filter.should_forward(fallback_tag, viewer, 1, false));
    }

    #[test]
    fn remove_stream_clears_both_primary_and_fallback_preferences() {
        let viewer = UserId::new();
        let primary_tag = 400u64;
        let fallback_tag = 401u64;
        let mut filter = LayerFilter::default();

        filter.set_preferred_layer(primary_tag, viewer, 2);
        filter.set_preferred_layer(fallback_tag, viewer, 2);

        filter.remove_stream(primary_tag);
        filter.remove_stream(fallback_tag);

        // After removal, defaults back to layer 0
        assert!(filter.should_forward(primary_tag, viewer, 0, false));
        assert!(filter.should_forward(fallback_tag, viewer, 0, false));
    }
}
