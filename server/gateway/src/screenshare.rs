use anyhow::Result;
use std::time::Instant;

use crate::screenshare_policy::ScreenSharePolicy;
use crate::state::{StreamSessionRegistry, StreamSessionOwnership};
use vp_control::ids::UserId;
use vp_control::ControlError;

pub fn validate_viewer_access(
    registry: &StreamSessionRegistry,
    stream_id: &str,
    viewer: UserId,
    channel_members: Option<&Vec<UserId>>,
) -> Result<()> {
    let ownership = registry
        .ownership_by_stream_id(stream_id)
        .ok_or(ControlError::InvalidArgument("unknown stream_id"))?;
    if ownership.owner_user_id == viewer {
        return Ok(());
    }
    let is_member = channel_members
        .map(|members| members.contains(&viewer))
        .unwrap_or(false);
    if !is_member {
        return Err(ControlError::PermissionDenied("viewer not allowed for stream").into());
    }
    Ok(())
}

/// Returns `(active_layer_id, stream_tags)` where `stream_tags` contains the primary tag and,
/// if present, the fallback tag. Callers must apply the layer preference and route recovery
/// signals to **all** returned tags so that viewers on either stream receive correct behavior.
pub fn select_and_persist_layer(
    registry: &mut StreamSessionRegistry,
    policy: &mut ScreenSharePolicy,
    stream_id: &str,
    viewer: UserId,
    preferred_layer_id: u32,
) -> Result<(u8, Vec<u64>)> {
    let ownership = registry
        .ownership_by_stream_id(stream_id)
        .ok_or(ControlError::InvalidArgument("unknown stream_id"))?
        .clone();

    let active_layer = policy.resolve_layer(
        stream_id,
        viewer,
        preferred_layer_id,
        &ownership.active_layer_ids,
        Instant::now(),
    )?;
    registry.set_viewer_preferred_layer(stream_id, viewer, active_layer);
    Ok((active_layer, all_tags(&ownership)))
}

/// Collects the primary tag and optional fallback tag into a Vec.
fn all_tags(ownership: &StreamSessionOwnership) -> Vec<u64> {
    let mut tags = vec![ownership.primary_tag];
    if let Some(tag) = ownership.fallback_tag {
        tags.push(tag);
    }
    tags
}

pub fn should_request_keyframe_on_layer_change(
    previous_layer: Option<u8>,
    active_layer: u8,
) -> bool {
    previous_layer != Some(active_layer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screenshare_policy::ScreenSharePolicy;
    use crate::state::{StreamSessionOwnership, StreamSessionRegistry};
    use vp_control::ids::{ChannelId, UserId};

    fn make_registry(primary_tag: u64, fallback_tag: Option<u64>) -> StreamSessionRegistry {
        let mut registry = StreamSessionRegistry::new();
        registry.register(
            "stream-x".to_string(),
            StreamSessionOwnership {
                primary_tag,
                fallback_tag,
                owner_user_id: UserId::new(),
                channel_id: ChannelId::new(),
                active_layer_ids: vec![0, 1, 2],
            },
        );
        registry
    }

    #[test]
    fn keyframe_requested_when_layer_changes() {
        assert!(should_request_keyframe_on_layer_change(Some(0), 1));
        assert!(should_request_keyframe_on_layer_change(None, 0));
        assert!(!should_request_keyframe_on_layer_change(Some(2), 2));
    }

    #[test]
    fn primary_only_returns_single_tag() {
        let mut registry = make_registry(42, None);
        let mut policy = ScreenSharePolicy::default();
        let viewer = UserId::new();

        let (layer, tags) = select_and_persist_layer(
            &mut registry,
            &mut policy,
            "stream-x",
            viewer,
            1,
        )
        .expect("should succeed");

        assert_eq!(layer, 1);
        assert_eq!(tags, vec![42]);
    }

    #[test]
    fn primary_and_fallback_returns_both_tags() {
        let mut registry = make_registry(10, Some(11));
        let mut policy = ScreenSharePolicy::default();
        let viewer = UserId::new();

        let (layer, tags) = select_and_persist_layer(
            &mut registry,
            &mut policy,
            "stream-x",
            viewer,
            2,
        )
        .expect("should succeed");

        assert_eq!(layer, 2);
        assert_eq!(tags, vec![10, 11]);
    }

    #[test]
    fn layer_preference_persisted_for_stream_id() {
        let mut registry = make_registry(10, Some(11));
        let mut policy = ScreenSharePolicy::default();
        let viewer = UserId::new();

        select_and_persist_layer(&mut registry, &mut policy, "stream-x", viewer, 2)
            .expect("should succeed");

        assert_eq!(registry.viewer_preferred_layer("stream-x", viewer), Some(2));
    }
}
