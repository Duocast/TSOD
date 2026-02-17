use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::{
    auth::{AuthProvider, AuthedIdentity},
    frame::{read_delimited, write_delimited},
    proto::voiceplatform::v1 as pb,
};

const CONTROL_STREAM_MAX_MSG: usize = 256 * 1024; // 256KB
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Gateway {
    auth: Arc<dyn AuthProvider>,
    alpn: Vec<u8>,
}

impl Gateway {
    pub fn new(auth: Arc<dyn AuthProvider>, alpn: String) -> Self {
        Self {
            auth,
            alpn: alpn.into_bytes(),
        }
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

        // ALPN check (defense-in-depth). Quinn/rustls already negotiates ALPN,
        // but this ensures your app rejects unexpected protocols.
        let negotiated = conn
            .handshake_data()
            .and_then(|d| d.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
            .and_then(|d| d.protocol);

        if negotiated.as_deref() != Some(&self.alpn[..]) {
            return Err(anyhow!("ALPN mismatch: got {:?}, want {:?}", negotiated, self.alpn));
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

        info!(
            %remote,
            session_id = %session_id,
            user_id = %identity.user_id,
            "authenticated"
        );

        // From here, you can hand off to your control-plane dispatcher.
        // For now, just keep reading until EOF.
        loop {
            let msg: pb::ClientToServer = match read_delimited(&mut recv, CONTROL_STREAM_MAX_MSG).await
            {
                Ok(m) => m,
                Err(e) => {
                    // likely EOF/reset
                    return Err(e);
                }
            };

            // Minimal ping/pong support.
            if let Some(pb::client_to_server::Payload::Ping(p)) = msg.payload {
                let resp = pb::ServerToClient {
                    request_id: msg.request_id,
                    session_id: Some(pb::SessionId { value: session_id.clone() }),
                    sent_at: Some(now_ts()),
                    error: None,
                    payload: Some(pb::server_to_client::Payload::Pong(pb::Pong {
                        nonce: p.nonce,
                        server_time: Some(now_ts()),
                    })),
                };
                write_delimited(&mut send, &resp).await?;
            } else {
                // Ignore other messages for now (join/chat/moderation will come later).
            }
        }
    }

    async fn do_hello(
        &self,
        send: &mut quinn::SendStream,
        recv: &mut quinn::RecvStream,
    ) -> Result<(String, Option<pb::ClientCaps>)> {
        let req: pb::ClientToServer =
            read_delimited(recv, CONTROL_STREAM_MAX_MSG).await.context("read Hello envelope")?;

        let hello = match req.payload {
            Some(pb::client_to_server::Payload::Hello(h)) => h,
            _ => return Err(anyhow!("expected Hello as first message")),
        };

        let session_id = uuid::Uuid::new_v4().to_string();

        let ack = pb::HelloAck {
            session_id: Some(pb::SessionId {
                value: session_id.clone(),
            }),
            max_message_size_bytes: 64 * 1024,
            max_upload_size_bytes: 50 * 1024 * 1024,
            ping_interval_ms: 15_000,
        };

        let resp = pb::ServerToClient {
            request_id: req.request_id,
            session_id: Some(pb::SessionId {
                value: session_id.clone(),
            }),
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
        let req: pb::ClientToServer =
            read_delimited(recv, CONTROL_STREAM_MAX_MSG).await.context("read Auth envelope")?;

        let auth_req = match req.payload {
            Some(pb::client_to_server::Payload::AuthRequest(a)) => a,
            _ => return Err(anyhow!("expected AuthRequest as second message")),
        };

        let identity = self
            .auth
            .authenticate(&auth_req)
            .context("auth failed")?;

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
            payload: Some(pb::server_to_client::Payload::AuthResponse(auth_resp)),
        };

        write_delimited(send, &resp).await.context("write AuthResponse")?;
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
