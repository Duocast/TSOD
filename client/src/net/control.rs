use anyhow::{anyhow, Result};
use tokio::time::{timeout, Duration};

use crate::{
    net::frame::{read_delimited, write_delimited},
    proto::voiceplatform::v1 as pb,
};

const MAX_CTRL_MSG: usize = 256 * 1024;

pub struct ControlClient {
    pub session_id: Option<pb::SessionId>,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    next_req: u64,
}

impl ControlClient {
    pub fn new(send: quinn::SendStream, recv: quinn::RecvStream) -> Self {
        Self { session_id: None, send, recv, next_req: 1 }
    }

    pub async fn hello_and_auth(&mut self, alpn: &str, dev_token: &str) -> Result<()> {
        let hello = pb::Hello {
            caps: Some(default_caps(alpn)),
            device_id: Some(pb::DeviceId { value: "dev-device".into() }),
        };
        self.send_req(pb::client_to_server::Payload::Hello(hello)).await?;
        let resp = self.read_resp().await?;
        match resp.payload {
            Some(pb::server_to_client::Payload::HelloAck(ack)) => {
                self.session_id = ack.session_id;
            }
            _ => return Err(anyhow!("expected HelloAck")),
        }

        let auth = pb::AuthRequest {
            method: Some(pb::auth_request::Method::DevToken(pb::DevTokenAuth { token: dev_token.into() })),
        };
        self.send_req(pb::client_to_server::Payload::AuthRequest(auth)).await?;
        let resp = self.read_resp().await?;
        if resp.error.is_some() {
            return Err(anyhow!("auth failed: {:?}", resp.error));
        }
        match resp.payload {
            Some(pb::server_to_client::Payload::AuthResponse(_)) => Ok(()),
            _ => Err(anyhow!("expected AuthResponse")),
        }
    }

    pub async fn join_channel(&mut self, channel_id: &str) -> Result<()> {
        let req = pb::JoinChannelRequest { channel_id: Some(pb::ChannelId { value: channel_id.into() }) };
        self.send_req(pb::client_to_server::Payload::JoinChannelRequest(req)).await?;
        let resp = self.read_resp().await?;
        if let Some(err) = resp.error {
            return Err(anyhow!("join error: {:?}", err));
        }
        Ok(())
    }

    pub async fn ping(&mut self) -> Result<()> {
        let nonce = rand::random::<u64>();
        self.send_req(pb::client_to_server::Payload::Ping(pb::Ping { nonce })).await?;
        let resp = timeout(Duration::from_secs(2), self.read_resp()).await??;
        match resp.payload {
            Some(pb::server_to_client::Payload::Pong(p)) if p.nonce == nonce => Ok(()),
            _ => Err(anyhow!("bad pong")),
        }
    }

    async fn send_req(&mut self, payload: pb::client_to_server::Payload) -> Result<()> {
        let req_id = self.next_req;
        self.next_req += 1;
        let msg = pb::ClientToServer {
            request_id: Some(pb::RequestId { value: req_id }),
            session_id: self.session_id.clone(),
            sent_at: Some(now_ts()),
            payload: Some(payload),
        };
        write_delimited(&mut self.send, &msg).await
    }

    async fn read_resp(&mut self) -> Result<pb::ServerToClient> {
        read_delimited(&mut self.recv, MAX_CTRL_MSG).await
    }
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
            client_name: "vp-client".into(),
            client_version: "0.1.0".into(),
            platform: std::env::consts::OS.into(),
            git_sha: "".into(),
        }),
        features: Some(pb::FeatureCaps {
            supports_quic_datagrams: true,
            supports_voice_fec: false,
            supports_streaming: false,
            supports_drag_drop_upload: true,
            supports_relay_mode: false,
            supports_screen_share: false,
            supports_video_call: false,
            supports_e2ee: false,
            supports_spatial_audio: false,
            supports_whisper: true,
            supports_noise_suppression: true,
            supports_echo_cancellation: false,
            supports_agc: true,
        }),
        voice_audio: Some(pb::AudioCaps {
            codec: pb::audio_caps::Codec::Opus as i32,
            sample_rate_hz: 48_000,
            stereo: false,
            frame_ms_preference: vec![20, 10],
            max_bitrate_bps: 64_000,
            max_simultaneous_decodes: 8,
        }),
        screen_video: None,
        caps_hash: Some(pb::CapabilityHash { sha256: sha256_bytes(alpn.as_bytes()) }),
        screen_share: None,
        camera_video: None,
    }
}

fn sha256_bytes(data: &[u8]) -> Vec<u8> {
    let d = ring::digest::digest(&ring::digest::SHA256, data);
    d.as_ref().to_vec()
}
