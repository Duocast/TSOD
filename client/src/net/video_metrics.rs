#[derive(Debug, Default, Clone)]
pub struct VideoMetrics {
    pub encoded_frames: u64,
    pub decoded_frames: u64,
    pub keyframes_requested: u64,
    pub bitrate_updates: u64,
}

impl VideoMetrics {
    pub fn record_encoded_frame(&mut self) {
        self.encoded_frames = self.encoded_frames.saturating_add(1);
    }

    pub fn record_decoded_frame(&mut self) {
        self.decoded_frames = self.decoded_frames.saturating_add(1);
    }

    pub fn record_keyframe_request(&mut self) {
        self.keyframes_requested = self.keyframes_requested.saturating_add(1);
    }

    pub fn record_bitrate_update(&mut self) {
        self.bitrate_updates = self.bitrate_updates.saturating_add(1);
    }
}
