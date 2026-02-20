use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::{
    auth::{AuthProvider, AuthedIdentity},
    frame::{read_delimited, write_delimited},
    proto::voiceplatform::v1 as pb,
state::{MembershipCache, PushHub, QuinnDatagramTx, Sessions},
};

use vp_control::ids::{ChannelId, ServerId, UserId};
use vp_control::model::{ChannelCreate, JoinChannel, SendMessage};
use vp_control::{ControlService, PgControlRepo, RequestContext};
use vp_media::voice_forwarder::VoiceForwarder;

const CONTROL_STREAM_MAX_MSG: usize = 256 * 1024; // 256KB
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Gateway {
    auth: Arc<dyn AuthProvider>,
    alpn: Vec<u8>,
    control: Arc<ControlService<PgControlRepo>>,
    sessions: Sessions,
    membership: MembershipCache,
    voice: Arc<VoiceForwarder>,
}

impl Gateway {
    pub fn new(
        auth: Arc<dyn AuthProvider>,
        alpn: String,
        control: Arc<ControlService<PgControlRepo>>,
        sessions: Sessions,
        membership: MembershipCache,
        voice: Arc<VoiceForwarder>,
    ) -> Self {
        Self {
            auth,
            alpn: alpn.into_bytes(),
            control,
            sessions,
            membership,
            voice,
        }
    }

    pub async fn serve(self, endpoint: quinn::Endpoint) -> Result<()> {
        info!("gateway listening");
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

        let (session_id, _hello_caps) = self.do_hello(&mut send, &mut recv).await?;
        let identity = self.do_auth(&mut send, &mut recv, &session_id).await?;

        let user_id =
            UserId(uuid::Uuid::parse_str(&identity.user_id).context("invalid user_id uuid")?);
        let server_id =
            ServerId(uuid::Uuid::parse_str(&identity.server_id).context("invalid server_id uuid")?);

        info!(%remote, session_id=%session_id, user_id=%identity.user_id, "authenticated");

        // Control stream writes are performed inline in this task.

        // Register push + datagram
        self.sessions
            .register(user_id, Arc::new(QuinnDatagramTx::new(conn.clone())));

        // Datagram recv loop (voice)
        let voice = self.voice.clone();
        let user_for_voice = user_id;
        tokio::spawn(async move {
            loop {
                match conn.read_datagram().await {
                    Ok(d) => {
                        voice.handle_incoming(user_for_voice, d).await;
                    }
                    Err(_) => break,
                }
            }
        });

        // Request loop
        loop {
            let msg: pb::ClientToServer = read_delimited(&mut recv, CONTROL_STREAM_MAX_MSG).await?;

            // Ping
            if let Some(pb::client_to_server::Payload::Ping(p)) = msg.payload {
                let resp = pb::ServerToClient {
                    request_id: msg.request_id,
                    session_id: Some(pb::SessionId {
                        value: session_id.clone(),
                    }),
                    sent_at: Some(now_ts()),
                    error: None,
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

            let ctx = RequestContext {
                server_id,
                user_id,
                is_admin: identity.is_admin,
            };
            let req_id = msg.request_id.clone();

            match msg.payload {
                Some(pb::client_to_server::Payload::JoinChannelRequest(r)) => {
                    let ch = parse_channel_id(r.channel_id.as_ref())?;
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
                        self.membership.set_user(m.user_id, ch, m.muted);
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
                            })
                            .collect(),
                    };

                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId { value: session_id.clone() }),
                        sent_at: Some(now_ts()),
                        error: None,
                        payload: Some(pb::server_to_client::Payload::JoinChannelResponse(pb::JoinChannelResponse { state: Some(state) })),
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
                        payload: Some(pb::server_to_client::Payload::LeaveChannelResponse(
                            pb::LeaveChannelResponse {
                                channel_id: Some(pb::ChannelId { value: ch.0.to_string() }),
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

                    let state = pb::ChannelState {
                        channel_id: Some(pb::ChannelId {
                            value: created.id.0.to_string(),
                        }),
                        name: created.name,
                        members: vec![],
                    };

                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: None,
                        payload: Some(pb::server_to_client::Payload::JoinChannelResponse(
                            pb::JoinChannelResponse { state: Some(state) },
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
                        payload: None,
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::ModerationActionRequest(r)) => {
                    let ch = parse_channel_id(r.channel_id.as_ref())?;
                    let target = r.target_user_id.as_ref().ok_or_else(|| anyhow!("target_user_id missing"))?;
                    let target = UserId(uuid::Uuid::parse_str(&target.value).context("invalid target_user_id")?);

                    if let Some(pb::moderation_action_request::Action::Mute(m)) = r.action {
                        let _ = self.control.set_mute(&ctx, ch, target, m.muted).await?;
                        self.membership.update_mute(target, ch, m.muted);
                    }

                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId { value: session_id.clone() }),
                        sent_at: Some(now_ts()),
                        error: None,
                        payload: None,
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
        }
    }

    async fn do_hello(
        &self,
        send: &mut quinn::SendStream,
        recv: &mut quinn::RecvStream,
    ) -> Result<(String, Option<pb::ClientCaps>)> {
        let req: pb::ClientToServer = read_delimited(recv, CONTROL_STREAM_MAX_MSG).await.context("read Hello envelope")?;

        let hello = match req.payload {
            Some(pb::client_to_server::Payload::Hello(h)) => h,
            _ => return Err(anyhow!("expected Hello as first message")),
        };

        let session_id = uuid::Uuid::new_v4().to_string();

        let ack = pb::HelloAck {
            session_id: Some(pb::SessionId { value: session_id.clone() }),
            max_message_size_bytes: 64 * 1024,
            max_upload_size_bytes: 50 * 1024 * 1024,
            ping_interval_ms: 15_000,
        };

        let resp = pb::ServerToClient {
            request_id: req.request_id,
            session_id: Some(pb::SessionId { value: session_id.clone() }),
            sent_at: Some(now_ts()),
            error: None,
            payload: Some(pb::server_to_client::Payload::HelloAck(ack)),
        };

        write_delimited(send, &resp).await.context("write HelloAck")?;
        Ok((session_id, hello.caps))
    }

    async fn do_auth(
        &self,
        send: &mut quinn::SendStream,
        recv: &mut quinn::RecvStream,
        session_id: &str,
    ) -> Result<AuthedIdentity> {
        let req: pb::ClientToServer = read_delimited(recv, CONTROL_STREAM_MAX_MSG).await.context("read Auth envelope")?;

        let auth_req = match req.payload {
            Some(pb::client_to_server::Payload::AuthRequest(a)) => a,
            _ => return Err(anyhow!("expected AuthRequest as second message")),
        };

        let identity = self.auth.authenticate(&auth_req).context("auth failed")?;

        let auth_resp = pb::AuthResponse {
            user_id: Some(pb::UserId { value: identity.user_id.clone() }),
            server_id: Some(pb::ServerId { value: identity.server_id.clone() }),
            is_admin: identity.is_admin,
        };

        let resp = pb::ServerToClient {
            request_id: req.request_id,
            session_id: Some(pb::SessionId { value: session_id.to_string() }),
            sent_at: Some(now_ts()),
            error: None,
            payload: Some(pb::server_to_client::Payload::AuthResponse(auth_resp)),
        };

        write_delimited(send, &resp).await.context("write AuthResponse")?;
        Ok(identity)
    }
}

fn parse_channel_id(ch: Option<&pb::ChannelId>) -> Result<ChannelId> {
    let ch = ch.ok_or_else(|| anyhow!("channel_id missing"))?;
    Ok(ChannelId(uuid::Uuid::parse_str(&ch.value).context("invalid channel_id")?))
}

fn now_ts() -> pb::Timestamp {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    pb::Timestamp { unix_millis: ms }
}
