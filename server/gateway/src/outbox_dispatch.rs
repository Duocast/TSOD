use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::proto::voiceplatform::v1 as pb;
use crate::state::{MembershipCache, PushHub};

use vp_control::{OutboxRecord, PgControlRepo};
use vp_control::ids::{ChannelId, ServerId, UserId};

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
    let token = format!("gw-{}", uuid::Uuid::new_v4());
    info!(%token, server_id = %cfg.server_id.0, "outbox dispatcher started");

    loop {
        let mut tx = repo.tx().await.context("outbox tx")?;
        let batch = repo
            .claim_outbox_batch(&mut tx, cfg.server_id, cfg.batch_size, &token, cfg.claim_ttl_seconds)
            .await
            .context("claim_outbox_batch")?;
        tx.commit().await.context("outbox tx commit")?;

        if batch.is_empty() {
            sleep(cfg.poll_interval).await;
            continue;
        }

        for rec in batch {
            if let Err(e) = handle_record(&repo, &hub, &membership, &cfg, &token, rec).await {
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
    cfg: &OutboxDispatcherConfig,
    token: &str,
    rec: OutboxRecord,
) -> Result<()> {
    // Translate record -> push payload
    let (channel_id, push) = translate_record(&rec)?;

    // Determine recipients
    let mut recipients = membership.members_of(channel_id).unwrap_or_default();
    if recipients.is_empty() {
        // Fallback: authoritative fetch from DB
        let mut tx = repo.tx().await?;
        let members = repo.list_members(&mut tx, channel_id).await?;
        tx.commit().await?;
        recipients = members.into_iter().map(|m| m.user_id).collect();
        if let Some(max) = membership.max_talkers_of(channel_id) {
            let _ = max; // keep clippy happy
        }
    }

    // Update membership cache for presence/mod events
    apply_cache_side_effects(membership, &rec)?;

    for uid in recipients {
        // Best-effort push; a disconnected user will simply fail to send.
        hub.send(uid, push.clone()).await;
    }

    // Ack (mark as published) with claim token
    let mut tx = repo.tx().await?;
    repo.ack_outbox_published(&mut tx, &rec.id, token).await?;
    tx.commit().await?;
    Ok(())
}

fn translate_record(rec: &OutboxRecord) -> Result<(ChannelId, pb::ServerToClient)> {
    let typ = rec
        .payload
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("outbox payload missing type"))?;

    match typ {
        "presence.member_joined" => {
            let channel_id = parse_uuid_field(&rec.payload, "channel_id")?;
            let user_id = parse_uuid_field(&rec.payload, "user_id")?;
            let display_name = rec.payload.get("display_name").and_then(|v| v.as_str()).unwrap_or("").to_string();

            let ev = pb::PresenceEvent {
                at: Some(now_ts()),
                kind: Some(pb::presence_event::Kind::MemberJoined(pb::MemberJoined {
                    channel_id: Some(pb::ChannelId { value: channel_id.0.to_string() }),
                    member: Some(pb::ChannelMember {
                        user_id: Some(pb::UserId { value: user_id.0.to_string() }),
                        display_name,
                        muted: false,
                        deafened: false,
                    }),
                })),
            };

            Ok((channel_id, server_push(pb::server_to_client::Payload::PresenceEvent(ev))))
        }
        "presence.member_left" => {
            let channel_id = parse_uuid_field(&rec.payload, "channel_id")?;
            let user_id = parse_uuid_field(&rec.payload, "user_id")?;

            let ev = pb::PresenceEvent {
                at: Some(now_ts()),
                kind: Some(pb::presence_event::Kind::MemberLeft(pb::MemberLeft {
                    channel_id: Some(pb::ChannelId { value: channel_id.0.to_string() }),
                    user_id: Some(pb::UserId { value: user_id.0.to_string() }),
                })),
            };

            Ok((channel_id, server_push(pb::server_to_client::Payload::PresenceEvent(ev))))
        }
        "presence.voice_state_changed" => {
            let channel_id = parse_uuid_field(&rec.payload, "channel_id")?;
            let user_id = parse_uuid_field(&rec.payload, "user_id")?;
            let muted = rec.payload.get("muted").and_then(|v| v.as_bool()).unwrap_or(false);
            let deafened = rec.payload.get("deafened").and_then(|v| v.as_bool()).unwrap_or(false);

            let ev = pb::PresenceEvent {
                at: Some(now_ts()),
                kind: Some(pb::presence_event::Kind::MemberVoiceStateChanged(pb::MemberVoiceStateChanged {
                    channel_id: Some(pb::ChannelId { value: channel_id.0.to_string() }),
                    user_id: Some(pb::UserId { value: user_id.0.to_string() }),
                    muted,
                    deafened,
                })),
            };

            Ok((channel_id, server_push(pb::server_to_client::Payload::PresenceEvent(ev))))
        }
        "chat.message_posted" => {
            let channel_id = parse_uuid_field(&rec.payload, "channel_id")?;
            let message_id = parse_uuid_field(&rec.payload, "message_id")?;
            let author_user_id = parse_uuid_field(&rec.payload, "author_user_id")?;
            let text = rec.payload.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let attachments = rec.payload.get("attachments").cloned().unwrap_or(Value::Array(vec![]));

            let ev = pb::ChatEvent {
                at: Some(now_ts()),
                kind: Some(pb::chat_event::Kind::MessagePosted(pb::MessagePosted {
                    message_id: Some(pb::MessageId { value: message_id.0.to_string() }),
                    channel_id: Some(pb::ChannelId { value: channel_id.0.to_string() }),
                    author_user_id: Some(pb::UserId { value: author_user_id.0.to_string() }),
                    text,
                    attachments: json_attachments_to_pb(attachments)?,
                })),
            };

            Ok((channel_id, server_push(pb::server_to_client::Payload::ChatEvent(ev))))
        }
        "moderation.user_muted" => {
            let channel_id = parse_uuid_field(&rec.payload, "channel_id")?;
            let target_user_id = parse_uuid_field(&rec.payload, "target_user_id")?;
            let actor_user_id = parse_uuid_field(&rec.payload, "actor_user_id")?;
            let muted = rec.payload.get("muted").and_then(|v| v.as_bool()).unwrap_or(false);
            let duration_seconds = rec.payload.get("duration_seconds").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

            let ev = pb::ModerationEvent {
                at: Some(now_ts()),
                kind: Some(pb::moderation_event::Kind::UserMuted(pb::UserMuted {
                    channel_id: Some(pb::ChannelId { value: channel_id.0.to_string() }),
                    target_user_id: Some(pb::UserId { value: target_user_id.0.to_string() }),
                    muted,
                    duration_seconds,
                    actor_user_id: Some(pb::UserId { value: actor_user_id.0.to_string() }),
                })),
            };

            Ok((channel_id, server_push(pb::server_to_client::Payload::ModerationEvent(ev))))
        }
        other => Err(anyhow!("unsupported outbox event type: {other}")),
    }
}

fn apply_cache_side_effects(membership: &MembershipCache, rec: &OutboxRecord) -> Result<()> {
    let typ = rec.payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match typ {
        "presence.member_joined" => {
            let channel_id = parse_uuid_field(&rec.payload, "channel_id")?;
            let user_id = parse_uuid_field(&rec.payload, "user_id")?;
            // Caller should have already set full state on join; but for multi-gateway ensure user is set.
            membership.set_user(user_id, channel_id, false);
        }
        "presence.member_left" => {
            let user_id = parse_uuid_field(&rec.payload, "user_id")?;
            membership.remove_user(user_id);
        }
        "presence.voice_state_changed" => {
            let channel_id = parse_uuid_field(&rec.payload, "channel_id")?;
            let user_id = parse_uuid_field(&rec.payload, "user_id")?;
            let muted = rec.payload.get("muted").and_then(|v| v.as_bool()).unwrap_or(false);
            membership.update_mute(user_id, channel_id, muted);
        }
        "moderation.user_muted" => {
            let channel_id = parse_uuid_field(&rec.payload, "channel_id")?;
            let user_id = parse_uuid_field(&rec.payload, "target_user_id")?;
            let muted = rec.payload.get("muted").and_then(|v| v.as_bool()).unwrap_or(false);
            membership.update_mute(user_id, channel_id, muted);
        }
        _ => {}
    }
    Ok(())
}

fn parse_uuid_field(v: &Value, field: &str) -> Result<vp_control::ids::ChannelId> {
    let s = v.get(field).and_then(|x| x.as_str()).ok_or_else(|| anyhow!("missing field {field}"))?;
    let id = uuid::Uuid::parse_str(s).context("uuid parse")?;
    Ok(vp_control::ids::ChannelId(id))
}

fn json_attachments_to_pb(v: Value) -> Result<Vec<pb::AttachmentRef>> {
    let arr = match v {
        Value::Array(a) => a,
        _ => return Ok(vec![]),
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let Value::Object(o) = item {
            let asset_id = o.get("asset_id").and_then(|x| x.as_str()).unwrap_or("");
            out.push(pb::AttachmentRef {
                asset_id: Some(pb::AssetId { value: asset_id.to_string() }),
                filename: o.get("filename").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                mime_type: o.get("mime_type").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                size_bytes: o.get("size_bytes").and_then(|x| x.as_u64()).unwrap_or(0),
                width: o.get("width").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                height: o.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                duration_ms: o.get("duration_ms").and_then(|x| x.as_u64()).unwrap_or(0),
            });
        }
    }
    Ok(out)
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
