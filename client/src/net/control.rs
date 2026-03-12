use anyhow::{anyhow, Context, Result};
use tokio::time::{timeout, Duration};

use crate::{
    identity::DeviceIdentity,
    net::{
        dispatcher::build_screenshare_caps,
        frame::{read_delimited, write_delimited},
    },
    proto::voiceplatform::v1 as pb,
    screen_share::runtime_probe::probe_media_caps,
    ShareSource, APP_VERSION,
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
        Self {
            session_id: None,
            send,
            recv,
            next_req: 1,
        }
    }

    pub async fn hello_and_auth(
        &mut self,
        alpn: &str,
        device_identity: &DeviceIdentity,
        preferred_display_name: &str,
    ) -> Result<()> {
        let hello = pb::Hello {
            caps: Some(default_caps(alpn)),
            device_id: Some(pb::DeviceId {
                value: device_identity.device_id.clone(),
            }),
        };
        self.send_req(pb::client_to_server::Payload::Hello(hello))
            .await?;
        let resp = self.read_resp().await?;
        let (session_id, challenge) = match resp.payload {
            Some(pb::server_to_client::Payload::HelloAck(ack)) => {
                self.session_id = ack.session_id.clone();
                (
                    ack.session_id
                        .as_ref()
                        .map(|s| s.value.clone())
                        .unwrap_or_default(),
                    ack.auth_challenge,
                )
            }
            _ => return Err(anyhow!("expected HelloAck")),
        };

        let signature = device_identity
            .sign_challenge(&challenge, &session_id)
            .context("sign auth challenge")?;

        let auth = pb::AuthRequest {
            preferred_display_name: preferred_display_name.into(),
            method: Some(pb::auth_request::Method::Device(pb::DeviceAuth {
                device_id: Some(pb::DeviceId {
                    value: device_identity.device_id.clone(),
                }),
                device_pubkey: device_identity.public_key.clone(),
                signature,
            })),
        };
        self.send_req(pb::client_to_server::Payload::AuthRequest(auth))
            .await?;
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
        let req = pb::JoinChannelRequest {
            channel_id: Some(pb::ChannelId {
                value: channel_id.into(),
            }),
        };
        self.send_req(pb::client_to_server::Payload::JoinChannelRequest(req))
            .await?;
        let resp = self.read_resp().await?;
        if let Some(err) = resp.error {
            return Err(anyhow!("join error: {:?}", err));
        }
        Ok(())
    }

    pub async fn ping(&mut self) -> Result<()> {
        let nonce = rand::random::<u64>();
        self.send_req(pb::client_to_server::Payload::Ping(pb::Ping { nonce }))
            .await?;
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
    let runtime_caps = probe_media_caps(&default_probe_share_source());
    let supports_1440p60 = runtime_caps.supports_1440p60;
    let screen_video_codecs = [pb::VideoCodec::Vp9, pb::VideoCodec::Av1]
        .into_iter()
        .filter(|codec| {
            runtime_caps.encode_backends.contains_key(codec)
                && runtime_caps.decode_backends.contains_key(codec)
        })
        .map(|codec| match codec {
            pb::VideoCodec::Vp9 => pb::video_caps::Codec::Vp9 as i32,
            pb::VideoCodec::Av1 => pb::video_caps::Codec::Av1 as i32,
            _ => unreachable!("policy codecs only include VP9/AV1"),
        })
        .collect::<Vec<_>>();
    let supports_screen_share = cfg!(feature = "screen-share") && !screen_video_codecs.is_empty();

    pb::ClientCaps {
        build: Some(pb::BuildInfo {
            client_name: "vp-client".into(),
            client_version: APP_VERSION.into(),
            platform: std::env::consts::OS.into(),
            git_sha: "".into(),
        }),
        features: Some(pb::FeatureCaps {
            supports_quic_datagrams: true,
            supports_voice_fec: true,
            supports_streaming: cfg!(feature = "screen-share") || cfg!(feature = "video-call"),
            supports_drag_drop_upload: true,
            supports_relay_mode: false,
            supports_screen_share,
            supports_video_call: cfg!(feature = "video-call"),
            supports_e2ee: false,
            supports_spatial_audio: false,
            supports_whisper: true,
            supports_noise_suppression: true,
            supports_echo_cancellation: cfg!(feature = "aec"),
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
        screen_video: Some(pb::VideoCaps {
            codecs: screen_video_codecs,
            max_width: if supports_1440p60 { 2560 } else { 1920 },
            max_height: if supports_1440p60 { 1440 } else { 1080 },
            max_fps: 60,
            max_bitrate_bps: 8_000_000,
            hw_encode_available: runtime_caps
                .encode_backends
                .get(&pb::VideoCodec::Av1)
                .map(|backends| !backends.is_empty())
                .unwrap_or(false),
        }),
        caps_hash: Some(pb::CapabilityHash {
            sha256: sha256_bytes(alpn.as_bytes()),
        }),
        screen_share: supports_screen_share.then(|| build_screenshare_caps(&runtime_caps)),
        camera_video: None,
    }
}

fn default_probe_share_source() -> ShareSource {
    #[cfg(target_os = "windows")]
    {
        return ShareSource::WindowsDisplay("0".to_string());
    }
    #[cfg(target_os = "linux")]
    {
        return ShareSource::LinuxPortal(String::new());
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        ShareSource::LinuxPortal(String::new())
    }
}

fn sha256_bytes(data: &[u8]) -> Vec<u8> {
    let d = ring::digest::digest(&ring::digest::SHA256, data);
    d.as_ref().to_vec()
}
