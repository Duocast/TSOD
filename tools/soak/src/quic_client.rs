use anyhow::{anyhow, Result};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::time::Duration;

use crate::pb;

const MAX_CTRL_MSG: usize = 256 * 1024;

pub struct Ctrl {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    next_req: u64,
    session_id: Option<pb::SessionId>,
}

impl Ctrl {
    pub fn new(send: quinn::SendStream, recv: quinn::RecvStream) -> Self {
        Self { send, recv, next_req: 1, session_id: None }
    }

    pub async fn hello_auth(&mut self, alpn: &str, dev_token: &str) -> Result<()> {
        let hello = pb::Hello {
            caps: Some(default_caps(alpn)),
            device_id: Some(pb::DeviceId { value: "soak-tool".into() }),
        };

        let resp = self.req(pb::client_to_server::Payload::Hello(hello), Duration::from_secs(5)).await?;
        match resp.payload {
            Some(pb::server_to_client::Payload::HelloAck(ack)) => {
                self.session_id = ack.session_id;
            }
            _ => return Err(anyhow!("expected HelloAck")),
        }

        let auth = pb::AuthRequest {
            method: Some(pb::auth_request::Method::DevToken(pb::DevTokenAuth{ token: dev_token.into() })),
        };
        let resp = self.req(pb::client_to_server::Payload::AuthRequest(auth), Duration::from_secs(5)).await?;
        if resp.error.is_some() {
            return Err(anyhow!("auth error: {:?}", resp.error));
        }
        Ok(())
    }

    pub async fn join(&mut self, channel_id: &str) -> Result<()> {
        let req = pb::JoinChannelRequest { channel_id: Some(pb::ChannelId { value: channel_id.into() }) };
        let resp = self.req(pb::client_to_server::Payload::JoinChannelRequest(req), Duration::from_secs(5)).await?;
        if resp.error.is_some() {
            return Err(anyhow!("join error: {:?}", resp.error));
        }
        Ok(())
    }

    pub async fn ping(&mut self) -> Result<()> {
        let nonce = rand::random::<u64>();
        let resp = self.req(pb::client_to_server::Payload::Ping(pb::Ping{ nonce }), Duration::from_secs(2)).await?;
        match resp.payload {
            Some(pb::server_to_client::Payload::Pong(p)) if p.nonce == nonce => Ok(()),
            _ => Err(anyhow!("bad pong")),
        }
    }

    async fn req(&mut self, payload: pb::client_to_server::Payload, _timeout: Duration) -> Result<pb::ServerToClient> {
        let rid = self.next_req;
        self.next_req += 1;

        let msg = pb::ClientToServer {
            request_id: Some(pb::RequestId { value: rid }),
            session_id: self.session_id.clone(),
            sent_at: Some(now_ts()),
            payload: Some(payload),
        };

        write_delimited(&mut self.send, &msg).await?;
        read_delimited(&mut self.recv, MAX_CTRL_MSG).await
    }
}

async fn read_delimited<M: Message + Default>(recv: &mut quinn::RecvStream, max_size: usize) -> Result<M> {
    let len = read_varint_u64(recv).await? as usize;
    if len == 0 || len > max_size { return Err(anyhow!("bad len {len}")); }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(M::decode(&buf[..])?)
}

async fn write_delimited<M: Message>(send: &mut quinn::SendStream, msg: &M) -> Result<()> {
    let mut body = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut body)?;
    write_varint_u64(send, body.len() as u64).await?;
    send.write_all(&body).await?;
    send.flush().await?;
    Ok(())
}

async fn read_varint_u64(recv: &mut quinn::RecvStream) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    for _ in 0..10 {
        let mut b = [0u8; 1];
        recv.read_exact(&mut b).await?;
        let byte = b[0];
        result |= ((byte & 0x7f) as u64) << shift;
        if (byte & 0x80) == 0 { return Ok(result); }
        shift += 7;
    }
    Err(anyhow!("varint too long"))
}

async fn write_varint_u64(send: &mut quinn::SendStream, mut v: u64) -> Result<()> {
    let mut buf = [0u8; 10];
    let mut i = 0;
    while v >= 0x80 {
        buf[i] = (v as u8) | 0x80;
        v >>= 7;
        i += 1;
    }
    buf[i] = v as u8;
    i += 1;
    send.write_all(&buf[..i]).await?;
    Ok(())
}

fn now_ts() -> pb::Timestamp {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    pb::Timestamp { unix_millis: ms }
}

fn default_caps(alpn: &str) -> pb::ClientCaps {
    pb::ClientCaps {
        build: Some(pb::BuildInfo {
            client_name: "vp-soak".into(),
            client_version: "0.1.0".into(),
            platform: std::env::consts::OS.into(),
            git_sha: "".into(),
        }),
        features: Some(pb::FeatureCaps {
            supports_quic_datagrams: true,
            supports_voice_fec: false,
            supports_streaming: false,
            supports_drag_drop_upload: false,
            supports_relay_mode: false,
        }),
        voice_audio: None,
        screen_video: None,
        caps_hash: Some(pb::CapabilityHash { sha256: alpn.as_bytes().to_vec() }),
    }
}
