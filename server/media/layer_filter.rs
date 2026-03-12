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
