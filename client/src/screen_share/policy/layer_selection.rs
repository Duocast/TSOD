use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct ViewerLayerSignals {
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub loss_rate: f32,
    pub decode_error_rate: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct LayerSelectionTick {
    pub preferred_layer_id: u32,
    pub request_keyframe: bool,
}

pub fn select_active_share_layer(
    requested_layer_id: u32,
    accepted_layer_ids: &[u32],
) -> Option<u32> {
    if accepted_layer_ids.contains(&requested_layer_id) {
        return Some(requested_layer_id);
    }
    if accepted_layer_ids.contains(&1) {
        return Some(1);
    }
    None
}

pub struct ViewerLayerSelectionPolicy {
    current_layer_id: u32,
    last_upshift_at: Option<Instant>,
}

impl ViewerLayerSelectionPolicy {
    const UPSHIFT_COOLDOWN: Duration = Duration::from_secs(3);

    pub fn new(initial_layer_id: u32) -> Self {
        Self {
            current_layer_id: initial_layer_id,
            last_upshift_at: None,
        }
    }

    pub fn evaluate(&mut self, now: Instant, signals: ViewerLayerSignals) -> LayerSelectionTick {
        let target_layer = ideal_layer_from_signals(signals);

        let mut next_layer = self.current_layer_id;
        if target_layer < self.current_layer_id {
            // Downshift fast.
            next_layer = target_layer;
        } else if target_layer > self.current_layer_id {
            // Upshift slowly.
            let can_upshift = self
                .last_upshift_at
                .map(|at| now.duration_since(at) >= Self::UPSHIFT_COOLDOWN)
                .unwrap_or(true);
            if can_upshift {
                next_layer = self.current_layer_id.saturating_add(1).min(target_layer);
                self.last_upshift_at = Some(now);
            }
        }

        let changed = next_layer != self.current_layer_id;
        self.current_layer_id = next_layer;

        LayerSelectionTick {
            preferred_layer_id: self.current_layer_id,
            request_keyframe: changed,
        }
    }
}

fn ideal_layer_from_signals(signals: ViewerLayerSignals) -> u32 {
    let viewport_pixels = signals
        .viewport_width
        .saturating_mul(signals.viewport_height);

    let mut layer = if viewport_pixels >= 2_560 * 1_440 {
        2
    } else if viewport_pixels >= 1_280 * 720 {
        1
    } else {
        0
    };

    if signals.loss_rate >= 0.08 || signals.decode_error_rate >= 0.08 {
        layer = 0;
    } else if signals.loss_rate >= 0.03 || signals.decode_error_rate >= 0.03 {
        layer = layer.min(1);
    }

    layer
}
