use anyhow::{anyhow, Result};
use std::{
    collections::HashMap,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, Mutex, RwLock, watch},
    time::timeout,
};
use tracing::warn;

use crate::{
    net::frame::{read_delimited, write_delimited},
    proto::voiceplatform::v1 as pb,
};

const MAX_CTRL_MSG: usize = 256 * 1024;

/// Server-push events emitted by the dispatcher.
/// Keep this fairly low-level; app layer can transform into UI state.
#[derive(Clone, Debug)]
pub enum PushEvent {
    Presence(pb::PresenceEvent),
    Chat(pb::ChatEvent),
    Moderation(pb::ModerationEvent),
    ServerHint(pb::ServerHint),
    Unknown(pb::ServerToClient),
}

/// Commands into the dispatcher (outgoing requests).
#[derive(Debug)]
enum Command {
    Send {
        payload: pb::client_to_server::Payload,
        resp_tx: oneshot::Sender<Result<pb::ServerToClient>>,
        #[allow(dead_code)]
        timeout: Duration,
    },
    Shutdown,
}

/// Public handle: cloneable, threadsafe.
#[derive(Clone)]
pub struct ControlDispatcher {
    inner: Arc<Inner>,
}

struct Inner {
    cmd_tx: mpsc::Sender<Command>,
    #[allow(dead_code)]
    push_tx: mpsc::Sender<PushEvent>,
    push_rx: Mutex<Option<mpsc::Receiver<PushEvent>>>,
    session_id: RwLock<Option<pb::SessionId>>,
}

impl ControlDispatcher {
    /// Start the dispatcher. Takes ownership of the control stream.
    /// - `shutdown_rx`: when true, dispatcher exits.
    pub fn start(
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(256);
        let (push_tx, push_rx) = mpsc::channel::<PushEvent>(1024);

        let inner = Arc::new(Inner {
            cmd_tx,
            push_tx,
            push_rx: Mutex::new(Some(push_rx)),
            session_id: RwLock::new(None),
        });

        tokio::spawn(dispatcher_task(
            inner.clone(),
            send,
            recv,
            cmd_rx,
            shutdown_rx,
        ));

        Self { inner }
    }

    /// Take the push event receiver (single consumer).
    pub async fn take_push_receiver(&self) -> mpsc::Receiver<PushEvent> {
        self.inner
            .push_rx
            .lock()
            .await
            .take()
            .expect("push receiver already taken")
    }

    pub async fn hello_auth(&self, alpn: &str, dev_token: &str) -> Result<()> {
        let hello = pb::Hello {
            caps: Some(default_caps(alpn)),
            device_id: Some(pb::DeviceId {
                value: "dev-device".into(),
            }),
        };
        let resp = self
            .send_request(pb::client_to_server::Payload::Hello(hello), Duration::from_secs(5))
            .await??;

        match resp.payload {
            Some(pb::server_to_client::Payload::HelloAck(ack)) => {
                if let Some(sid) = ack.session_id {
                    *self.inner.session_id.write().await = Some(sid);
                }
            }
            _ => return Err(anyhow!("expected HelloAck")),
        }

        let auth = pb::AuthRequest {
            method: Some(pb::auth_request::Method::DevToken(pb::DevTokenAuth {
                token: dev_token.into(),
            })),
        };

        let resp = self
            .send_request(
                pb::client_to_server::Payload::AuthRequest(auth),
                Duration::from_secs(5),
            )
            .await??;

        if resp.error.is_some() {
            return Err(anyhow!("auth failed: {:?}", resp.error));
        }

        match resp.payload {
            Some(pb::server_to_client::Payload::AuthResponse(_)) => Ok(()),
            _ => Err(anyhow!("expected AuthResponse")),
        }
    }

    pub async fn join_channel(&self, channel_id: &str) -> Result<()> {
        let req = pb::JoinChannelRequest {
            channel_id: Some(pb::ChannelId {
                value: channel_id.into(),
            }),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::JoinChannelRequest(req),
                Duration::from_secs(5),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("join error: {:?}", err));
        }
        Ok(())
    }

    pub async fn ping(&self) -> Result<()> {
        let nonce = rand::random::<u64>();
        let resp = self
            .send_request(
                pb::client_to_server::Payload::Ping(pb::Ping { nonce }),
                Duration::from_secs(2),
            )
            .await??;

        match resp.payload {
            Some(pb::server_to_client::Payload::Pong(p)) if p.nonce == nonce => Ok(()),
            _ => Err(anyhow!("bad pong")),
        }
    }

    pub async fn leave_channel(&self, channel_id: &str) -> Result<()> {
        let req = pb::LeaveChannelRequest {
            channel_id: Some(pb::ChannelId { value: channel_id.into() }),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::LeaveChannelRequest(req),
                Duration::from_secs(5),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("leave_channel error: {:?}", err));
        }
        Ok(())
    }

    pub async fn create_channel(&self, name: &str) -> Result<String> {
        let req = pb::CreateChannelRequest {
            name: name.into(),
            ..Default::default()
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::CreateChannelRequest(req),
                Duration::from_secs(5),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("create_channel error: {:?}", err));
        }
        match resp.payload {
            Some(pb::server_to_client::Payload::CreateChannelResponse(cr)) => {
                let ch_id = cr.state
                    .and_then(|s| s.channel_id)
                    .map(|id| id.value)
                    .unwrap_or_default();
                Ok(ch_id)
            }
            _ => Err(anyhow!("expected CreateChannelResponse")),
        }
    }

    pub async fn send_chat(&self, channel_id: &str, text: &str) -> Result<()> {
        let req = pb::SendMessageRequest {
            channel_id: Some(pb::ChannelId { value: channel_id.into() }),
            text: text.into(),
            attachments: vec![],
            reply_to_message_id: None,
            mentions: vec![],
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::SendMessageRequest(req),
                Duration::from_secs(5),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("send_chat error: {:?}", err));
        }
        Ok(())
    }

    /// Low-level request API with correlation.
    pub async fn send_request(
        &self,
        payload: pb::client_to_server::Payload,
        timeout_dur: Duration,
    ) -> Result<Result<pb::ServerToClient>> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.inner
            .cmd_tx
            .send(Command::Send {
                payload,
                resp_tx,
                timeout: timeout_dur,
            })
            .await
            .map_err(|_| anyhow!("dispatcher stopped"))?;

        Ok(timeout(timeout_dur + Duration::from_millis(250), resp_rx)
            .await
            .map_err(|_| anyhow!("request timed out waiting for response"))?
            .map_err(|_| anyhow!("dispatcher dropped response"))?)
    }

    pub async fn shutdown(&self) {
        let _ = self.inner.cmd_tx.send(Command::Shutdown).await;
    }
}

/// Dispatcher task: owns send/recv streams.
async fn dispatcher_task(
    inner: Arc<Inner>,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    mut cmd_rx: mpsc::Receiver<Command>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<pb::ServerToClient>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next_req: Arc<Mutex<u64>> = Arc::new(Mutex::new(1));

    // Spawn reader task
    let reader_pending = pending.clone();
    let reader_inner = inner.clone();
    let reader = tokio::spawn(async move {
        loop {
            let msg: pb::ServerToClient = match read_delimited(&mut recv, MAX_CTRL_MSG).await {
                Ok(m) => m,
                Err(e) => return Err::<(), anyhow::Error>(e),
            };

            if let Some(rid) = msg.request_id.as_ref().map(|x| x.value) {
                if let Some(tx) = reader_pending.lock().await.remove(&rid) {
                    let _ = tx.send(Ok(msg));
                    continue;
                }
            }

            let ev = classify_push(msg);
            if reader_inner.push_tx.try_send(ev).is_err() {
                // drop if full
            }
        }
    });

    // Writer/command loop
    tokio::pin!(reader);
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    None => break,
                    Some(Command::Shutdown) => break,
                    Some(Command::Send { payload, resp_tx, timeout: _ }) => {
                        let rid = {
                            let mut g = next_req.lock().await;
                            let v = *g;
                            *g += 1;
                            v
                        };

                        pending.lock().await.insert(rid, resp_tx);

                        let session_id = inner.session_id.read().await.clone();
                        let msg = pb::ClientToServer {
                            request_id: Some(pb::RequestId { value: rid }),
                            session_id,
                            sent_at: Some(now_ts()),
                            payload: Some(payload),
                        };

                        if let Err(e) = write_delimited(&mut send, &msg).await {
                            warn!("control send failed: {e:#}");
                            fail_all_pending(&pending).await;
                            break;
                        }
                    }
                }
            }
            r = &mut reader => {
                if let Err(e) = r {
                    warn!("control reader join error: {}", e);
                }
                fail_all_pending(&pending).await;
                break;
            }
        }
    }

    fail_all_pending(&pending).await;
}

async fn fail_all_pending(
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<pb::ServerToClient>>>>>,
) {
    let mut map = pending.lock().await;
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(anyhow!("dispatcher shutdown")));
    }
}

fn classify_push(msg: pb::ServerToClient) -> PushEvent {
    match msg.payload {
        Some(pb::server_to_client::Payload::PresenceEvent(e)) => PushEvent::Presence(e),
        Some(pb::server_to_client::Payload::ChatEvent(e)) => PushEvent::Chat(e),
        Some(pb::server_to_client::Payload::ModerationEvent(e)) => PushEvent::Moderation(e),
        Some(pb::server_to_client::Payload::ServerHint(h)) => PushEvent::ServerHint(h),
        _ => PushEvent::Unknown(msg),
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
        caps_hash: Some(pb::CapabilityHash {
            sha256: alpn.as_bytes().to_vec(),
        }),
        screen_share: None,
        camera_video: None,
    }
}
