use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tokio::time::sleep;
use tracing::{info, warn};

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

    let recipients = membership.members_of(channel_id).unwrap_or_default();
    
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

            let ev = pb::ChatEvent {
                at: Some(now_ts()),
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
            });
        }
    }
    out
}

fn server_push(payload: pb::server_to_client::Payload) -> pb::ServerToClient {
    pb::ServerToClient {
        request_id: Some(pb::RequestId { value: 0 }),
        session_id: None,
        sent_at: Some(now_ts()),
        error: None,
        payload: Some(payload),
    }
}

fn now_ts() -> pb::Timestamp {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    pb::Timestamp { unix_millis: ms }
}
