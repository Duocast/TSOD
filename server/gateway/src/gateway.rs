use anyhow::{anyhow, Context, Result};
use ring::rand::SecureRandom;
use scopeguard::defer;
use std::sync::Arc;
use tokio::{
    sync::mpsc,
    time::{timeout, Duration},
};
use tracing::{debug, info, warn};

use crate::{
    auth::{AuthProvider, AuthedIdentity},
    frame::{read_delimited, write_delimited},
    media::MediaService,
    proto::voiceplatform::v1 as pb,
    state::{MembershipCache, PushHub, QuinnDatagramTx, Sessions},
};

use vp_control::ids::{ChannelId, ServerId, UserId};
use vp_control::model::{ChannelCreate, JoinChannel, SendMessage};
use vp_control::{ControlError, ControlRepo, ControlService, PgControlRepo, RequestContext};
use vp_media::voice_forwarder::VoiceForwarder;

const CONTROL_STREAM_MAX_MSG: usize = 256 * 1024; // 256KB
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Gateway {
    auth: Arc<dyn AuthProvider>,
    alpn: Vec<u8>,
    control: Arc<ControlService<PgControlRepo>>,
    sessions: Sessions,
    push: PushHub,
    membership: MembershipCache,
    voice: Arc<VoiceForwarder>,
    media: Arc<MediaService>,
}

impl Gateway {
    pub fn new(
        auth: Arc<dyn AuthProvider>,
        alpn: String,
        control: Arc<ControlService<PgControlRepo>>,
        sessions: Sessions,
        push: PushHub,
        membership: MembershipCache,
        voice: Arc<VoiceForwarder>,
        media: Arc<MediaService>,
    ) -> Self {
        Self {
            auth,
            alpn: alpn.into_bytes(),
            control,
            sessions,
            push,
            membership,
            voice,
            media,
        }
    }

    pub async fn serve(self, endpoint: quinn::Endpoint) -> Result<()> {
        info!(expected_alpn = %String::from_utf8_lossy(&self.alpn), "gateway listening");

        loop {
            let incoming = endpoint
                .accept()
                .await
                .ok_or_else(|| anyhow!("endpoint closed"))?;
            let gw = self.clone();

            tokio::spawn(async move {
                if let Err(e) = gw.handle_conn(incoming).await {
                    warn!("conn ended with error: {:#}", e);
                }
            });
        }
    }

    async fn handle_conn(&self, incoming: quinn::Incoming) -> Result<()> {
        let conn = incoming.await.context("accept quic connection")?;

        // ALPN check (defense-in-depth).
        let negotiated = conn
            .handshake_data()
            .and_then(|d| d.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
            .and_then(|d| d.protocol);

        info!(
            remote = %conn.remote_address(),
            expected_alpn = %String::from_utf8_lossy(&self.alpn),
            negotiated_alpn = ?negotiated
                .as_ref()
                .map(|p| String::from_utf8_lossy(p).to_string()),
            "QUIC handshake completed"
        );

        if negotiated.as_deref() != Some(&self.alpn[..]) {
            return Err(anyhow!(
                "ALPN mismatch: got {:?}, want {:?}",
                negotiated,
                self.alpn
            ));
        }

        let remote = conn.remote_address();
        info!(%remote, "connected");

        // Expect client to open the first bi-directional stream as the control stream.
        let (mut send, mut recv) = timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
            .await
            .context("control accept_bi timeout")?
            .context("accept_bi failed")?;

        let (session_id, _hello_caps, auth_challenge) = self.do_hello(&mut send, &mut recv).await?;
        let identity = self
            .do_auth(&mut send, &mut recv, &session_id, &auth_challenge)
            .await?;

        let user_id =
            UserId(uuid::Uuid::parse_str(&identity.user_id).context("invalid user_id uuid")?);
        let server_id =
            ServerId(uuid::Uuid::parse_str(&identity.server_id).context("invalid server_id uuid")?);

        info!(
            %remote,
            session_id = %session_id,
            user_id = %identity.user_id,
            display_name = %identity.display_name,
            "authenticated"
        );

        let snapshot = self
            .build_initial_snapshot(server_id, user_id, &identity.display_name)
            .await?;
        debug!(
            session_id = %session_id,
            server_id = %server_id.0,
            user_id = %user_id.0,
            channel_count = snapshot.channels.len(),
            member_scope_count = snapshot.channel_members.len(),
            "sending post-auth authoritative snapshot"
        );
        let snapshot_push = pb::ServerToClient {
            request_id: None,
            session_id: Some(pb::SessionId {
                value: session_id.clone(),
            }),
            sent_at: Some(now_ts()),
            error: None,
            event_seq: unix_ms_u64(),
            payload: Some(pb::server_to_client::Payload::InitialStateSnapshot(
                snapshot,
            )),
        };
        write_delimited(&mut send, &snapshot_push)
            .await
            .context("write InitialStateSnapshot push")?;

        // Control stream writes are performed inline in this task.

        let (push_tx, mut push_rx) = mpsc::channel::<pb::ServerToClient>(1024);
        self.push.register(user_id, &session_id, push_tx);

        // Register push + datagram
        self.sessions
            .register(user_id, Arc::new(QuinnDatagramTx::new(conn.clone())));

        let mut current_channel: Option<ChannelId> = None;
        defer! {
            self.push.unregister(user_id, &session_id);
            self.sessions.unregister(user_id);
        }

        // Datagram recv loop (voice)
        let voice = self.voice.clone();
        let user_for_voice = user_id;
        let conn_voice = conn.clone();
        tokio::spawn(async move {
            loop {
                match conn_voice.read_datagram().await {
                    Ok(d) => {
                        voice.handle_incoming(user_for_voice, d).await;
                    }
                    Err(_) => break,
                }
            }
        });

        let ctx = RequestContext {
            server_id,
            user_id,
            is_admin: identity.is_admin,
        };

        let media = self.media.clone();
        let conn_media = conn.clone();
        tokio::spawn(async move {
            loop {
                match conn_media.accept_bi().await {
                    Ok((send_s, recv_s)) => {
                        let media = media.clone();
                        tokio::spawn(async move {
                            if let Err(e) = media.handle_stream(send_s, recv_s, user_id).await {
                                warn!("media stream failed: {:#}", e);
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        // Request + push loop
        let res: Result<()> = async {
            loop {
                let msg: pb::ClientToServer = tokio::select! {
                push = push_rx.recv() => {
                    match push {
                        Some(push_msg) => {
                            debug!(user_id=%user_id.0, "sending server push to client session");
                            if let Err(e) = write_delimited(&mut send, &push_msg).await {
                                warn!("control push write failed: {:#}", e);
                                break;
                            }
                            continue;
                        }
                        None => break,
                    }
                }
                read = read_delimited(&mut recv, CONTROL_STREAM_MAX_MSG) => read?,
            };

            // Ping
            if let Some(pb::client_to_server::Payload::Ping(p)) = msg.payload {
                let resp = pb::ServerToClient {
                    request_id: msg.request_id,
                    session_id: Some(pb::SessionId {
                        value: session_id.clone(),
                    }),
                    sent_at: Some(now_ts()),
                    error: None,
                    event_seq: 0,
                    payload: Some(pb::server_to_client::Payload::Pong(pb::Pong {
                        nonce: p.nonce,
                        server_time: Some(now_ts()),
                    })),
                };
                if let Err(e) = write_delimited(&mut send, &resp).await {
                    warn!("control write failed: {:#}", e);
                    break;
                }
                continue;
            }

                let req_id = msg.request_id.clone();

                let req_result: Result<()> = async {
            match msg.payload {
                Some(pb::client_to_server::Payload::JoinChannelRequest(r)) => {
                    let ch = parse_channel_id(r.channel_id.as_ref())?;
                    debug!(
                        session_id = %session_id,
                        server_id = %server_id.0,
                        channel_id = %ch.0,
                        user_id = %user_id.0,
                        display_name = %identity.display_name,
                        "join_channel request"
                    );
                    let members = self
                        .control
                        .join_channel(
                            &ctx,
                            JoinChannel {
                                channel_id: ch,
                                display_name: identity.display_name.clone(),
                            },
                        )
                        .await?;
                    let chan = self.control.get_channel(&ctx, ch).await?;

                    // Update membership cache
                    let member_ids = members.iter().map(|m| m.user_id).collect::<Vec<_>>();
                    self.membership.set_channel_state(
                        ch,
                        chan.max_talkers.map(|v| v as usize).unwrap_or(4),
                        member_ids.clone(),
                    );
                    for m in &members {
                        self.membership
                            .set_user(m.user_id, ch, m.muted, m.deafened);
                    }
                    current_channel = Some(ch);

                    debug!(
                        session_id = %session_id,
                        server_id = %server_id.0,
                        channel_id = %ch.0,
                        user_id = %user_id.0,
                        member_count = members.len(),
                        "join_channel response state built"
                    );
                    for member in &members {
                        debug!(
                            session_id = %session_id,
                            channel_id = %ch.0,
                            member_user_id = %member.user_id.0,
                            member_display_name = %member.display_name,
                            "join_channel member snapshot"
                        );
                    }

                    let state = pb::ChannelState {
                        channel_id: Some(pb::ChannelId {
                            value: ch.0.to_string(),
                        }),
                        name: chan.name,
                        members: members
                            .into_iter()
                            .map(|m| pb::ChannelMember {
                                user_id: Some(pb::UserId {
                                    value: m.user_id.0.to_string(),
                                }),
                                display_name: m.display_name,
                                muted: m.muted,
                                deafened: m.deafened,
                                ..Default::default()
                            })
                            .collect(),
                        ..Default::default()
                    };

                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::JoinChannelResponse(
                            pb::JoinChannelResponse { state: Some(state) },
                        )),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::LeaveChannelRequest(r)) => {
                    let ch = parse_channel_id(r.channel_id.as_ref())?;
                    self.control.leave_channel(&ctx, ch).await?;

                    self.membership.remove_user(user_id);
                    if current_channel == Some(ch) {
                        current_channel = None;
                    }
                    // best effort update channel member list
                    if let Some(mut cur) = self.membership.members_of(ch) {
                        cur.retain(|u| *u != user_id);
                        let max = self.membership.max_talkers_of(ch).unwrap_or(4);
                        self.membership.set_channel_state(ch, max, cur);
                    }

                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::LeaveChannelResponse(
                            pb::LeaveChannelResponse {
                                channel_id: Some(pb::ChannelId {
                                    value: ch.0.to_string(),
                                }),
                            },
                        )),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::CreateChannelRequest(r)) => {
                    let parent = r
                        .parent_channel_id
                        .as_ref()
                        .and_then(|pid| uuid::Uuid::parse_str(&pid.value).ok())
                        .map(ChannelId);
                    let created = self
                        .control
                        .create_channel(
                            &ctx,
                            ChannelCreate {
                                name: r.name,
                                parent_id: parent,
                                max_members: None,
                                max_talkers: None,
                            },
                        )
                        .await?;

                    debug!(server_id=%server_id.0, channel_id=%created.id.0, user_id=%user_id.0, "create_channel request committed in control service");
                    let state = pb::ChannelState {
                        channel_id: Some(pb::ChannelId {
                            value: created.id.0.to_string(),
                        }),
                        name: created.name,
                        members: vec![],
                        ..Default::default()
                    };

                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::CreateChannelResponse(
                            pb::CreateChannelResponse { state: Some(state) },
                        )),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                    debug!(server_id=%server_id.0, channel_id=%created.id.0, user_id=%user_id.0, "create_channel response sent");
                }
                Some(pb::client_to_server::Payload::RenameChannelRequest(r)) => {
                    let ch = parse_channel_id(r.channel_id.as_ref())?;
                    let renamed = self.control.rename_channel(&ctx, ch, &r.new_name).await?;
                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::RenameChannelResponse(
                            pb::RenameChannelResponse {
                                channel: Some(pb::ChannelInfo {
                                    channel_id: Some(pb::ChannelId {
                                        value: renamed.id.0.to_string(),
                                    }),
                                    name: renamed.name,
                                    parent_channel_id: renamed.parent_id.map(|pid| pb::ChannelId {
                                        value: pid.0.to_string(),
                                    }),
                                    user_limit: renamed.max_members.unwrap_or_default().max(0)
                                        as u32,
                                    ..Default::default()
                                }),
                            },
                        )),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::DeleteChannelRequest(r)) => {
                    let ch = parse_channel_id(r.channel_id.as_ref())?;
                    self.control.delete_channel(&ctx, ch).await?;
                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::DeleteChannelResponse(
                            pb::DeleteChannelResponse {
                                channel_id: Some(pb::ChannelId {
                                    value: ch.0.to_string(),
                                }),
                            },
                        )),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::SendMessageRequest(r)) => {
                    let ch = parse_channel_id(r.channel_id.as_ref())?;
                    let attachments = serde_json::Value::Array(
                        r.attachments
                            .into_iter()
                            .map(|a| {
                                serde_json::json!({
                                    "asset_id": a.asset_id.map(|x| x.value).unwrap_or_default(),
                                    "filename": a.filename,
                                    "mime_type": a.mime_type,
                                    "size_bytes": a.size_bytes,
                                    "width": a.width,
                                    "height": a.height,
                                    "duration_ms": a.duration_ms,
                                    "sha256": a.sha256,
                                })
                            })
                            .collect(),
                    );
                    let _posted = self
                        .control
                        .send_message(
                            &ctx,
                            SendMessage {
                                channel_id: ch,
                                text: r.text,
                                attachments: Some(attachments),
                            },
                        )
                        .await?;

                    // Ack only; the actual delivery is via outbox push.
                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: None,
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::ModerationActionRequest(r)) => {
                    let ch = parse_channel_id(r.channel_id.as_ref())?;
                    let target = r
                        .target_user_id
                        .as_ref()
                        .ok_or_else(|| anyhow!("target_user_id missing"))?;
                    let target = UserId(
                        uuid::Uuid::parse_str(&target.value).context("invalid target_user_id")?,
                    );

                    if let Some(action) = r.action {
                        match action {
                            pb::moderation_action_request::Action::Mute(m) => {
                                tracing::info!(actor=%ctx.user_id.0,target=%target.0,channel=%ch.0,muted=m.muted,"moderation mute action");
                                let _ = self
                                    .control
                                    .set_voice_mute(&ctx, ch, target, m.muted, None)
                                    .await?;
                                self.membership.update_mute(target, ch, m.muted);
                            }
                            pb::moderation_action_request::Action::Deafen(m) => {
                                tracing::info!(actor=%ctx.user_id.0,target=%target.0,channel=%ch.0,deafened=m.deafened,"moderation deafen action");
                                let _ = self
                                    .control
                                    .set_voice_deafen(&ctx, ch, target, m.deafened, None)
                                    .await?;
                                self.membership.update_deafen(target, ch, m.deafened);
                            }
                            pb::moderation_action_request::Action::Kick(k) => {
                                tracing::info!(actor=%ctx.user_id.0,target=%target.0,channel=%ch.0,"moderation kick action");
                                self.control
                                    .kick_member(&ctx, ch, target, Some(k.reason))
                                    .await?;
                            }
                            _ => {}
                        }
                    }

                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: None,
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::PokeRequest(r)) => {
                    let target = r
                        .target_user_id
                        .as_ref()
                        .ok_or_else(|| anyhow!("target_user_id missing"))?;
                    let target = UserId(
                        uuid::Uuid::parse_str(&target.value).context("invalid target_user_id")?,
                    );
                    tracing::info!(actor=%ctx.user_id.0,target=%target.0,"poke request");
                    self.control
                        .poke_user(&ctx, target, &identity.display_name, r.message)
                        .await?;
                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::PokeResponse(
                            pb::PokeResponse {},
                        )),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::GetInitialStateSnapshotRequest(_)) => {
                    let snapshot = self
                        .build_initial_snapshot(server_id, user_id, &identity.display_name)
                        .await?;
                    debug!(
                        session_id = %session_id,
                        server_id = %server_id.0,
                        user_id = %user_id.0,
                        channel_count = snapshot.channels.len(),
                        member_scope_count = snapshot.channel_members.len(),
                        "responding with authoritative snapshot"
                    );
                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: snapshot.snapshot_version,
                        payload: Some(pb::server_to_client::Payload::InitialStateSnapshot(
                            snapshot,
                        )),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                _ => {
                    // Ignore other messages for now.
                }
            }

                    Ok(())
                }
                .await;

                if let Err(err) = req_result {
                    let (code, message) = classify_request_error(&err);
                    warn!(
                        session_id = %session_id,
                        user_id = %user_id.0,
                        request_id = ?req_id.as_ref().map(|r| r.value),
                        code = ?code,
                        "control request failed: {:#}",
                        err
                    );

                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: Some(pb::Error {
                            code: code as i32,
                            message,
                            detail: err.to_string(),
                        }),
                        event_seq: 0,
                        payload: None,
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
            }

            Ok(())
        }
        .await;

        if let Some(ch) = current_channel {
            if let Err(e) = self.control.leave_channel(&ctx, ch).await {
                warn!(
                    user_id = %user_id.0,
                    channel_id = %ch.0,
                    error = %e,
                    "disconnect cleanup leave_channel failed"
                );
            } else {
                self.membership.remove_user(user_id);
                if let Some(mut cur) = self.membership.members_of(ch) {
                    cur.retain(|u| *u != user_id);
                    let max = self.membership.max_talkers_of(ch).unwrap_or(4);
                    self.membership.set_channel_state(ch, max, cur);
                }
            }
        }

        res
    }

    async fn do_hello(
        &self,
        send: &mut quinn::SendStream,
        recv: &mut quinn::RecvStream,
    ) -> Result<(String, Option<pb::ClientCaps>, Vec<u8>)> {
        let req: pb::ClientToServer = read_delimited(recv, CONTROL_STREAM_MAX_MSG)
            .await
            .context("read Hello envelope")?;

        let hello = match req.payload {
            Some(pb::client_to_server::Payload::Hello(h)) => h,
            _ => return Err(anyhow!("expected Hello as first message")),
        };

        let session_id = uuid::Uuid::new_v4().to_string();

        let mut auth_challenge = [0u8; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut auth_challenge)
            .map_err(|_| anyhow::anyhow!("failed to generate auth challenge"))?;

        let ack = pb::HelloAck {
            session_id: Some(pb::SessionId {
                value: session_id.clone(),
            }),
            max_message_size_bytes: 64 * 1024,
            max_upload_size_bytes: 50 * 1024 * 1024,
            ping_interval_ms: 15_000,
            auth_challenge: auth_challenge.to_vec(),
        };

        let resp = pb::ServerToClient {
            request_id: req.request_id,
            session_id: Some(pb::SessionId {
                value: session_id.clone(),
            }),
            sent_at: Some(now_ts()),
            error: None,
            event_seq: 0,
            payload: Some(pb::server_to_client::Payload::HelloAck(ack)),
        };

        write_delimited(send, &resp)
            .await
            .context("write HelloAck")?;
        Ok((session_id, hello.caps, auth_challenge.to_vec()))
    }

    async fn do_auth(
        &self,
        send: &mut quinn::SendStream,
        recv: &mut quinn::RecvStream,
        session_id: &str,
        auth_challenge: &[u8],
    ) -> Result<AuthedIdentity> {
        let req: pb::ClientToServer = read_delimited(recv, CONTROL_STREAM_MAX_MSG)
            .await
            .context("read Auth envelope")?;

        let auth_req = match req.payload {
            Some(pb::client_to_server::Payload::AuthRequest(a)) => a,
            _ => return Err(anyhow!("expected AuthRequest as second message")),
        };

        let mut identity = self
            .auth
            .authenticate(&auth_req, session_id, auth_challenge)
            .await
            .context("auth failed")?;
        if let Some(preferred) = normalize_preferred_display_name(&auth_req.preferred_display_name)
        {
            identity.display_name = preferred;
        }

        let auth_resp = pb::AuthResponse {
            user_id: Some(pb::UserId {
                value: identity.user_id.clone(),
            }),
            server_id: Some(pb::ServerId {
                value: identity.server_id.clone(),
            }),
            is_admin: identity.is_admin,
        };

        let resp = pb::ServerToClient {
            request_id: req.request_id,
            session_id: Some(pb::SessionId {
                value: session_id.to_string(),
            }),
            sent_at: Some(now_ts()),
            error: None,
            event_seq: 0,
            payload: Some(pb::server_to_client::Payload::AuthResponse(auth_resp)),
        };

        write_delimited(send, &resp)
            .await
            .context("write AuthResponse")?;
        Ok(identity)
    }
}

impl Gateway {
    async fn build_initial_snapshot(
        &self,
        server_id: ServerId,
        user_id: UserId,
        self_display_name: &str,
    ) -> Result<pb::InitialStateSnapshot> {
        let mut tx = self.control.repo().tx().await?;
        let channels =
            <PgControlRepo as ControlRepo>::list_channels(self.control.repo(), &mut tx, server_id)
                .await?;

        let mut channel_snapshots = Vec::with_capacity(channels.len());
        let mut members_scoped = Vec::with_capacity(channels.len());
        let mut default_channel_id = None;

        for (idx, channel) in channels.iter().enumerate() {
            if idx == 0 {
                default_channel_id = Some(pb::ChannelId {
                    value: channel.id.0.to_string(),
                });
            }

            channel_snapshots.push(pb::ChannelSnapshot {
                info: Some(pb::ChannelInfo {
                    channel_id: Some(pb::ChannelId {
                        value: channel.id.0.to_string(),
                    }),
                    name: channel.name.clone(),
                    parent_channel_id: channel.parent_id.map(|pid| pb::ChannelId {
                        value: pid.0.to_string(),
                    }),
                    user_limit: channel.max_members.unwrap_or_default().max(0) as u32,
                    ..Default::default()
                }),
            });

            let members = <PgControlRepo as ControlRepo>::list_members(
                self.control.repo(),
                &mut tx,
                server_id,
                channel.id,
            )
            .await?;
            let pb_members = members
                .into_iter()
                .map(|m| pb::ChannelMember {
                    user_id: Some(pb::UserId {
                        value: m.user_id.0.to_string(),
                    }),
                    display_name: m.display_name,
                    muted: m.muted,
                    deafened: m.deafened,
                    ..Default::default()
                })
                .collect::<Vec<_>>();

            members_scoped.push(pb::ChannelMembersSnapshot {
                channel_id: Some(pb::ChannelId {
                    value: channel.id.0.to_string(),
                }),
                members: pb_members,
            });
        }
        tx.commit().await?;

        info!(
            server_id = %server_id.0,
            auth_user_id = %user_id.0,
            channels = channel_snapshots.len(),
            members_scopes = members_scoped.len(),
            "initial snapshot prepared"
        );

        Ok(pb::InitialStateSnapshot {
            server_id: Some(pb::ServerId {
                value: server_id.0.to_string(),
            }),
            server_name: "TSOD".to_string(),
            self_user_id: Some(pb::UserId {
                value: user_id.0.to_string(),
            }),
            self_display_name: self_display_name.to_string(),
            channels: channel_snapshots,
            channel_members: members_scoped,
            default_channel_id,
            snapshot_version: unix_ms_u64(),
        })
    }
}

fn normalize_preferred_display_name(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(64).collect())
}

fn parse_channel_id(ch: Option<&pb::ChannelId>) -> Result<ChannelId> {
    let ch = ch.ok_or_else(|| anyhow!("channel_id missing"))?;
    Ok(ChannelId(
        uuid::Uuid::parse_str(&ch.value).context("invalid channel_id")?,
    ))
}

fn classify_request_error(err: &anyhow::Error) -> (pb::error::Code, String) {
    if let Some(ctrl) = err.downcast_ref::<ControlError>() {
        return match ctrl {
            ControlError::PermissionDenied(msg) => {
                (pb::error::Code::PermissionDenied, msg.to_string())
            }
            ControlError::NotFound(msg) => (pb::error::Code::NotFound, msg.to_string()),
            ControlError::AlreadyExists(msg) => (pb::error::Code::AlreadyExists, msg.to_string()),
            ControlError::InvalidArgument(msg) => {
                (pb::error::Code::InvalidArgument, msg.to_string())
            }
            ControlError::ResourceExhausted(msg) => {
                (pb::error::Code::ResourceExhausted, msg.to_string())
            }
            ControlError::FailedPrecondition(msg) => {
                (pb::error::Code::FailedPrecondition, msg.to_string())
            }
            ControlError::Db(_) | ControlError::Anyhow(_) => (
                pb::error::Code::Internal,
                "internal server error".to_string(),
            ),
        };
    }

    (
        pb::error::Code::Internal,
        "internal server error".to_string(),
    )
}

fn now_ts() -> pb::Timestamp {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    pb::Timestamp { unix_millis: ms }
}

fn unix_ms_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::normalize_preferred_display_name;

    #[test]
    fn preferred_display_name_is_trimmed_and_limited() {
        assert_eq!(normalize_preferred_display_name("   "), None);
        assert_eq!(
            normalize_preferred_display_name("  Overdose  "),
            Some("Overdose".to_string())
        );

        let long = "x".repeat(80);
        let normalized = normalize_preferred_display_name(&long).unwrap();
        assert_eq!(normalized.len(), 64);
    }
}
