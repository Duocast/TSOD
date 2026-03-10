use anyhow::{anyhow, Context, Result};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, oneshot, watch, Mutex, RwLock},
    time::timeout,
};
use tracing::info;

use crate::{
    identity::DeviceIdentity,
    net::{
        frame::{read_delimited, write_delimited},
        UiLogTx,
    },
    proto::voiceplatform::v1 as pb,
};

const MAX_CTRL_MSG: usize = 256 * 1024;

/// Stream-type discriminator bytes written as the first byte on each bidi stream.
pub const STREAM_TYPE_MEDIA: u8 = 0x01;
pub const STREAM_TYPE_PROFILE_ASSET: u8 = 0x02;
 
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
    UserProfile {
        event: pb::UserProfileEvent,
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

/// Commands into the dispatcher (outgoing requests).
#[derive(Debug)]
enum Command {
    Send {
        payload: pb::client_to_server::Payload,
        resp_tx: oneshot::Sender<Result<pb::ServerToClient>>,
        #[allow(dead_code)]
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
                Duration::from_secs(1),
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
                Duration::from_secs(1),
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
                Duration::from_secs(1),
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
                Duration::from_secs(1),
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
                Duration::from_secs(1),
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
                Duration::from_secs(1),
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

    pub async fn rename_channel(
        &self,
        channel_id: &str,
        new_name: &str,
        codec: u8,
        bitrate_bps: u32,
    ) -> Result<()> {
        let opus_profile = match codec {
            1 => pb::OpusProfile::OpusMusic as i32,
            _ => pb::OpusProfile::OpusVoice as i32,
        };
        let req = pb::UpdateChannelRequest {
            channel_id: Some(pb::ChannelId {
                value: channel_id.into(),
            }),
            name: new_name.into(),
            bitrate: bitrate_bps,
            opus_profile,
            ..Default::default()
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::UpdateChannelRequest(req),
                Duration::from_secs(1),
            )
            .await??;
        if let Some(err) = resp.error {
            return Err(anyhow!("update_channel error: {:?}", err));
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
                Duration::from_secs(1),
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
                Duration::from_secs(1),
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
                Duration::from_secs(1),
            )
            .await??;
        if let Some(err) = resp.error {
            return Err(anyhow!("moderate_user error: {:?}", err));
        }
        Ok(())
    }

    pub async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<()> {
        let req = pb::AddReactionRequest {
            message_id: Some(pb::MessageId {
                value: message_id.into(),
            }),
            channel_id: Some(pb::ChannelId {
                value: channel_id.into(),
            }),
            emoji: emoji.into(),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::AddReactionRequest(req),
                Duration::from_secs(1),
            )
            .await??;
        if let Some(err) = resp.error {
            return Err(anyhow!("add_reaction error: {:?}", err));
        }
        Ok(())
    }

    pub async fn remove_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<()> {
        let req = pb::RemoveReactionRequest {
            message_id: Some(pb::MessageId {
                value: message_id.into(),
            }),
            channel_id: Some(pb::ChannelId {
                value: channel_id.into(),
            }),
            emoji: emoji.into(),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::RemoveReactionRequest(req),
                Duration::from_secs(1),
            )
            .await??;
        if let Some(err) = resp.error {
            return Err(anyhow!("remove_reaction error: {:?}", err));
        }
        Ok(())
    }

    pub async fn send_typing(&self, channel_id: &str) -> Result<()> {
        let req = pb::SendTypingRequest {
            channel_id: Some(pb::ChannelId {
                value: channel_id.into(),
            }),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::SendTypingRequest(req),
                Duration::from_secs(1),
            )
            .await??;
        if let Some(err) = resp.error {
            return Err(anyhow!("send_typing error: {:?}", err));
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
                Duration::from_secs(2),
            )
            .await??;
        if let Some(err) = resp.error {
            return Err(anyhow!("poke_user error: {:?}", err));
        }
        Ok(())
    }
    pub async fn set_away_message(&self, message: &str) -> Result<()> {
        let req = pb::UpdateUserProfileRequest {
            display_name: None,
            description: None,
            status: if message.trim().is_empty() {
                pb::OnlineStatus::Online as i32
            } else {
                pb::OnlineStatus::Idle as i32
            },
            custom_status_text: Some(message.to_string()),
            custom_status_emoji: None,
            custom_status_expires: None,
            accent_color: None,
            links: Vec::new(),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::UpdateUserProfileRequest(req),
                Duration::from_secs(1),
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
                Duration::from_secs(1),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("set_avatar error: {:?}", err));
        }
        Ok(())
    }

    /// Update the user's profile text fields, accent color, and links.
    pub async fn update_user_profile(
        &self,
        display_name: Option<String>,
        description: Option<String>,
        accent_color: Option<u32>,
        links: Vec<crate::ui::model::ProfileLinkData>,
    ) -> Result<()> {
        let pb_links = links
            .into_iter()
            .map(|l| pb::ProfileLink {
                platform: l.platform,
                url: l.url,
                display_text: l.display_text,
                verified: l.verified,
            })
            .collect();

        let req = pb::UpdateUserProfileRequest {
            display_name,
            description,
            status: pb::OnlineStatus::StatusUnspecified as i32,
            custom_status_text: None,
            custom_status_emoji: None,
            custom_status_expires: None,
            accent_color,
            links: pb_links,
        };

        let resp = self
            .send_request(
                pb::client_to_server::Payload::UpdateUserProfileRequest(req),
                Duration::from_secs(5),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("update_user_profile error: {:?}", err));
        }
        Ok(())
    }

    /// Set the user's custom status (emoji + text), clearing if both are empty.
    pub async fn set_custom_status(
        &self,
        status_text: Option<String>,
        status_emoji: Option<String>,
    ) -> Result<()> {
        let req = pb::SetCustomStatusRequest {
            status_text,
            status_emoji,
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::SetCustomStatusRequest(req),
                Duration::from_secs(3),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("set_custom_status error: {:?}", err));
        }
        Ok(())
    }

    /// Set the user's banner by verified asset_id.
    pub async fn set_banner(&self, asset_id: &str) -> Result<()> {
        let req = pb::SetBannerRequest {
            asset_id: Some(pb::AssetId {
                value: asset_id.to_string(),
            }),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::SetBannerRequest(req),
                Duration::from_secs(3),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("set_banner error: {:?}", err));
        }
        Ok(())
    }

    /// Upload a profile asset (avatar or banner) to the server over a new authenticated
    /// Quinn bidi stream.  Returns the verified `asset_id` on success.
    pub async fn upload_profile_asset(
        &self,
        conn: &quinn::Connection,
        purpose: &str,
        image_bytes: Vec<u8>,
        mime_type: &str,
    ) -> Result<String> {

        // Step 1: request upload session approval on the control stream.
        let begin_req = pb::BeginProfileAssetUploadRequest {
            purpose: purpose.to_string(),
            mime_type: mime_type.to_string(),
            byte_length: image_bytes.len() as u64,
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::BeginProfileAssetUploadRequest(begin_req),
                Duration::from_secs(5),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("begin_profile_asset_upload error: {:?}", err));
        }

        let session_id = match resp.payload {
            Some(pb::server_to_client::Payload::BeginProfileAssetUploadResponse(r)) => r.session_id,
            _ => return Err(anyhow!("unexpected response to BeginProfileAssetUploadRequest")),
        };

        // Step 2: open a dedicated bidi stream for the asset data.
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .context("open profile asset upload stream")?;
 
        // Write stream-type discriminator so the server routes correctly.
        send.write_all(&[STREAM_TYPE_PROFILE_ASSET])
            .await
            .context("write stream type")?;
 
        let data_req = pb::UploadProfileAssetDataRequest {
            session_id,
            data: image_bytes,
        };
        write_delimited(&mut send, &data_req).await?;
        send.finish().context("finish profile asset upload stream")?;

        // Read back the CompleteProfileAssetUploadResponse.
        let complete_resp: pb::CompleteProfileAssetUploadResponse =
            read_delimited(&mut recv, 64 * 1024).await?;

        let asset_id = complete_resp
            .asset_id
            .map(|a| a.value)
            .ok_or_else(|| anyhow!("server returned no asset_id"))?;

        Ok(asset_id)
    }

    /// Fetch the calling user's own profile.
    pub async fn fetch_self_profile(&self, user_id: &str) -> Result<crate::ui::model::UserProfileData> {
        let req = pb::GetUserProfileRequest {
            user_id: Some(pb::UserId {
                value: user_id.to_string(),
            }),
        };
        let resp = self
            .send_request(
                pb::client_to_server::Payload::GetUserProfileRequest(req),
                Duration::from_secs(5),
            )
            .await??;

        if let Some(err) = resp.error {
            return Err(anyhow!("fetch_self_profile error: {:?}", err));
        }

        let profile = match resp.payload {
            Some(pb::server_to_client::Payload::GetUserProfileResponse(r)) => {
                r.profile.ok_or_else(|| anyhow!("empty profile in response"))?
            }
            _ => return Err(anyhow!("unexpected response to GetUserProfileRequest")),
        };

        Ok(pb_profile_to_ui(&profile))
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
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<pb::ServerToClient>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next_req: Arc<Mutex<u64>> = Arc::new(Mutex::new(1));

    // Spawn reader task
    let reader_pending = pending.clone();
    let reader_inner = inner.clone();
    let reader_ui_log_tx = ui_log_tx.clone();
    let reader = tokio::spawn(async move {
        loop {
            let msg: pb::ServerToClient = match read_delimited(&mut recv, MAX_CTRL_MSG).await {
                Ok(m) => m,
                Err(e) => {
                    let _ = reader_ui_log_tx.send(format!("[dispatcher] exiting: control read/decode failed for ServerToClient ({e:?})"));
                    return Err::<(), anyhow::Error>(e);
                }
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
                if *shutdown_rx.borrow() {
                    let _ = ui_log_tx.send("[dispatcher] exiting: shutdown signal received".to_string());
                    break;
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
                            let _ = ui_log_tx.send(format!("[dispatcher] exiting: control send failed ({e:?})"));
                            fail_all_pending(&pending).await;
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

                        if let Err(e) = write_delimited(&mut send, &msg).await {
                            let _ = ui_log_tx.send(format!("[dispatcher] exiting: control send failed ({e:?})"));
                            fail_all_pending(&pending).await;
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

/// Convert a protobuf UserProfile to the UI model type.
pub fn pb_profile_to_ui(p: &pb::UserProfile) -> crate::ui::model::UserProfileData {
    use crate::ui::model::*;

    let status = match pb::OnlineStatus::try_from(p.status).unwrap_or_default() {
        pb::OnlineStatus::Online => OnlineStatus::Online,
        pb::OnlineStatus::Idle => OnlineStatus::Idle,
        pb::OnlineStatus::DoNotDisturb => OnlineStatus::DoNotDisturb,
        pb::OnlineStatus::Invisible => OnlineStatus::Invisible,
        pb::OnlineStatus::Offline => OnlineStatus::Offline,
        pb::OnlineStatus::StatusUnspecified => OnlineStatus::Online,
    };

    let avatar_url = if p.avatar_asset_url.is_empty() {
        None
    } else {
        Some(p.avatar_asset_url.clone())
    };
    let banner_url = if p.banner_asset_url.is_empty() {
        None
    } else {
        Some(p.banner_asset_url.clone())
    };

    let badges = p
        .badges
        .iter()
        .map(|b| BadgeData {
            id: b.id.clone(),
            label: b.label.clone(),
            icon_url: b.icon_url.clone(),
            tooltip: b.tooltip.clone(),
        })
        .collect();

    let links = p
        .links
        .iter()
        .map(|l| ProfileLinkData {
            platform: l.platform.clone(),
            url: l.url.clone(),
            display_text: l.display_text.clone(),
            verified: l.verified,
        })
        .collect();

    let current_activity = p.current_activity.as_ref().map(|a| GameActivityData {
        game_name: a.game_name.clone(),
        details: a.details.clone(),
        state: a.state.clone(),
        started_at: a.started_at.as_ref().map(|t| t.unix_millis).unwrap_or(0),
        large_image_url: a.large_image_url.clone(),
    });

    UserProfileData {
        user_id: p.user_id.as_ref().map(|u| u.value.clone()).unwrap_or_default(),
        display_name: p.display_name.clone(),
        description: p.description.clone(),
        status,
        custom_status_text: p.custom_status_text.clone(),
        custom_status_emoji: p.custom_status_emoji.clone(),
        accent_color: p.accent_color,
        avatar_url,
        banner_url,
        badges,
        links,
        created_at: 0,
        last_seen_at: 0,
        current_activity,
        roles: Vec::new(),
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
        Some(pb::server_to_client::Payload::UserProfileEvent(event)) => {
            PushEvent::UserProfile {
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

pub fn available_screen_share_codecs() -> Vec<&'static str> {
    available_video_codecs()
        .into_iter()
        .filter_map(|codec| match codec {
            pb::VideoCodec::Av1 => Some("AV1"),
            pb::VideoCodec::Vp9 => Some("VP9"),
            _ => None,
        })
        .collect()
}

fn available_video_codecs() -> Vec<pb::VideoCodec> {
    let mut codecs = Vec::with_capacity(2);
    if cfg!(feature = "video-av1") {
        codecs.push(pb::VideoCodec::Av1);
    }
    if cfg!(feature = "video-vp9") {
        codecs.push(pb::VideoCodec::Vp9);
    }
    let decodable = crate::net::video_decode::available_decodable_codecs();
    codecs.retain(|codec| decodable.contains(codec));
    codecs
}

fn default_media_capabilities() -> pb::ClientMediaCapabilities {
    let codecs: Vec<i32> = available_video_codecs()
        .into_iter()
        .map(|codec| codec as i32)
        .collect();

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

fn platform_has_capture_backend() -> bool {
    cfg!(any(
        target_os = "windows",
        target_os = "linux",
        target_os = "macos"
    ))
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
    let preferred_screenshare_codec = screen_video_codecs.first().copied();

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
            supports_screen_share: cfg!(feature = "screen-share")
                && platform_has_capture_backend()
                && !media_caps.encode.is_empty()
                && !media_caps.decode.is_empty(),
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
        screen_share: preferred_screenshare_codec.map(|codec| pb::ScreenShareCaps {
            codec,
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
