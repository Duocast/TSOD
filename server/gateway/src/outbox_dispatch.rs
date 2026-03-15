use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::proto::voiceplatform::v1 as pb;
use crate::state::{MembershipCache, PushHub};

use vp_control::ids::{ChannelId, MessageId, ServerId, UserId};
use vp_control::model::OutboxEventRow;
use vp_control::{ControlRepo, PgControlRepo};

pub struct OutboxDispatcherConfig {
    pub server_id: ServerId,
    pub poll_interval: Duration,
    pub batch_size: i64,
    pub claim_ttl_seconds: i64,
}

pub async fn run_outbox_dispatcher(
    repo: PgControlRepo,
    hub: PushHub,
    membership: MembershipCache,
    cfg: OutboxDispatcherConfig,
) -> Result<()> {
    let token = uuid::Uuid::new_v4();
    info!(claim_token = %token, server_id = %cfg.server_id.0, ttl_s = cfg.claim_ttl_seconds, "outbox dispatcher started");

    loop {
        let mut tx = repo.tx().await.context("outbox tx")?;
        let batch = <PgControlRepo as ControlRepo>::claim_outbox_batch(
            &repo,
            &mut tx,
            cfg.server_id,
            token,
            cfg.batch_size,
        )
        .await
        .context("claim_outbox_batch")?;
        tx.commit().await.context("outbox tx commit")?;

        if batch.is_empty() {
            sleep(cfg.poll_interval).await;
            continue;
        }

        debug!(server_id=%cfg.server_id.0, claimed=batch.len(), "claimed outbox rows");

        for rec in batch {
            if let Err(e) = handle_record(&repo, &hub, &membership, token, rec).await {
                warn!("outbox record handling error: {:#}", e);
                // do not ack; it'll be reclaimed after TTL
            }
        }
    }
}

async fn handle_record(
    repo: &PgControlRepo,
    hub: &PushHub,
    membership: &MembershipCache,
    token: uuid::Uuid,
    rec: OutboxEventRow,
) -> Result<()> {
    let (channel_id, push) = translate_record(&rec)?;

    // NOTE: For poke.received we resolve a single UserId. This is correct
    // because PushHub::send fans out to *all* sessions for that user (see
    // state.rs:send_to), so every connected session receives the notification.
    let recipients = if rec.topic == "poke.received" {
        vec![parse_user_id_field(&rec.payload_json, "target_user_id")?]
    } else if matches!(
        rec.topic.as_str(),
        "channel.created"
            | "channels.created"
            | "channel.renamed"
            | "channel.deleted"
            | "perm.role.upserted"
            | "perm.role.deleted"
            | "perm.role.order_changed"
            | "perm.role.caps_changed"
            | "perm.user.roles_changed"
            | "perm.channel.overrides_changed"
            | "perm.audit.appended"
    ) {
        hub.connected_users()
    } else {
        membership.members_of(channel_id).unwrap_or_default()
    };

    debug!(
        outbox_id = %rec.id.0,
        topic = %rec.topic,
        channel_id = %channel_id.0,
        server_id = %rec.server_id.0,
        fanout = recipients.len(),
        event_seq = push.event_seq,
        "dispatching outbox event"
    );

    apply_cache_side_effects(membership, &rec)?;

    for uid in recipients {
        hub.send(uid, push.clone()).await;
    }

    let mut tx = repo.tx().await?;
    <PgControlRepo as ControlRepo>::ack_outbox_published(repo, &mut tx, &[rec.id], token).await?;
    tx.commit().await?;
    Ok(())
}

fn translate_record(rec: &OutboxEventRow) -> Result<(ChannelId, pb::ServerToClient)> {
    match rec.topic.as_str() {
        "presence.member_joined" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let user_id = parse_user_id_field(&rec.payload_json, "user_id")?;
            let display_name = rec
                .payload_json
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let away_message = rec
                .payload_json
                .get("away_message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            let ev = pb::PresenceEvent {
                at: Some(now_ts()),
                kind: Some(pb::presence_event::Kind::MemberJoined(pb::MemberJoined {
                    channel_id: Some(pb::ChannelId {
                        value: channel_id.0.to_string(),
                    }),
                    member: Some(pb::ChannelMember {
                        user_id: Some(pb::UserId {
                            value: user_id.0.to_string(),
                        }),
                        display_name,
                        muted: false,
                        deafened: false,
                        away_message,
                        ..Default::default()
                    }),
                })),
            };

            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::PresenceEvent(ev)),
            ))
        }
        "presence.member_left" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let user_id = parse_user_id_field(&rec.payload_json, "user_id")?;

            let ev = pb::PresenceEvent {
                at: Some(now_ts()),
                kind: Some(pb::presence_event::Kind::MemberLeft(pb::MemberLeft {
                    channel_id: Some(pb::ChannelId {
                        value: channel_id.0.to_string(),
                    }),
                    user_id: Some(pb::UserId {
                        value: user_id.0.to_string(),
                    }),
                })),
            };

            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::PresenceEvent(ev)),
            ))
        }
        "presence.voice_state_changed" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let user_id = parse_user_id_field(&rec.payload_json, "user_id")?;
            let muted = rec
                .payload_json
                .get("muted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let deafened = rec
                .payload_json
                .get("deafened")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            let ev = pb::PresenceEvent {
                at: Some(now_ts()),
                kind: Some(pb::presence_event::Kind::MemberVoiceStateChanged(
                    pb::MemberVoiceStateChanged {
                        channel_id: Some(pb::ChannelId {
                            value: channel_id.0.to_string(),
                        }),
                        user_id: Some(pb::UserId {
                            value: user_id.0.to_string(),
                        }),
                        muted,
                        deafened,
                        ..Default::default()
                    },
                )),
            };

            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::PresenceEvent(ev)),
            ))
        }
        "presence.user_online_status_changed" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let user_id = parse_user_id_field(&rec.payload_json, "user_id")?;
            let custom_status_text = rec
                .payload_json
                .get("custom_status_text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let custom_status_emoji = rec
                .payload_json
                .get("custom_status_emoji")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let custom_status_expires = rec
                .payload_json
                .get("custom_status_expires_ms")
                .and_then(Value::as_i64)
                .map(|ms| pb::Timestamp { unix_millis: ms });

            let ev = pb::PresenceEvent {
                at: Some(now_ts()),
                kind: Some(pb::presence_event::Kind::UserOnlineStatusChanged(
                    pb::UserOnlineStatusChanged {
                        user_id: Some(pb::UserId {
                            value: user_id.0.to_string(),
                        }),
                        status: pb::OnlineStatus::Online as i32,
                        custom_status_text,
                        custom_status_emoji,
                        custom_status_expires,
                    },
                )),
            };

            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::PresenceEvent(ev)),
            ))
        }
        "chat.message_posted" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let message_id = parse_message_id_field(&rec.payload_json, "message_id")?;
            let author_user_id = parse_user_id_field(&rec.payload_json, "author_user_id")?;
            let text = rec
                .payload_json
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let attachments = rec
                .payload_json
                .get("attachments")
                .cloned()
                .unwrap_or(Value::Array(vec![]));

            let event_at = rec
                .payload_json
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
                .map(|dt| pb::Timestamp {
                    unix_millis: dt.with_timezone(&Utc).timestamp_millis(),
                })
                .unwrap_or_else(now_ts);

            let ev = pb::ChatEvent {
                at: Some(event_at),
                kind: Some(pb::chat_event::Kind::MessagePosted(pb::MessagePosted {
                    message_id: Some(pb::MessageId {
                        value: message_id.0.to_string(),
                    }),
                    channel_id: Some(pb::ChannelId {
                        value: channel_id.0.to_string(),
                    }),
                    author_user_id: Some(pb::UserId {
                        value: author_user_id.0.to_string(),
                    }),
                    text,
                    attachments: json_attachments_to_pb(attachments),
                    ..Default::default()
                })),
            };

            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::ChatEvent(ev)),
            ))
        }
        "moderation.user_muted" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let target_user_id = parse_user_id_field(&rec.payload_json, "target_user_id")?;
            let actor_user_id = parse_user_id_field(&rec.payload_json, "actor_user_id")?;
            let muted = rec
                .payload_json
                .get("muted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let duration_seconds = rec
                .payload_json
                .get("duration_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;

            let ev = pb::ModerationEvent {
                at: Some(now_ts()),
                kind: Some(pb::moderation_event::Kind::UserMuted(pb::UserMuted {
                    channel_id: Some(pb::ChannelId {
                        value: channel_id.0.to_string(),
                    }),
                    target_user_id: Some(pb::UserId {
                        value: target_user_id.0.to_string(),
                    }),
                    muted,
                    duration_seconds,
                    actor_user_id: Some(pb::UserId {
                        value: actor_user_id.0.to_string(),
                    }),
                })),
            };

            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::ModerationEvent(ev)),
            ))
        }

        "moderation.user_deafened" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let target_user_id = parse_user_id_field(&rec.payload_json, "target_user_id")?;
            let actor_user_id = parse_user_id_field(&rec.payload_json, "actor_user_id")?;
            let deafened = rec
                .payload_json
                .get("deafened")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let ev = pb::ModerationEvent {
                at: Some(now_ts()),
                kind: Some(pb::moderation_event::Kind::UserDeafened(pb::UserDeafened {
                    channel_id: Some(pb::ChannelId {
                        value: channel_id.0.to_string(),
                    }),
                    target_user_id: Some(pb::UserId {
                        value: target_user_id.0.to_string(),
                    }),
                    deafened,
                    duration_seconds: 0,
                    actor_user_id: Some(pb::UserId {
                        value: actor_user_id.0.to_string(),
                    }),
                })),
            };
            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::ModerationEvent(ev)),
            ))
        }
        "moderation.user_kicked" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let target_user_id = parse_user_id_field(&rec.payload_json, "target_user_id")?;
            let actor_user_id = parse_user_id_field(&rec.payload_json, "actor_user_id")?;
            let reason = rec
                .payload_json
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let ev = pb::ModerationEvent {
                at: Some(now_ts()),
                kind: Some(pb::moderation_event::Kind::UserKicked(pb::UserKicked {
                    channel_id: Some(pb::ChannelId {
                        value: channel_id.0.to_string(),
                    }),
                    target_user_id: Some(pb::UserId {
                        value: target_user_id.0.to_string(),
                    }),
                    reason,
                    actor_user_id: Some(pb::UserId {
                        value: actor_user_id.0.to_string(),
                    }),
                })),
            };
            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::ModerationEvent(ev)),
            ))
        }
        "poke.received" => {
            let _target_user_id = parse_user_id_field(&rec.payload_json, "target_user_id")?;
            let from_user_id = parse_user_id_field(&rec.payload_json, "from_user_id")?;
            let from_display_name = rec
                .payload_json
                .get("from_display_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let message = rec
                .payload_json
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let ev = pb::PokeEvent {
                at: Some(now_ts()),
                from_user_id: Some(pb::UserId {
                    value: from_user_id.0.to_string(),
                }),
                from_display_name,
                message,
            };
            Ok((
                ChannelId(uuid::Uuid::nil()),
                server_push(pb::server_to_client::Payload::PokeEvent(ev)),
            ))
        }
        // Compatibility alias support: keep consuming queued channel-created rows
        // emitted by older/newer producers.
        "channel.created" | "channels.created" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let name = rec
                .payload_json
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("New Channel")
                .to_string();
            let parent_channel_id = rec
                .payload_json
                .get("parent_channel_id")
                .and_then(Value::as_str)
                .map(|value| pb::ChannelId {
                    value: value.to_string(),
                });
            let channel_type = parse_i32_field_default(&rec.payload_json, "channel_type", 2);
            let description = rec
                .payload_json
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let user_limit = parse_u32_field_default(&rec.payload_json, "max_members", 0);
            let bitrate = parse_u32_field_default(&rec.payload_json, "bitrate_bps", 64_000);
            let opus_profile = parse_i32_field_default(&rec.payload_json, "opus_profile", 1);

            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::ChannelCreatedPush(
                    pb::ChannelCreatedPush {
                        channel: Some(pb::ChannelInfo {
                            channel_id: Some(pb::ChannelId {
                                value: channel_id.0.to_string(),
                            }),
                            name,
                            channel_type,
                            description,
                            parent_channel_id,
                            user_limit,
                            bitrate,
                            opus_profile,
                            ..Default::default()
                        }),
                    },
                )),
            ))
        }
        "channel.renamed" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let name = rec
                .payload_json
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Renamed Channel")
                .to_string();
            let parent_channel_id = rec
                .payload_json
                .get("parent_channel_id")
                .and_then(Value::as_str)
                .map(|value| pb::ChannelId {
                    value: value.to_string(),
                });
            let channel_type = parse_i32_field_default(&rec.payload_json, "channel_type", 2);
            let description = rec
                .payload_json
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let user_limit = parse_u32_field_default(&rec.payload_json, "max_members", 0);
            let bitrate = parse_u32_field_default(&rec.payload_json, "bitrate_bps", 64_000);
            let opus_profile = parse_i32_field_default(&rec.payload_json, "opus_profile", 1);

            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::ChannelRenamedPush(
                    pb::ChannelRenamedPush {
                        channel: Some(pb::ChannelInfo {
                            channel_id: Some(pb::ChannelId {
                                value: channel_id.0.to_string(),
                            }),
                            name,
                            channel_type,
                            description,
                            parent_channel_id,
                            user_limit,
                            bitrate,
                            opus_profile,
                            ..Default::default()
                        }),
                    },
                )),
            ))
        }
        "channel.deleted" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::ChannelDeletedPush(
                    pb::ChannelDeletedPush {
                        channel_id: Some(pb::ChannelId {
                            value: channel_id.0.to_string(),
                        }),
                    },
                )),
            ))
        }

        "perm.role.upserted" => {
            let role_id = rec
                .payload_json
                .get("role_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = rec
                .payload_json
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let position = rec
                .payload_json
                .get("position")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0) as u32;
            Ok((
                ChannelId(uuid::Uuid::nil()),
                server_push(pb::server_to_client::Payload::PermissionsPushEvent(
                    pb::PushEvent {
                        evt: Some(pb::push_event::Evt::RoleUpserted(pb::RoleUpserted {
                            role_id,
                            name,
                            position,
                        })),
                    },
                )),
            ))
        }
        "perm.role.deleted" => {
            let role_id = rec
                .payload_json
                .get("role_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok((
                ChannelId(uuid::Uuid::nil()),
                server_push(pb::server_to_client::Payload::PermissionsPushEvent(
                    pb::PushEvent {
                        evt: Some(pb::push_event::Evt::RoleDeleted(pb::RoleDeleted {
                            role_id,
                        })),
                    },
                )),
            ))
        }
        "perm.role.order_changed" => {
            let role_ids = rec
                .payload_json
                .get("role_ids")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Ok((
                ChannelId(uuid::Uuid::nil()),
                server_push(pb::server_to_client::Payload::PermissionsPushEvent(
                    pb::PushEvent {
                        evt: Some(pb::push_event::Evt::RoleOrder(pb::RoleOrderChanged {
                            role_ids,
                        })),
                    },
                )),
            ))
        }
        "perm.role.caps_changed" => {
            let role_id = rec
                .payload_json
                .get("role_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok((
                ChannelId(uuid::Uuid::nil()),
                server_push(pb::server_to_client::Payload::PermissionsPushEvent(
                    pb::PushEvent {
                        evt: Some(pb::push_event::Evt::RoleCaps(pb::RoleCapsChanged {
                            role_id,
                        })),
                    },
                )),
            ))
        }
        "perm.user.roles_changed" => {
            let user_id = parse_user_id_field(&rec.payload_json, "user_id")?;
            Ok((
                ChannelId(uuid::Uuid::nil()),
                server_push(pb::server_to_client::Payload::PermissionsPushEvent(
                    pb::PushEvent {
                        evt: Some(pb::push_event::Evt::UserRoles(pb::UserRolesChanged {
                            user_id: Some(pb::UserId {
                                value: user_id.0.to_string(),
                            }),
                        })),
                    },
                )),
            ))
        }
        "perm.channel.overrides_changed" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::PermissionsPushEvent(
                    pb::PushEvent {
                        evt: Some(pb::push_event::Evt::ChanOvr(pb::ChannelOverridesChanged {
                            channel_id: Some(pb::ChannelId {
                                value: channel_id.0.to_string(),
                            }),
                        })),
                    },
                )),
            ))
        }
        "perm.audit.appended" => {
            let action = rec
                .payload_json
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let target_type = rec
                .payload_json
                .get("target_type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let target_id = rec
                .payload_json
                .get("target_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok((
                ChannelId(uuid::Uuid::nil()),
                server_push(pb::server_to_client::Payload::PermissionsPushEvent(
                    pb::PushEvent {
                        evt: Some(pb::push_event::Evt::AuditAppended(pb::AuditAppended {
                            action,
                            target_type,
                            target_id,
                        })),
                    },
                )),
            ))
        }
        other => Err(anyhow!("unsupported outbox event type: {other}")),
    }
}

fn apply_cache_side_effects(membership: &MembershipCache, rec: &OutboxEventRow) -> Result<()> {
    match rec.topic.as_str() {
        "presence.member_joined" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let user_id = parse_user_id_field(&rec.payload_json, "user_id")?;
            membership.set_user(user_id, channel_id, false, false);
            membership.add_channel_member(channel_id, user_id);
        }
        "presence.member_left" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let user_id = parse_user_id_field(&rec.payload_json, "user_id")?;
            membership.remove_user(user_id);
            membership.remove_channel_member(channel_id, user_id);
        }
        "presence.voice_state_changed" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let user_id = parse_user_id_field(&rec.payload_json, "user_id")?;
            let muted = rec
                .payload_json
                .get("muted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let deafened = rec
                .payload_json
                .get("deafened")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            membership.update_voice_state(user_id, channel_id, muted, deafened);
        }
        "presence.user_online_status_changed" => {
            // no membership cache side effects
        }
        "moderation.user_muted" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let user_id = parse_user_id_field(&rec.payload_json, "target_user_id")?;
            let muted = rec
                .payload_json
                .get("muted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let deafened = rec
                .payload_json
                .get("deafened")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            membership.update_voice_state(user_id, channel_id, muted, deafened);
        }
        "moderation.user_deafened" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let user_id = parse_user_id_field(&rec.payload_json, "target_user_id")?;
            let deafened = rec
                .payload_json
                .get("deafened")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            membership.update_deafen(user_id, channel_id, deafened);
        }
        "channel.created"
        | "channels.created"
        | "channel.renamed"
        | "channel.deleted"
        | "perm.role.upserted"
        | "perm.role.deleted"
        | "perm.role.order_changed"
        | "perm.role.caps_changed"
        | "perm.user.roles_changed"
        | "perm.channel.overrides_changed"
        | "perm.audit.appended" => {
            // no membership side-effects; event is still consumed/acked.
        }
        _ => {}
    }
    Ok(())
}

fn parse_uuid(v: &Value, field: &str) -> Result<uuid::Uuid> {
    let s = v
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing field {field}"))?;
    uuid::Uuid::parse_str(s).context("uuid parse")
}

fn parse_channel_id_field(v: &Value, field: &str) -> Result<ChannelId> {
    Ok(ChannelId(parse_uuid(v, field)?))
}

fn parse_user_id_field(v: &Value, field: &str) -> Result<UserId> {
    Ok(UserId(parse_uuid(v, field)?))
}

fn parse_message_id_field(v: &Value, field: &str) -> Result<MessageId> {
    Ok(MessageId(parse_uuid(v, field)?))
}

fn parse_i32_field_default(v: &Value, field: &str, default: i32) -> i32 {
    v.get(field)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(default)
}

fn parse_u32_field_default(v: &Value, field: &str, default: u32) -> u32 {
    v.get(field)
        .and_then(|value| {
            if value.is_null() {
                return Some(default as u64);
            }
            value
                .as_u64()
                .or_else(|| value.as_i64().map(|n| n.max(0) as u64))
        })
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(default)
}

fn json_attachments_to_pb(v: Value) -> Vec<pb::AttachmentRef> {
    let arr = match v {
        Value::Array(a) => a,
        _ => return vec![],
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let Value::Object(o) = item {
            let asset_id = o.get("asset_id").and_then(Value::as_str).unwrap_or("");
            out.push(pb::AttachmentRef {
                asset_id: Some(pb::AssetId {
                    value: asset_id.to_string(),
                }),
                filename: o
                    .get("filename")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                mime_type: o
                    .get("mime_type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                size_bytes: o.get("size_bytes").and_then(Value::as_u64).unwrap_or(0),
                width: o.get("width").and_then(Value::as_u64).unwrap_or(0) as u32,
                height: o.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
                duration_ms: o.get("duration_ms").and_then(Value::as_u64).unwrap_or(0),
                sha256: o
                    .get("sha256")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    out
}

fn server_push(payload: pb::server_to_client::Payload) -> pb::ServerToClient {
    pb::ServerToClient {
        request_id: None,
        session_id: None,
        sent_at: Some(now_ts()),
        error: None,
        event_seq: now_seq(),
        payload: Some(payload),
    }
}

fn now_seq() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn now_ts() -> pb::Timestamp {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    pb::Timestamp { unix_millis: ms }
}

#[cfg(test)]
mod tests {

    use super::{apply_cache_side_effects, translate_record};
    use crate::proto::voiceplatform::v1 as pb;
    use crate::state::MembershipCache;
    use serde_json::json;
    use vp_control::ids::{OutboxId, ServerId};
    use vp_control::model::OutboxEventRow;
    use vp_media::voice_forwarder::MembershipProvider;

    #[test]
    fn translate_channel_created_topic_is_supported() {
        let channel_id = uuid::Uuid::new_v4();
        let rec = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "channel.created".to_string(),
            payload_json: json!({
                "channel_id": channel_id,
                "name": "General"
            }),
        };

        let (parsed_channel, push) =
            translate_record(&rec).expect("channel.created should be supported");
        assert_eq!(parsed_channel.0, channel_id);
        match push.payload {
            Some(pb::server_to_client::Payload::ChannelCreatedPush(cr)) => {
                let channel = cr.channel.expect("channel");
                assert_eq!(channel.name, "General");
            }
            other => panic!("unexpected payload: {:?}", other),
        }
    }

    #[test]
    fn translate_channels_created_alias_is_supported() {
        let channel_id = uuid::Uuid::new_v4();
        let rec = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "channels.created".to_string(),
            payload_json: json!({"channel_id": channel_id}),
        };

        let (parsed_channel, _push) =
            translate_record(&rec).expect("channels.created alias should be supported");
        assert_eq!(parsed_channel.0, channel_id);
    }

    #[test]
    fn translate_presence_user_online_status_changed_is_supported() {
        let channel_id = uuid::Uuid::new_v4();
        let user_id = uuid::Uuid::new_v4();
        let rec = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "presence.user_online_status_changed".to_string(),
            payload_json: json!({
                "channel_id": channel_id,
                "user_id": user_id,
                "custom_status_text": "Lunch"
            }),
        };

        let (parsed_channel, push) = translate_record(&rec)
            .expect("presence.user_online_status_changed should be supported");
        assert_eq!(parsed_channel.0, channel_id);
        match push.payload {
            Some(pb::server_to_client::Payload::PresenceEvent(ev)) => match ev.kind {
                Some(pb::presence_event::Kind::UserOnlineStatusChanged(status)) => {
                    assert_eq!(status.user_id.expect("user id").value, user_id.to_string());
                    assert_eq!(status.custom_status_text, "Lunch");
                }
                other => panic!("unexpected presence event: {:?}", other),
            },
            other => panic!("unexpected payload: {:?}", other),
        }
    }
    
    #[test]
    fn translate_presence_member_joined_includes_away_message() {
        let channel_id = uuid::Uuid::new_v4();
        let user_id = uuid::Uuid::new_v4();
        let rec = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "presence.member_joined".to_string(),
            payload_json: json!({
                "channel_id": channel_id,
                "user_id": user_id,
                "display_name": "alice",
                "away_message": "Out to lunch"
            }),
        };

        let (parsed_channel, push) =
            translate_record(&rec).expect("presence.member_joined should be supported");
        assert_eq!(parsed_channel.0, channel_id);
        match push.payload {
            Some(pb::server_to_client::Payload::PresenceEvent(ev)) => match ev.kind {
                Some(pb::presence_event::Kind::MemberJoined(joined)) => {
                    let member = joined.member.expect("member");
                    assert_eq!(member.user_id.expect("user id").value, user_id.to_string());
                    assert_eq!(member.display_name, "alice");
                    assert_eq!(member.away_message, "Out to lunch");
                }
                other => panic!("unexpected presence event: {:?}", other),
            },
            other => panic!("unexpected payload: {:?}", other),
        }
    }
    
    #[test]
    fn member_join_left_side_effects_update_channel_members() {
        let membership = MembershipCache::new();
        let channel_id = uuid::Uuid::new_v4();
        let user_id = uuid::Uuid::new_v4();

        membership.set_channel(vp_control::ids::ChannelId(channel_id), 4, vec![]);

        let joined = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "presence.member_joined".to_string(),
            payload_json: json!({
                "channel_id": channel_id,
                "user_id": user_id
            }),
        };
        apply_cache_side_effects(&membership, &joined).expect("join side effects should apply");

        let members = membership
            .members_of(vp_control::ids::ChannelId(channel_id))
            .expect("channel should exist in cache");
        assert_eq!(members, vec![vp_control::ids::UserId(user_id)]);

        let left = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "presence.member_left".to_string(),
            payload_json: json!({
                "channel_id": channel_id,
                "user_id": user_id
            }),
        };
        apply_cache_side_effects(&membership, &left).expect("left side effects should apply");

        let members = membership
            .members_of(vp_control::ids::ChannelId(channel_id))
            .expect("channel should exist in cache");
        assert!(members.is_empty());
    }
    #[tokio::test]
    async fn voice_state_and_deafen_side_effects_update_membership_state() {
        let membership = MembershipCache::new();
        let channel = vp_control::ids::ChannelId(uuid::Uuid::new_v4());
        let user = vp_control::ids::UserId(uuid::Uuid::new_v4());

        membership.set_channel(channel, 4, vec![user]);
        membership.set_user(user, channel, false, false);

        let voice_state = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "presence.voice_state_changed".to_string(),
            payload_json: json!({
                "channel_id": channel.0,
                "user_id": user.0,
                "muted": true,
                "deafened": true
            }),
        };
        apply_cache_side_effects(&membership, &voice_state).expect("voice state side effects");

        assert!(membership.is_muted(channel, user).await);
        assert!(membership.is_deafened(channel, user).await);

        let moderation = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "moderation.user_deafened".to_string(),
            payload_json: json!({
                "channel_id": channel.0,
                "target_user_id": user.0,
                "deafened": false
            }),
        };
        apply_cache_side_effects(&membership, &moderation).expect("moderation deafen side effects");

        assert!(membership.is_muted(channel, user).await);
        assert!(!membership.is_deafened(channel, user).await);
    }

    #[test]
    fn status_changed_propagates_text_and_emoji() {
        let channel_id = uuid::Uuid::new_v4();
        let user_id = uuid::Uuid::new_v4();
        let rec = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "presence.user_online_status_changed".to_string(),
            payload_json: json!({
                "channel_id": channel_id,
                "user_id": user_id,
                "custom_status_text": "In a meeting",
                "custom_status_emoji": "\u{1F4BC}",
            }),
        };

        let (_ch, push) = translate_record(&rec).expect("should translate");
        match push.payload {
            Some(pb::server_to_client::Payload::PresenceEvent(ev)) => match ev.kind {
                Some(pb::presence_event::Kind::UserOnlineStatusChanged(s)) => {
                    assert_eq!(s.custom_status_text, "In a meeting");
                    assert_eq!(s.custom_status_emoji, "\u{1F4BC}");
                    assert!(s.custom_status_expires.is_none());
                }
                other => panic!("unexpected: {:?}", other),
            },
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn status_changed_propagates_emoji_only() {
        let channel_id = uuid::Uuid::new_v4();
        let user_id = uuid::Uuid::new_v4();
        let rec = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "presence.user_online_status_changed".to_string(),
            payload_json: json!({
                "channel_id": channel_id,
                "user_id": user_id,
                "custom_status_text": "",
                "custom_status_emoji": "\u{2615}",
            }),
        };

        let (_ch, push) = translate_record(&rec).expect("should translate");
        match push.payload {
            Some(pb::server_to_client::Payload::PresenceEvent(ev)) => match ev.kind {
                Some(pb::presence_event::Kind::UserOnlineStatusChanged(s)) => {
                    assert_eq!(s.custom_status_text, "");
                    assert_eq!(s.custom_status_emoji, "\u{2615}");
                }
                other => panic!("unexpected: {:?}", other),
            },
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn status_changed_propagates_expiry() {
        let channel_id = uuid::Uuid::new_v4();
        let user_id = uuid::Uuid::new_v4();
        let expires_ms: i64 = 1700000000000;
        let rec = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "presence.user_online_status_changed".to_string(),
            payload_json: json!({
                "channel_id": channel_id,
                "user_id": user_id,
                "custom_status_text": "BRB",
                "custom_status_emoji": "\u{1F6B6}",
                "custom_status_expires_ms": expires_ms,
            }),
        };

        let (_ch, push) = translate_record(&rec).expect("should translate");
        match push.payload {
            Some(pb::server_to_client::Payload::PresenceEvent(ev)) => match ev.kind {
                Some(pb::presence_event::Kind::UserOnlineStatusChanged(s)) => {
                    assert_eq!(s.custom_status_text, "BRB");
                    assert_eq!(s.custom_status_emoji, "\u{1F6B6}");
                    let ts = s.custom_status_expires.expect("should have expiry");
                    assert_eq!(ts.unix_millis, expires_ms);
                }
                other => panic!("unexpected: {:?}", other),
            },
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn status_changed_clear_propagates_empty_fields() {
        let channel_id = uuid::Uuid::new_v4();
        let user_id = uuid::Uuid::new_v4();
        let rec = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "presence.user_online_status_changed".to_string(),
            payload_json: json!({
                "channel_id": channel_id,
                "user_id": user_id,
                "custom_status_text": "",
                "custom_status_emoji": "",
                "custom_status_expires_ms": null,
            }),
        };

        let (_ch, push) = translate_record(&rec).expect("should translate");
        match push.payload {
            Some(pb::server_to_client::Payload::PresenceEvent(ev)) => match ev.kind {
                Some(pb::presence_event::Kind::UserOnlineStatusChanged(s)) => {
                    assert_eq!(s.custom_status_text, "");
                    assert_eq!(s.custom_status_emoji, "");
                    assert!(s.custom_status_expires.is_none());
                }
                other => panic!("unexpected: {:?}", other),
            },
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn status_changed_backward_compat_missing_emoji_fields() {
        // Old-format outbox events without emoji/expiry should still work
        let channel_id = uuid::Uuid::new_v4();
        let user_id = uuid::Uuid::new_v4();
        let rec = OutboxEventRow {
            id: OutboxId(uuid::Uuid::new_v4()),
            server_id: ServerId(uuid::Uuid::new_v4()),
            topic: "presence.user_online_status_changed".to_string(),
            payload_json: json!({
                "channel_id": channel_id,
                "user_id": user_id,
                "custom_status_text": "Legacy status"
            }),
        };

        let (_ch, push) = translate_record(&rec).expect("should translate");
        match push.payload {
            Some(pb::server_to_client::Payload::PresenceEvent(ev)) => match ev.kind {
                Some(pb::presence_event::Kind::UserOnlineStatusChanged(s)) => {
                    assert_eq!(s.custom_status_text, "Legacy status");
                    assert_eq!(s.custom_status_emoji, "");
                    assert!(s.custom_status_expires.is_none());
                }
                other => panic!("unexpected: {:?}", other),
            },
            other => panic!("unexpected: {:?}", other),
        }
    }
}
