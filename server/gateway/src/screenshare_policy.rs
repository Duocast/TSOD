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
    last_selected_layer: HashMap<(String, UserId), u8>,
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

        let key = (stream_id.to_string(), viewer);

        if let Some(last) = self.last_switch_at.get(&key) {
            if now.duration_since(*last) < Self::LAYER_SWITCH_COOLDOWN {
                if let Some(previous_layer) = self.last_selected_layer.get(&key).copied() {
                    return Ok(previous_layer);
                }
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

        self.last_switch_at.insert(key.clone(), now);
        self.last_selected_layer.insert(key, active);
        Ok(active)
    }
}

#[cfg(test)]
mod tests {
    use super::ScreenSharePolicy;
    use std::time::{Duration, Instant};
    use vp_control::ids::UserId;

    #[test]
    fn returns_previous_layer_during_cooldown_instead_of_error() {
        let mut policy = ScreenSharePolicy::default();
        let now = Instant::now();
        let stream_id = "stream-1";
        let viewer = UserId(uuid::Uuid::new_v4());

        let first = policy
            .resolve_layer(stream_id, viewer, 2, &[0, 1, 2], now)
            .expect("first selection should succeed");
        assert_eq!(first, 2);

        let second = policy
            .resolve_layer(
                stream_id,
                viewer,
                0,
                &[0, 1, 2],
                now + Duration::from_millis(100),
            )
            .expect("selection during cooldown should return previous layer");
        assert_eq!(second, 2);
    }

    #[test]
    fn allows_switch_after_cooldown_window() {
        let mut policy = ScreenSharePolicy::default();
        let now = Instant::now();
        let stream_id = "stream-2";
        let viewer = UserId(uuid::Uuid::new_v4());

        policy
            .resolve_layer(stream_id, viewer, 2, &[0, 1, 2], now)
            .expect("first selection should succeed");

        let switched = policy
            .resolve_layer(
                stream_id,
                viewer,
                0,
                &[0, 1, 2],
                now + Duration::from_millis(400),
            )
            .expect("switch should succeed after cooldown");
        assert_eq!(switched, 0);
    }
}
