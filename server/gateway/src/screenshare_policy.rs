use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use anyhow::Result;
use vp_control::ids::UserId;
use vp_control::ControlError;

#[derive(Default)]
pub struct ScreenSharePolicy {
    last_switch_at: HashMap<(String, UserId), Instant>,
}

impl ScreenSharePolicy {
    const LAYER_SWITCH_COOLDOWN: Duration = Duration::from_millis(350);

    pub fn resolve_layer(
        &mut self,
        stream_id: &str,
        viewer: UserId,
        requested_layer_id: u32,
        available_layers: &[u8],
        now: Instant,
    ) -> Result<u8> {
        if requested_layer_id > 2 {
            return Err(ControlError::InvalidArgument("invalid preferred_layer_id").into());
        }
        if available_layers.is_empty() {
            return Err(ControlError::FailedPrecondition("stream has no available layers").into());
        }

        if let Some(last) = self.last_switch_at.get(&(stream_id.to_string(), viewer)) {
            if now.duration_since(*last) < Self::LAYER_SWITCH_COOLDOWN {
                return Err(ControlError::FailedPrecondition("layer switch cooldown").into());
            }
        }

        let requested = requested_layer_id as u8;
        let active = if available_layers.contains(&requested) {
            requested
        } else {
            // conservative fallback to best available not above requested, else lowest.
            available_layers
                .iter()
                .copied()
                .filter(|l| *l <= requested)
                .max()
                .unwrap_or_else(|| *available_layers.iter().min().unwrap_or(&0))
        };

        self.last_switch_at
            .insert((stream_id.to_string(), viewer), now);
        Ok(active)
    }
}
