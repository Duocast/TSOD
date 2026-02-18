use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use std::{sync::Arc};
use tokio::{sync::mpsc, time::{timeout, Duration}};
use tracing::{info, warn};

use crate::{
    auth::{AuthProvider, AuthedIdentity},
    frame::read_delimited,
    proto::voiceplatform::v1 as pb,
    state::{GatewayState, QuinnDatagramTx, MemberState, mk_ctx, control_join, control_create_channel},
};

const CONTROL_STREAM_MAX_MSG: usize = 256 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Gateway {
    auth: Arc<dyn AuthProvider>,
    alpn: Vec<u8>,
    state: Arc<GatewayState>,
}

impl Gateway {
    pub fn new(auth: Arc<dyn AuthProvider>, alpn: String, state: Arc<GatewayState>) -> Self {
        Self { auth, alpn: alpn.into_bytes(), state }
    }

    pub async fn serve(self, endpoint: quinn::Endpoint) -> Result<()> {
        info!("gateway listening");
        loop {
            let incoming = endpoint.accept().await.ok_or_else(|| anyhow!("endpoint closed"))?;
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

        let negotiated = conn
            .handshake_data()
            .and_then(|d| d.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
            .and_then(|d| d.protocol);

        if negotiated.as_deref() != Some(&self.alpn[..]) {
            return Err(anyhow!("ALPN mismatch: got {:?}, want {:?}", negotiated, self.alpn));
        }

        let remote = conn.remote_address();
        info!(%remote, "connected");

        let (send, mut recv) = timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
            .await
            .context("control accept_bi timeout")?
            .context("accept_bi failed")?;

        // Single writer task for this connection’s SendStream
        let (tx_out, mut rx_out) = mpsc::channel::<pb::ServerToClient>(512);
        let mut send_stream = send;
        tokio::spawn(async move {
            while let Some(msg) = rx_out.recv().await {
                if let Err(e) = crate::frame::write_delimited(&mut send_stream, &msg).await {
                    warn!("control writer failed: {e:#}");
                    break;
                }
            }
        });

        // Hello + Auth
        let (session_id, _caps) = self.do_hello(&tx_out, &mut recv).await?;
        let identity = self.do_auth(&tx_out, &mut recv, &session_id).await?;

        let user_uuid = identity.user_id.parse::<uuid::Uuid>().context("user_id must be UUID")?;
        let server_uuid = identity.server_id.parse::<uuid::Uuid>().context("server_id must be UUID")?;
        let user_id = vp_control::ids::UserId(user_uuid);
        let server_id = vp_control::ids::ServerId(server_uuid);

        // Register push + voice tx
        self.state.pushes.register(user_id, tx_out.clone()).await;
        self.state.sessions.register(user_id, Arc::new(QuinnDatagramTx { conn: conn.clone() })).await;

        info!(%remote, session_id=%session_id, user=%identity.user_id, "authenticated");

        // Start datagram read loop -> voice forwarder
        let vf = self.state.voice.clone();
        tokio::spawn(async move {
            loop {
                match conn.read_datagram().await {
                    Ok(d) => vf.handle_incoming(user_id, d).await,
                    Err(_) => break,
                }
            }
        });

        // Control loop: join/leave/create/ping/chat
        let ctx = mk_ctx(server_id, user_id, identity.is_admin);

        loop {
            let msg: pb::ClientToServer = match read_delimited(&mut recv, CONTROL_STREAM_MAX_MSG).await {
                Ok(m) => m,
                Err(e) => return Err(e),
            };

            let rid = msg.request_id.clone();
            let base = pb::ServerToClient {
                request_id: rid.clone(),
                session_id: Some(pb::SessionId { value: session_id.clone() }),
                sent_at: Some(now_ts()),
                error: None,
                payload: None,
            };

            match msg.payload {
                Some(pb::client_to_server::Payload::Ping(p)) => {
                    let resp = pb::ServerToClient {
                        payload: Some(pb::server_to_client::Payload::Pong(pb::Pong {
                            nonce: p.nonce,
                            server_time: Some(now_ts()),
                        })),
                        ..base
                    };
                    let _ = tx_out.send(resp).await;
                }

                Some(pb::client_to_server::Payload::JoinChannelRequest(req)) => {
                    let ch = vp_control::ids::ChannelId(req.channel_id.value.parse::<uuid::Uuid>()?);

                    // Call control plane (perms + DB)
                    let members = control_join(&self.state.control, &ctx, ch, identity.user_id.clone()).await?;

                    // Convert members -> cache + response
                    let mut cache_members = vec![];
                    let mut pb_members = vec![];

                    for m in &members {
                        cache_members.push((m.user_id, MemberState {
                            display_name: m.display_name.clone(),
                            muted: m.muted,
                            deafened: m.deafened,
                        }));
                        pb_members.push(pb::ChannelMember {
                            user_id: Some(pb::UserId { value: m.user_id.0.to_string() }),
                            display_name: m.display_name.clone(),
                            muted: m.muted,
                            deafened: m.deafened,
                        });
                    }

                    let max_talkers = 4; // TODO: read from channel row (extend repo/model if needed)
                    self.state.membership.set_channel_state(ch, cache_members, max_talkers).await;

                    // Response
                    let resp = pb::ServerToClient {
                        payload: Some(pb::server_to_client::Payload::JoinChannelResponse(
                            pb::JoinChannelResponse {
                                state: Some(pb::ChannelState {
                                    channel_id: Some(pb::ChannelId { value: ch.0.to_string() }),
                                    name: "(server)".into(), // TODO: fetch channel name via repo
                                    members: pb_members.clone(),
                                }),
                            }
                        )),
                        ..base
                    };
                    let _ = tx_out.send(resp).await;

                    // Server push PresenceEvent to other members
                    let pres = pb::ServerToClient {
                        request_id: Some(pb::RequestId { value: 0 }),
                        session_id: Some(pb::SessionId { value: session_id.clone() }),
                        sent_at: Some(now_ts()),
                        error: None,
                        payload: Some(pb::server_to_client::Payload::PresenceEvent(pb::PresenceEvent {
                            at: Some(now_ts()),
                            kind: Some(pb::presence_event::Kind::MemberJoined(pb::MemberJoined {
                                channel_id: Some(pb::ChannelId { value: ch.0.to_string() }),
                                member: Some(pb::ChannelMember {
                                    user_id: Some(pb::UserId { value: user_id.0.to_string() }),
                                    display_name: identity.user_id.clone(),
                                    muted: false,
                                    deafened: false,
                                }),
                            })),
                        })),
                    };

                    let members_uids = self.state.membership.members(ch).await;
                    for u in members_uids {
                        if u != user_id {
                            self.state.pushes.send_to(u, pres.clone()).await;
                        }
                    }
                }

                Some(pb::client_to_server::Payload::LeaveChannelRequest(req)) => {
                    let ch = vp_control::ids::ChannelId(req.channel_id.value.parse::<uuid::Uuid>()?);
                    // TODO: call control.leave_channel(ctx, ch) (exists in vp-control)
                    self.state.membership.remove_member(ch, user_id).await;

                    let resp = pb::ServerToClient {
                        payload: Some(pb::server_to_client::Payload::LeaveChannelResponse(
                            pb::LeaveChannelResponse { channel_id: Some(pb::ChannelId { value: ch.0.to_string() }) }
                        )),
                        ..base
                    };
                    let _ = tx_out.send(resp).await;

                    // push MemberLeft
                    let pres = pb::ServerToClient {
                        request_id: Some(pb::RequestId { value: 0 }),
                        session_id: Some(pb::SessionId { value: session_id.clone() }),
                        sent_at: Some(now_ts()),
                        error: None,
                        payload: Some(pb::server_to_client::Payload::PresenceEvent(pb::PresenceEvent {
                            at: Some(now_ts()),
                            kind: Some(pb::presence_event::Kind::MemberLeft(pb::MemberLeft {
                                channel_id: Some(pb::ChannelId { value: ch.0.to_string() }),
                                user_id: Some(pb::UserId { value: user_id.0.to_string() }),
                            })),
                        })),
                    };

                    let members_uids = self.state.membership.members(ch).await;
                    for u in members_uids {
                        if u != user_id {
                            self.state.pushes.send_to(u, pres.clone()).await;
                        }
                    }
                }

                Some(pb::client_to_server::Payload::CreateChannelRequest(req)) => {
                    let parent = if req.parent_channel_id.value.is_empty() {
                        None
                    } else {
                        Some(vp_control::ids::ChannelId(req.parent_channel_id.value.parse::<uuid::Uuid>()?))
                    };

                    let ch = control_create_channel(&self.state.control, &ctx, req.name.clone(), parent).await?;

                    let resp = pb::ServerToClient {
                        payload: Some(pb::server_to_client::Payload::CreateChannelResponse(
                            pb::CreateChannelResponse {
                                state: Some(pb::ChannelState {
                                    channel_id: Some(pb::ChannelId { value: ch.id.0.to_string() }),
                                    name: ch.name,
                                    members: vec![],
                                }),
                            }
                        )),
                        ..base
                    };
                    let _ = tx_out.send(resp).await;
                }

                _ => {
                    // Unknown/unsupported request: reply INVALID_ARGUMENT
                    let err = pb::Error {
                        code: pb::error::Code::InvalidArgument as i32,
                        message: "unsupported request".into(),
                        detail: "".into(),
                    };
                    let resp = pb::ServerToClient { error: Some(err), ..base };
                    let _ = tx_out.send(resp).await;
                }
            }
        }
    }

    async fn do_hello(
        &self,
        tx_out: &mpsc::Sender<pb::ServerToClient>,
        recv: &mut quinn::RecvStream,
    ) -> Result<(String, Option<pb::ClientCaps>)> {
        let req: pb::ClientToServer =
            crate::frame::read_delimited(recv, CONTROL_STREAM_MAX_MSG).await.context("read Hello envelope")?;

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

        let _ = tx_out.send(resp).await;
        Ok((session_id, hello.caps))
    }

    async fn do_auth(
        &self,
        tx_out: &mpsc::Sender<pb::ServerToClient>,
        recv: &mut quinn::RecvStream,
        session_id: &str,
    ) -> Result<AuthedIdentity> {
        let req: pb::ClientToServer =
            crate::frame::read_delimited(recv, CONTROL_STREAM_MAX_MSG).await.context("read Auth envelope")?;

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

        let _ = tx_out.send(resp).await;
        Ok(identity)
    }
}

fn now_ts() -> pb::Timestamp {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    pb::Timestamp { unix_millis: ms }
}
