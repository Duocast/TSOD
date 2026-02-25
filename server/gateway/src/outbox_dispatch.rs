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

    let recipients = if rec.topic == "channel.created" || rec.topic == "channels.created" {
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

            let state = pb::ChannelState {
                channel_id: Some(pb::ChannelId {
                    value: channel_id.0.to_string(),
                }),
                name,
                members: vec![],
                ..Default::default()
            };

            Ok((
                channel_id,
                server_push(pb::server_to_client::Payload::CreateChannelResponse(
                    pb::CreateChannelResponse { state: Some(state) },
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
            membership.set_user(user_id, channel_id, false);
        }
        "presence.member_left" => {
            let user_id = parse_user_id_field(&rec.payload_json, "user_id")?;
            membership.remove_user(user_id);
        }
        "presence.voice_state_changed" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let user_id = parse_user_id_field(&rec.payload_json, "user_id")?;
            let muted = rec
                .payload_json
                .get("muted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            membership.update_mute(user_id, channel_id, muted);
        }
        "moderation.user_muted" => {
            let channel_id = parse_channel_id_field(&rec.payload_json, "channel_id")?;
            let user_id = parse_user_id_field(&rec.payload_json, "target_user_id")?;
            let muted = rec
                .payload_json
                .get("muted")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            membership.update_mute(user_id, channel_id, muted);
        }
        "channel.created" | "channels.created" => {
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
                ..Default::default()
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
    use super::translate_record;
    use crate::proto::voiceplatform::v1 as pb;
    use serde_json::json;
    use vp_control::ids::{OutboxId, ServerId};
    use vp_control::model::OutboxEventRow;

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
            Some(pb::server_to_client::Payload::CreateChannelResponse(cr)) => {
                let state = cr.state.expect("state");
                assert_eq!(state.name, "General");
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
}
