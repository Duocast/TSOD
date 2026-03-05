use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    error::Error as StdError,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, oneshot, watch, Mutex, RwLock},
    time::timeout,
};
use tokio_util::time::{delay_queue::Key as DelayKey, DelayQueue};
use tracing::{debug, info};

use crate::{
    identity::DeviceIdentity,
    net::{
        frame::{read_delimited, write_delimited},
        UiLogTx,
    },
    proto::voiceplatform::v1 as pb,
};

/// Server-push events emitted by the dispatcher.
/// Keep this fairly low-level; app layer can transform into UI state.
#[derive(Clone, Debug)]
pub enum PushEvent {
    Presence {
        event: pb::PresenceEvent,
        event_seq: u64,
    },
    Chat {
        event: pb::ChatEvent,
        event_seq: u64,
    },
    Moderation {
        event: pb::ModerationEvent,
        event_seq: u64,
    },
    ChannelCreated {
        event: pb::ChannelCreatedPush,
        event_seq: u64,
    },
    ChannelRenamed {
        event: pb::ChannelRenamedPush,
        event_seq: u64,
    },
    ChannelDeleted {
        event: pb::ChannelDeletedPush,
        event_seq: u64,
    },
    ServerHint {
        hint: pb::ServerHint,
        event_seq: u64,
    },
    VoiceTelemetry {
        event: pb::VoiceTelemetryPush,
        event_seq: u64,
    },
    Poke {
        event: pb::PokeEvent,
        event_seq: u64,
    },
    Snapshot {
        snapshot: pb::InitialStateSnapshot,
        event_seq: u64,
    },
    Permissions {
        event: pb::PushEvent,
        event_seq: u64,
    },
    SubscribeStream {
        event: pb::SubscribeStream,
        event_seq: u64,
    },
    UnsubscribeStream {
        event: pb::UnsubscribeStream,
        event_seq: u64,
    },
    Unknown(pb::ServerToClient),
}

#[derive(Clone, Debug)]
pub struct AuthInfo {
    pub user_id: String,
    pub session_id: String,
    pub server_id: String,
}

#[derive(Clone, Debug)]
pub struct JoinChannelState {
    pub members: Vec<pb::ChannelMember>,
    pub info: Option<pb::ChannelInfo>,
}

#[derive(Debug)]
enum DispatcherRequestError {
    Timeout { request_id: u64 },
    Disconnected,
    Server { request_id: u64, message: String },
}

impl fmt::Display for DispatcherRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout { request_id } => write!(f, "request {request_id} timed out"),
            Self::Disconnected => write!(f, "dispatcher transport closed"),
            Self::Server {
                request_id,
                message,
            } => {
                write!(f, "control error for request {request_id}: {message}")
            }
        }
    }
}

impl StdError for DispatcherRequestError {}

struct PendingEntry {
    resp_tx: oneshot::Sender<std::result::Result<pb::ServerToClient, DispatcherRequestError>>,
    timeout_key: DelayKey,
}

/// Commands into the dispatcher (outgoing requests).
#[derive(Debug)]
enum Command {
    Send {
        payload: pb::client_to_server::Payload,
        resp_tx: oneshot::Sender<std::result::Result<pb::ServerToClient, DispatcherRequestError>>,
        timeout: Duration,
    },
    SendNoResponse {
        payload: pb::client_to_server::Payload,
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
        ui_log_tx: UiLogTx,
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
            ui_log_tx,
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

    pub async fn hello_auth(
        &self,
        alpn: &str,
        device_identity: &DeviceIdentity,
        preferred_display_name: &str,
    ) -> Result<AuthInfo> {
        let hello = pb::Hello {
            caps: Some(default_caps(alpn)),
            device_id: Some(pb::DeviceId {
                value: device_identity.device_id.clone(),
            }),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::Hello(hello),
                Duration::from_secs(5),
            )
            .await??;

        let (session_id, challenge) = match resp.payload {
            Some(pb::server_to_client::Payload::HelloAck(ack)) => {
                let sid = ack
                    .session_id
                    .as_ref()
                    .map(|s| s.value.clone())
                    .unwrap_or_default();
                if let Some(sid_msg) = ack.session_id {
                    *self.inner.session_id.write().await = Some(sid_msg);
                }
                (sid, ack.auth_challenge)
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

        let resp = self
            .send_request(
                pb::client_to_server::Payload::AuthRequest(auth),
                Duration::from_secs(5),
            )
            .await??;

        if resp.error.is_some() {
            return Err(anyhow!("auth failed: {:?}", resp.error));
        }

        let session_id = self
            .inner
            .session_id
            .read()
            .await
            .as_ref()
            .map(|sid| sid.value.clone())
            .unwrap_or_default();

        match resp.payload {
            Some(pb::server_to_client::Payload::AuthResponse(a)) => {
                let media_caps = default_media_capabilities();
                info!(
                    encode = ?media_caps.encode,
                    decode = ?media_caps.decode,
                    "advertising media codec capabilities"
                );
                let _ = self
                    .send_request(
                        pb::client_to_server::Payload::CapabilitiesUpdate(pb::CapabilitiesUpdate {
                            caps: Some(media_caps),
                        }),
                        Duration::from_secs(2),
                    )
                    .await;
                Ok(AuthInfo {
                    user_id: a.user_id.map(|u| u.value).unwrap_or_default(),
                    session_id,
                    server_id: a.server_id.map(|sid| sid.value).unwrap_or_default(),
                })
            }
            _ => Err(anyhow!("expected AuthResponse")),
        }
    }

    pub async fn join_channel(&self, channel_id: &str) -> Result<JoinChannelState> {
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
        match resp.payload {
            Some(pb::server_to_client::Payload::JoinChannelResponse(jr)) => {
                let state = jr
                    .state
                    .ok_or_else(|| anyhow!("join response missing channel state"))?;
                Ok(JoinChannelState {
                    members: state.members,
                    info: state.info,
                })
            }
            _ => Err(anyhow!("expected JoinChannelResponse")),
        }
    }

    pub async fn get_initial_state_snapshot(&self) -> Result<pb::InitialStateSnapshot> {
        let req = pb::GetInitialStateSnapshotRequest {};
        let resp = self
            .send_request(
                pb::client_to_server::Payload::GetInitialStateSnapshotRequest(req),
                Duration::from_secs(5),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("get_initial_state_snapshot error: {:?}", err));
        }

        match resp.payload {
            Some(pb::server_to_client::Payload::InitialStateSnapshot(snapshot)) => Ok(snapshot),
            _ => Err(anyhow!("expected InitialStateSnapshot")),
        }
    }

    pub async fn ping(&self) -> Result<Duration> {
        let nonce = rand::random::<u64>();
        let started_at = Instant::now();
        let resp = self
            .send_request(
                pb::client_to_server::Payload::Ping(pb::Ping { nonce }),
                Duration::from_secs(2),
            )
            .await??;

        match resp.payload {
            Some(pb::server_to_client::Payload::Pong(p)) if p.nonce == nonce => {
                Ok(started_at.elapsed())
            }
            _ => Err(anyhow!("bad pong")),
        }
    }

    pub async fn leave_channel(&self, channel_id: &str) -> Result<()> {
        let req = pb::LeaveChannelRequest {
            channel_id: Some(pb::ChannelId {
                value: channel_id.into(),
            }),
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

    pub async fn create_channel(
        &self,
        name: &str,
        description: &str,
        channel_type: u8,
        codec: u8,
        bitrate: u32,
        user_limit: u32,
        parent_channel_id: Option<&str>,
    ) -> Result<String> {
        let ch_type = match channel_type {
            1 => pb::ChannelType::Text as i32,
            2 => pb::ChannelType::Streaming as i32,
            _ => pb::ChannelType::Voice as i32,
        };
        let opus_profile = match codec {
            1 => pb::OpusProfile::OpusMusic as i32,
            _ => pb::OpusProfile::OpusVoice as i32,
        };
        let req = pb::CreateChannelRequest {
            name: name.into(),
            description: description.into(),
            channel_type: ch_type,
            bitrate,
            user_limit,
            opus_profile,
            parent_channel_id: parent_channel_id.map(|value| pb::ChannelId {
                value: value.to_string(),
            }),
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
                let ch_id = cr
                    .state
                    .and_then(|s| s.channel_id)
                    .map(|id| id.value)
                    .unwrap_or_default();
                Ok(ch_id)
            }
            _ => Err(anyhow!("expected CreateChannelResponse")),
        }
    }

    pub async fn rename_channel(&self, channel_id: &str, new_name: &str) -> Result<()> {
        let req = pb::RenameChannelRequest {
            channel_id: Some(pb::ChannelId {
                value: channel_id.into(),
            }),
            new_name: new_name.into(),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::RenameChannelRequest(req),
                Duration::from_secs(5),
            )
            .await??;
        if let Some(err) = resp.error {
            return Err(anyhow!("rename_channel error: {:?}", err));
        }
        Ok(())
    }

    pub async fn delete_channel(&self, channel_id: &str) -> Result<()> {
        let req = pb::DeleteChannelRequest {
            channel_id: Some(pb::ChannelId {
                value: channel_id.into(),
            }),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::DeleteChannelRequest(req),
                Duration::from_secs(5),
            )
            .await??;
        if let Some(err) = resp.error {
            return Err(anyhow!("delete_channel error: {:?}", err));
        }
        Ok(())
    }

    pub async fn send_chat(
        &self,
        channel_id: &str,
        text: &str,
        attachments: Vec<pb::AttachmentRef>,
    ) -> Result<()> {
        let req = pb::SendMessageRequest {
            channel_id: Some(pb::ChannelId {
                value: channel_id.into(),
            }),
            text: text.into(),
            attachments,
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

    pub async fn moderate_user(
        &self,
        channel_id: &str,
        target_user_id: &str,
        action: pb::moderation_action_request::Action,
    ) -> Result<()> {
        let req = pb::ModerationActionRequest {
            channel_id: Some(pb::ChannelId {
                value: channel_id.into(),
            }),
            target_user_id: Some(pb::UserId {
                value: target_user_id.into(),
            }),
            action: Some(action),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::ModerationActionRequest(req),
                Duration::from_secs(5),
            )
            .await??;
        if let Some(err) = resp.error {
            return Err(anyhow!("moderate_user error: {:?}", err));
        }
        Ok(())
    }

    pub async fn poke_user(&self, target_user_id: &str, message: &str) -> Result<()> {
        let req = pb::PokeRequest {
            target_user_id: Some(pb::UserId {
                value: target_user_id.into(),
            }),
            message: message.into(),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::PokeRequest(req),
                Duration::from_secs(5),
            )
            .await??;
        if let Some(err) = resp.error {
            return Err(anyhow!("poke_user error: {:?}", err));
        }
        Ok(())
    }
    pub async fn set_away_message(&self, message: &str) -> Result<()> {
        let req = pb::UpdateUserProfileRequest {
            display_name: String::new(),
            description: String::new(),
            status: if message.trim().is_empty() {
                pb::OnlineStatus::Online as i32
            } else {
                pb::OnlineStatus::Idle as i32
            },
            custom_status_text: message.to_string(),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::UpdateUserProfileRequest(req),
                Duration::from_secs(5),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("set_away_message error: {:?}", err));
        }
        Ok(())
    }

    pub async fn set_avatar(&self, avatar_asset_url: &str) -> Result<()> {
        let req = pb::SetAvatarRequest {
            asset_id: Some(pb::AssetId {
                value: avatar_asset_url.to_string(),
            }),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::SetAvatarRequest(req),
                Duration::from_secs(5),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("set_avatar error: {:?}", err));
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
            .map_err(|_| anyhow!("dispatcher dropped response"))?
            .map_err(|err| anyhow!(err)))
    }

    pub async fn shutdown(&self) {
        let _ = self.inner.cmd_tx.send(Command::Shutdown).await;
    }

    pub async fn send_no_response(&self, payload: pb::client_to_server::Payload) -> Result<()> {
        self.inner
            .cmd_tx
            .send(Command::SendNoResponse { payload })
            .await
            .map_err(|_| anyhow!("dispatcher stopped"))
    }
}

/// Dispatcher task: owns send/recv streams.
async fn dispatcher_task(
    inner: Arc<Inner>,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    mut cmd_rx: mpsc::Receiver<Command>,
    mut shutdown_rx: watch::Receiver<bool>,
    ui_log_tx: UiLogTx,
) {
    enum ReaderEvent {
        Response(pb::ServerToClient),
        Error { request_id: u64, message: String },
    }

    let max_ctrl_msg = crate::net::frame::MAX_CONTROL_FRAME_LEN;
    let mut pending: HashMap<u64, PendingEntry> = HashMap::new();
    let mut timeouts: DelayQueue<u64> = DelayQueue::new();
    let mut next_req: u64 = 1;
    let (reader_event_tx, mut reader_event_rx) = mpsc::channel::<ReaderEvent>(1024);

    // Spawn reader task
    let reader_inner = inner.clone();
    let reader_ui_log_tx = ui_log_tx.clone();
    let reader = tokio::spawn(async move {
        loop {
            let env: pb::ControlEnvelope = match read_delimited(&mut recv, max_ctrl_msg).await {
                Ok(m) => m,
                Err(e) => {
                    let _ = reader_ui_log_tx.send(format!("[dispatcher] exiting: control read/decode failed for ControlEnvelope ({e:?})"));
                    return Err::<(), anyhow::Error>(e);
                }
            };

            match env.payload {
                Some(pb::control_envelope::Payload::ControlAck(ack)) => {
                    debug!(request_id = ack.request_id, "received control ack");
                }
                Some(pb::control_envelope::Payload::ControlResponse(msg)) => {
                    if msg.request_id.is_some() {
                        if reader_event_tx
                            .send(ReaderEvent::Response(msg))
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    } else {
                        let ev = classify_push(msg);
                        let _ = reader_inner.push_tx.try_send(ev);
                    }
                }
                Some(pb::control_envelope::Payload::ControlError(err)) => {
                    if reader_event_tx
                        .send(ReaderEvent::Error {
                            request_id: err.request_id,
                            message: err.message,
                        })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                _ => {
                    return Err(anyhow!("unexpected control envelope payload from server"));
                }
            }
        }
    });

    // Writer/command loop
    tokio::pin!(reader);
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let _ = ui_log_tx.send("[dispatcher] exiting: shutdown signal received".to_string());
                    break;
                }
            }
            expired = timeouts.next() => {
                if let Some(expired) = expired {
                    let rid = expired.into_inner();
                    if let Some(entry) = pending.remove(&rid) {
                        let _ = entry.resp_tx.send(Err(DispatcherRequestError::Timeout { request_id: rid }));
                    }
                }
            }
            evt = reader_event_rx.recv() => {
                match evt {
                    Some(ReaderEvent::Response(msg)) => {
                        let Some(rid) = msg.request_id.as_ref().map(|x| x.value) else {
                            continue;
                        };

                        if let Some(entry) = pending.remove(&rid) {
                            let _ = timeouts.remove(&entry.timeout_key);
                            let _ = entry.resp_tx.send(Ok(msg));
                        } else {
                            debug!(request_id = rid, "ignoring late/unmatched response id");
                        }
                    }
                    Some(ReaderEvent::Error { request_id, message }) => {
                        if let Some(entry) = pending.remove(&request_id) {
                            let _ = timeouts.remove(&entry.timeout_key);
                            let _ = entry
                                .resp_tx
                                .send(Err(DispatcherRequestError::Server { request_id, message }));
                        } else {
                            debug!(request_id, "ignoring late/unmatched control error id");
                        }
                    }
                    None => break,
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    None => {
                        let _ = ui_log_tx.send("[dispatcher] exiting: command channel closed".to_string());
                        break;
                    }
                    Some(Command::Shutdown) => {
                        let _ = ui_log_tx.send("[dispatcher] exiting: shutdown command received".to_string());
                        break;
                    }
                    Some(Command::Send { payload, resp_tx, timeout }) => {
                        let rid = next_req;
                        next_req += 1;

                        let timeout_key = timeouts.insert(rid, timeout);
                        pending.insert(rid, PendingEntry { resp_tx, timeout_key });

                        let session_id = inner.session_id.read().await.clone();
                        let msg = pb::ClientToServer {
                            request_id: Some(pb::RequestId { value: rid }),
                            session_id,
                            sent_at: Some(now_ts()),
                            payload: Some(payload),
                        };
                        let env = pb::ControlEnvelope {
                            request_id: rid,
                            payload: Some(pb::control_envelope::Payload::ControlRequest(msg)),
                        };

                        if let Err(e) = write_delimited(&mut send, &env).await {
                            let _ = ui_log_tx.send(format!("[dispatcher] exiting: control send failed ({e:?})"));
                            fail_all_pending(&mut pending).await;
                            break;
                        }
                    }
                    Some(Command::SendNoResponse { payload }) => {
                        let session_id = inner.session_id.read().await.clone();
                        let msg = pb::ClientToServer {
                            request_id: None,
                            session_id,
                            sent_at: Some(now_ts()),
                            payload: Some(payload),
                        };
                        let env = pb::ControlEnvelope {
                            request_id: 0,
                            payload: Some(pb::control_envelope::Payload::ControlRequest(msg)),
                        };

                        if let Err(e) = write_delimited(&mut send, &env).await {
                            let _ = ui_log_tx.send(format!("[dispatcher] exiting: control send failed ({e:?})"));
                            fail_all_pending(&mut pending).await;
                            break;
                        }
                    }
                }
            }
            r = &mut reader => {
                match r {
                    Ok(Ok(())) => {
                        let _ = ui_log_tx.send("[dispatcher] exiting: control reader stopped cleanly".to_string());
                    }
                    Ok(Err(e)) => {
                        let _ = ui_log_tx.send(format!("[dispatcher] exiting: control reader stream error ({e:?})"));
                    }
                    Err(e) => {
                        let _ = ui_log_tx.send(format!("[dispatcher] exiting: control reader join error ({e:?})"));
                    }
                }
                fail_all_pending(&mut pending).await;
                break;
            }
        }
    }

    fail_all_pending(&mut pending).await;
}

async fn fail_all_pending(pending: &mut HashMap<u64, PendingEntry>) {
    for (_, entry) in pending.drain() {
        let _ = entry
            .resp_tx
            .send(Err(DispatcherRequestError::Disconnected));
    }
}

fn classify_push(msg: pb::ServerToClient) -> PushEvent {
    match msg.payload {
        Some(pb::server_to_client::Payload::PresenceEvent(e)) => PushEvent::Presence {
            event: e,
            event_seq: msg.event_seq,
        },
        Some(pb::server_to_client::Payload::ChatEvent(e)) => PushEvent::Chat {
            event: e,
            event_seq: msg.event_seq,
        },
        Some(pb::server_to_client::Payload::ModerationEvent(e)) => PushEvent::Moderation {
            event: e,
            event_seq: msg.event_seq,
        },
        Some(pb::server_to_client::Payload::ChannelCreatedPush(ev)) => PushEvent::ChannelCreated {
            event: ev,
            event_seq: msg.event_seq,
        },
        Some(pb::server_to_client::Payload::ChannelRenamedPush(ev)) => PushEvent::ChannelRenamed {
            event: ev,
            event_seq: msg.event_seq,
        },
        Some(pb::server_to_client::Payload::ChannelDeletedPush(ev)) => PushEvent::ChannelDeleted {
            event: ev,
            event_seq: msg.event_seq,
        },
        Some(pb::server_to_client::Payload::ServerHint(h)) => PushEvent::ServerHint {
            hint: h,
            event_seq: msg.event_seq,
        },
        Some(pb::server_to_client::Payload::VoiceTelemetryPush(event)) => {
            PushEvent::VoiceTelemetry {
                event,
                event_seq: msg.event_seq,
            }
        }
        Some(pb::server_to_client::Payload::PokeEvent(e)) => PushEvent::Poke {
            event: e,
            event_seq: msg.event_seq,
        },
        Some(pb::server_to_client::Payload::InitialStateSnapshot(snapshot)) => {
            PushEvent::Snapshot {
                snapshot,
                event_seq: msg.event_seq,
            }
        }
        Some(pb::server_to_client::Payload::PermissionsPushEvent(event)) => {
            PushEvent::Permissions {
                event,
                event_seq: msg.event_seq,
            }
        }
        Some(pb::server_to_client::Payload::SubscribeStream(event)) => PushEvent::SubscribeStream {
            event,
            event_seq: msg.event_seq,
        },
        Some(pb::server_to_client::Payload::UnsubscribeStream(event)) => {
            PushEvent::UnsubscribeStream {
                event,
                event_seq: msg.event_seq,
            }
        }
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

fn default_media_capabilities() -> pb::ClientMediaCapabilities {
    let mut codecs = Vec::with_capacity(3);
    if cfg!(feature = "video-av1") {
        codecs.push(pb::VideoCodec::Av1 as i32);
    }
    if cfg!(feature = "video-vp9") {
        codecs.push(pb::VideoCodec::Vp9 as i32);
    }
    codecs.push(pb::VideoCodec::Vp8 as i32);

    pb::ClientMediaCapabilities {
        decode: codecs.clone(),
        encode: codecs,
        hw_encode_av1: false,
        hw_encode_vp9: false,
        hw_encode_vp8: false,
        hw_decode_av1: false,
        hw_decode_vp9: false,
        hw_decode_vp8: false,
    }
}

fn default_caps(alpn: &str) -> pb::ClientCaps {
    let media_caps = default_media_capabilities();
    let screen_video_codecs: Vec<i32> = media_caps
        .encode
        .iter()
        .filter_map(|codec| match pb::VideoCodec::try_from(*codec).ok() {
            Some(pb::VideoCodec::Av1) => Some(pb::video_caps::Codec::Av1 as i32),
            Some(pb::VideoCodec::Vp9) => Some(pb::video_caps::Codec::Vp9 as i32),
            _ => None,
        })
        .collect();
    let preferred_screenshare_codec = screen_video_codecs
        .first()
        .copied()
        .unwrap_or(pb::video_caps::Codec::Vp9 as i32);

    pb::ClientCaps {
        build: Some(pb::BuildInfo {
            client_name: "vp-client".into(),
            client_version: "0.1.0".into(),
            platform: std::env::consts::OS.into(),
            git_sha: "".into(),
        }),
        features: Some(pb::FeatureCaps {
            supports_quic_datagrams: true,
            supports_voice_fec: true,
            supports_streaming: cfg!(feature = "screen-share") || cfg!(feature = "video-call"),
            supports_drag_drop_upload: true,
            supports_relay_mode: false,
            supports_screen_share: cfg!(feature = "screen-share"),
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
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
            max_bitrate_bps: 8_000_000,
            hw_encode_available: false,
        }),
        caps_hash: Some(pb::CapabilityHash {
            sha256: alpn.as_bytes().to_vec(),
        }),
        screen_share: Some(pb::ScreenShareCaps {
            codec: preferred_screenshare_codec,
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
            max_bitrate_bps: 8_000_000,
            max_simulcast_layers: 1,
            supports_system_audio: false,
        }),
        camera_video: None,
    }
}
