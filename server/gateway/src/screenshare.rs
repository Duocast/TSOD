use anyhow::Result;
use std::time::Instant;

use crate::screenshare_policy::ScreenSharePolicy;
use crate::state::StreamSessionRegistry;
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

pub fn select_and_persist_layer(
    registry: &mut StreamSessionRegistry,
    policy: &mut ScreenSharePolicy,
    stream_id: &str,
    viewer: UserId,
    preferred_layer_id: u32,
) -> Result<(u8, u64)> {
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
    Ok((active_layer, ownership.primary_tag))
}
