use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use crate::net::video_transport::VideoStreamProfile;

#[derive(Debug, Clone, Copy)]
pub struct LayerBitrateHint {
    pub layer_id: u32,
    pub target_bitrate_bps: u32,
}

pub fn bitrate_hint_for_layer(layer_id: u32) -> LayerBitrateHint {
    let target_bitrate_bps = match layer_id {
        2 => 8_000_000,
        1 => 3_500_000,
        _ => 1_500_000,
    };
    LayerBitrateHint {
        layer_id,
        target_bitrate_bps,
    }
}

#[derive(Clone)]
pub struct BitrateController {
    target_bps: Arc<AtomicU32>,
}

impl BitrateController {
    pub fn new(profile: VideoStreamProfile, layer_id: u8) -> Self {
        let base = match profile {
            VideoStreamProfile::P1080p60 => 4_000_000,
            VideoStreamProfile::P1440p60 => 8_000_000,
        };
        let layer = bitrate_hint_for_layer(layer_id as u32).target_bitrate_bps;
        Self {
            target_bps: Arc::new(AtomicU32::new(base.min(layer).max(1_000_000))),
        }
    }

    pub fn current_target_bps(&self) -> u32 {
        self.target_bps.load(Ordering::Relaxed)
    }

    pub fn set_layer(&self, layer_id: u8) {
        let hint = bitrate_hint_for_layer(layer_id as u32).target_bitrate_bps;
        self.target_bps.store(hint, Ordering::Relaxed);
    }

    pub fn apply_network_feedback(&self, measured_egress_bps: u32) -> u32 {
        let current = self.current_target_bps();
        let next = if measured_egress_bps < current / 2 {
            (current as f32 * 0.8) as u32
        } else if measured_egress_bps > current {
            (current as f32 * 1.05) as u32
        } else {
            current
        }
        .clamp(750_000, 20_000_000);
        self.target_bps.store(next, Ordering::Relaxed);
        next
    }
}
