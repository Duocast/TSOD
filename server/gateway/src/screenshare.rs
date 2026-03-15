use anyhow::Result;
use std::time::Instant;

use crate::screenshare_policy::ScreenSharePolicy;
use crate::state::{ShareMetadata, StreamSessionRegistry, StreamSessionOwnership};
use vp_control::ids::{ChannelId, UserId};
use vp_control::ControlError;

/// Validates that `user` is a member of `channel_id` and therefore authorized
/// to start a screen share in that channel.  Fails closed: if the member list
/// is unavailable (`None`) the request is denied.
pub fn validate_start_share_authorization(
    user: UserId,
    channel_id: ChannelId,
    channel_members: Option<&Vec<UserId>>,
) -> Result<()> {
    let is_member = channel_members
        .map(|members| members.contains(&user))
        .unwrap_or(false);
    if !is_member {
        return Err(
            ControlError::PermissionDenied("not a channel member; cannot start share").into(),
        );
    }
    Ok(())
}

/// Validates that `user` is the owner of the stream identified by `stream_id`.
/// Only the owner may perform mutating actions on their own share (e.g. stop).
pub fn validate_owner_action(
    registry: &StreamSessionRegistry,
    stream_id: &str,
    user: UserId,
) -> Result<()> {
    let ownership = registry
        .ownership_by_stream_id(stream_id)
        .ok_or(ControlError::InvalidArgument("unknown stream_id"))?;
    if ownership.owner_user_id != user {
        return Err(
            ControlError::PermissionDenied("only the stream owner may perform this action").into(),
        );
    }
    Ok(())
}

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
    use crate::proto::voiceplatform::v1 as pb;
    use crate::screenshare_policy::ScreenSharePolicy;
    use crate::state::{ShareMetadata, StreamSessionOwnership, StreamSessionRegistry};
    use vp_control::ids::{ChannelId, UserId};

    fn test_metadata() -> ShareMetadata {
        ShareMetadata {
            codec: pb::VideoCodec::Vp9 as i32,
            layers: vec![],
            has_audio: false,
        }
    }

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
                metadata: test_metadata(),
            },
        );
        registry
    }

    fn make_registry_with_owner(
        stream_id: &str,
        owner: UserId,
        channel: ChannelId,
        primary_tag: u64,
    ) -> StreamSessionRegistry {
        let mut registry = StreamSessionRegistry::new();
        registry.register(
            stream_id.to_string(),
            StreamSessionOwnership {
                primary_tag,
                fallback_tag: None,
                owner_user_id: owner,
                channel_id: channel,
                active_layer_ids: vec![0, 1],
                metadata: test_metadata(),
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

    // ── start-share authorization ──────────────────────────────────────

    #[test]
    fn start_share_allowed_for_channel_member() {
        let user = UserId::new();
        let channel = ChannelId::new();
        let members = vec![user, UserId::new()];
        validate_start_share_authorization(user, channel, Some(&members))
            .expect("member should be authorized");
    }

    #[test]
    fn start_share_denied_for_non_member() {
        let user = UserId::new();
        let channel = ChannelId::new();
        let members = vec![UserId::new()];
        let err = validate_start_share_authorization(user, channel, Some(&members))
            .expect_err("non-member should be denied");
        assert!(
            err.downcast_ref::<ControlError>()
                .map(|e| matches!(e, ControlError::PermissionDenied(_)))
                .unwrap_or(false),
            "expected PermissionDenied, got: {err:?}"
        );
    }

    #[test]
    fn start_share_denied_when_member_list_unavailable() {
        let user = UserId::new();
        let channel = ChannelId::new();
        let err = validate_start_share_authorization(user, channel, None)
            .expect_err("should fail closed when member list is None");
        assert!(
            err.downcast_ref::<ControlError>()
                .map(|e| matches!(e, ControlError::PermissionDenied(_)))
                .unwrap_or(false),
        );
    }

    // ── owner-only actions ─────────────────────────────────────────────

    #[test]
    fn owner_can_stop_own_share() {
        let owner = UserId::new();
        let channel = ChannelId::new();
        let registry = make_registry_with_owner("s1", owner, channel, 50);
        validate_owner_action(&registry, "s1", owner)
            .expect("owner should be allowed to stop own share");
    }

    #[test]
    fn non_owner_cannot_stop_someone_elses_share() {
        let owner = UserId::new();
        let imposter = UserId::new();
        let channel = ChannelId::new();
        let registry = make_registry_with_owner("s1", owner, channel, 50);
        let err = validate_owner_action(&registry, "s1", imposter)
            .expect_err("non-owner should be denied");
        assert!(
            err.downcast_ref::<ControlError>()
                .map(|e| matches!(e, ControlError::PermissionDenied(_)))
                .unwrap_or(false),
            "expected PermissionDenied, got: {err:?}"
        );
    }

    #[test]
    fn owner_action_on_unknown_stream_returns_invalid_argument() {
        let registry = StreamSessionRegistry::new();
        let err = validate_owner_action(&registry, "no-such-stream", UserId::new())
            .expect_err("unknown stream should error");
        assert!(
            err.downcast_ref::<ControlError>()
                .map(|e| matches!(e, ControlError::InvalidArgument(_)))
                .unwrap_or(false),
        );
    }

    // ── viewer access (recovery / keyframe) ────────────────────────────

    #[test]
    fn valid_viewer_can_request_recovery() {
        let owner = UserId::new();
        let viewer = UserId::new();
        let channel = ChannelId::new();
        let registry = make_registry_with_owner("s1", owner, channel, 60);
        let members = vec![owner, viewer];
        validate_viewer_access(&registry, "s1", viewer, Some(&members))
            .expect("channel member should have viewer access");
    }

    #[test]
    fn owner_has_implicit_viewer_access() {
        let owner = UserId::new();
        let channel = ChannelId::new();
        let registry = make_registry_with_owner("s1", owner, channel, 60);
        // Even with empty member list, owner passes.
        let members = vec![];
        validate_viewer_access(&registry, "s1", owner, Some(&members))
            .expect("owner should always have viewer access");
    }

    #[test]
    fn non_member_cannot_request_recovery() {
        let owner = UserId::new();
        let outsider = UserId::new();
        let channel = ChannelId::new();
        let registry = make_registry_with_owner("s1", owner, channel, 60);
        let members = vec![owner]; // outsider not in list
        let err = validate_viewer_access(&registry, "s1", outsider, Some(&members))
            .expect_err("non-member should be denied viewer access");
        assert!(
            err.downcast_ref::<ControlError>()
                .map(|e| matches!(e, ControlError::PermissionDenied(_)))
                .unwrap_or(false),
            "expected PermissionDenied, got: {err:?}"
        );
    }

    #[test]
    fn viewer_access_denied_when_member_list_unavailable() {
        let owner = UserId::new();
        let viewer = UserId::new();
        let channel = ChannelId::new();
        let registry = make_registry_with_owner("s1", owner, channel, 60);
        let err = validate_viewer_access(&registry, "s1", viewer, None)
            .expect_err("should fail closed when member list is None");
        assert!(
            err.downcast_ref::<ControlError>()
                .map(|e| matches!(e, ControlError::PermissionDenied(_)))
                .unwrap_or(false),
        );
    }
}
