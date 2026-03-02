use anyhow::{anyhow, Context, Result};
use ring::rand::SecureRandom;
use scopeguard::defer;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    sync::mpsc,
    time::{timeout, Duration, Instant},
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
use vp_media::stream_forwarder::StreamForwarder;
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
    video: Arc<StreamForwarder>,
    media: Arc<MediaService>,
}

impl Gateway {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        auth: Arc<dyn AuthProvider>,
        alpn: String,
        control: Arc<ControlService<PgControlRepo>>,
        sessions: Sessions,
        push: PushHub,
        membership: MembershipCache,
        voice: Arc<VoiceForwarder>,
        video: Arc<StreamForwarder>,
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
            video,
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

        // Client must explicitly request an authoritative snapshot via
        // GetInitialStateSnapshotRequest after auth.

        // Control stream writes are performed inline in this task.

        let (push_tx, mut push_rx) = mpsc::channel::<pb::ServerToClient>(1024);
        self.push.register(user_id, &session_id, push_tx);

        // Register push + datagram
        self.sessions.register(
            user_id,
            &session_id,
            Arc::new(QuinnDatagramTx::new(conn.clone())),
        );

        let mut current_channel: Option<ChannelId> = None;
        let mut recovery_forwarded_at: HashMap<u64, Instant> = HashMap::new();
        let voice_forwarder = self.voice.clone();
        let video_forwarder = self.video.clone();
        defer! {
            let voice_forwarder = voice_forwarder.clone();
            let video_forwarder = video_forwarder.clone();
            let loop_session_id = session_id.clone();
            tokio::spawn(async move {
                voice_forwarder.unregister_session(user_id, &loop_session_id).await;
                video_forwarder.unregister_session(user_id, &loop_session_id).await;
            });
            self.push.unregister(user_id, &session_id);
            self.sessions.unregister(user_id, &session_id);
        }

        // Datagram recv loop: dispatch voice vs video by kind byte.
        let voice = self.voice.clone();
        let video = self.video.clone();
        let user_for_dg = user_id;
        let conn_dg = conn.clone();
        tokio::spawn(async move {
            const VOICE_DATAGRAM_QUEUE_CAPACITY: usize = 1024;
            const VIDEO_DATAGRAM_QUEUE_CAPACITY: usize = 8192;
            const VIDEO_DATAGRAM_WORKERS: usize = 2;

            let oversized_drops = Arc::new(AtomicU64::new(0));
            let voice_queue_full_drops = Arc::new(AtomicU64::new(0));
            let voice_queue_closed_drops = Arc::new(AtomicU64::new(0));
            let video_queue_full_drops = Arc::new(AtomicU64::new(0));
            let video_queue_closed_drops = Arc::new(AtomicU64::new(0));
            let video_rx_count = Arc::new(AtomicU64::new(0));
            let video_rx_bytes = Arc::new(AtomicU64::new(0));

            let (voice_dg_tx, mut voice_dg_rx) =
                mpsc::channel::<bytes::Bytes>(VOICE_DATAGRAM_QUEUE_CAPACITY);
            let mut video_senders = Vec::with_capacity(VIDEO_DATAGRAM_WORKERS);
            for _ in 0..VIDEO_DATAGRAM_WORKERS {
                let (video_dg_tx, mut video_dg_rx) =
                    mpsc::channel::<bytes::Bytes>(VIDEO_DATAGRAM_QUEUE_CAPACITY);
                video_senders.push(video_dg_tx);

                let video = video.clone();
                let user_for_video = user_for_dg;
                tokio::spawn(async move {
                    while let Some(d) = video_dg_rx.recv().await {
                        video.handle_incoming_datagram(user_for_video, d).await;
                    }
                });
            }

            let voice_for_worker = voice.clone();
            let user_for_voice = user_for_dg;
            tokio::spawn(async move {
                while let Some(d) = voice_dg_rx.recv().await {
                    voice_for_worker.handle_incoming(user_for_voice, d).await;
                }
            });

            let mut video_rr = 0usize;
            let mut last_log = Instant::now();
            while let Ok(d) = conn_dg.read_datagram().await {
                if d.len() > vp_voice::APP_MEDIA_MTU {
                    oversized_drops.fetch_add(1, Ordering::Relaxed);
                    if last_log.elapsed() >= Duration::from_secs(1) {
                        let drops = oversized_drops.swap(0, Ordering::Relaxed);
                        if drops > 0 {
                            warn!(oversized_drops = drops, "dropping oversized datagrams");
                        }
                        last_log = Instant::now();
                    }
                    continue;
                }

                // Dispatch: byte[1] == 0x02 → video, otherwise → voice.
                if is_video_datagram(&d) {
                    video_rx_count.fetch_add(1, Ordering::Relaxed);
                    video_rx_bytes.fetch_add(d.len() as u64, Ordering::Relaxed);
                    if !video_senders.is_empty() {
                        let idx = video_rr % video_senders.len();
                        video_rr = video_rr.wrapping_add(1);
                        if let Err(err) = video_senders[idx].try_send(d) {
                            match err {
                                mpsc::error::TrySendError::Full(_) => {
                                    video_queue_full_drops.fetch_add(1, Ordering::Relaxed);
                                }
                                mpsc::error::TrySendError::Closed(_) => {
                                    video_queue_closed_drops.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                } else if let Err(err) = voice_dg_tx.try_send(d) {
                    match err {
                        mpsc::error::TrySendError::Full(_) => {
                            voice_queue_full_drops.fetch_add(1, Ordering::Relaxed);
                        }
                        mpsc::error::TrySendError::Closed(_) => {
                            voice_queue_closed_drops.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }

                if last_log.elapsed() >= Duration::from_secs(1) {
                    let c = video_rx_count.swap(0, Ordering::Relaxed);
                    let b = video_rx_bytes.swap(0, Ordering::Relaxed);
                    let video_drops = video_queue_full_drops.swap(0, Ordering::Relaxed);
                    let video_closed = video_queue_closed_drops.swap(0, Ordering::Relaxed);
                    if c > 0 {
                        info!(
                            "[video] server rx datagrams/sec={} bytes/sec={} queue_full_drops/sec={}",
                            c, b, video_drops
                        );
                    } else if video_drops > 0 {
                        info!(
                            "[video] server rx datagrams/sec=0 bytes/sec=0 queue_full_drops/sec={}",
                            video_drops
                        );
                    }
                    if video_closed > 0 {
                        warn!(
                            video_queue_closed_drops = video_closed,
                            "[video] datagram worker channel closed"
                        );
                    }
                    let voice_closed = voice_queue_closed_drops.swap(0, Ordering::Relaxed);
                    if voice_closed > 0 {
                        warn!(
                            voice_queue_closed_drops = voice_closed,
                            "[voice] datagram worker channel closed"
                        );
                    }
                    let drops = oversized_drops.swap(0, Ordering::Relaxed);
                    if drops > 0 {
                        warn!(oversized_drops = drops, "dropping oversized datagrams");
                    }
                    last_log = Instant::now();
                }
            }

            let oversized = oversized_drops.load(Ordering::Relaxed);
            let voice_drops = voice_queue_full_drops.load(Ordering::Relaxed);
            let voice_closed = voice_queue_closed_drops.load(Ordering::Relaxed);
            let video_drops = video_queue_full_drops.load(Ordering::Relaxed);
            let video_closed = video_queue_closed_drops.load(Ordering::Relaxed);
            if oversized > 0
                || voice_drops > 0
                || voice_closed > 0
                || video_drops > 0
                || video_closed > 0
            {
                warn!(
                    oversized,
                    voice_drops,
                    voice_closed,
                    video_drops,
                    video_closed,
                    "datagram recv loop ended with dropped datagrams"
                );
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
            while let Ok((send_s, recv_s)) = conn_media.accept_bi().await {
                let media = media.clone();
                tokio::spawn(async move {
                    if let Err(e) = media.handle_stream(send_s, recv_s, user_id).await {
                        warn!("media stream failed: {:#}", e);
                    }
                });
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

                let req_id = msg.request_id;

            let request_result: Result<()> = {
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
                        name: chan.name.clone(),
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
                        info: Some(pb::ChannelInfo {
                            channel_id: Some(pb::ChannelId {
                                value: ch.0.to_string(),
                            }),
                            name: chan.name,
                            channel_type: chan.channel_type,
                            description: chan.description,
                            parent_channel_id: chan.parent_id.map(|pid| pb::ChannelId {
                                value: pid.0.to_string(),
                            }),
                            user_limit: chan.max_members.unwrap_or_default().max(0) as u32,
                            bitrate: chan.bitrate_bps.max(0) as u32,
                            opus_profile: chan.opus_profile,
                            ..Default::default()
                        }),
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
                    let user_limit = r.user_limit;
                    let bitrate_bps = (r.bitrate as i32).clamp(8_000, 510_000);
                    let created = self
                        .control
                        .create_channel(
                            &ctx,
                            ChannelCreate {
                                name: r.name,
                                parent_id: parent,
                                max_members: if user_limit == 0 {
                                    None
                                } else {
                                    Some(user_limit as i32)
                                },
                                max_talkers: None,
                                channel_type: r.channel_type,
                                description: r.description,
                                bitrate_bps,
                                opus_profile: r.opus_profile,
                            },
                        )
                        .await?;

                    debug!(server_id=%server_id.0, channel_id=%created.id.0, user_id=%user_id.0, "create_channel request committed in control service");
                    let state = pb::ChannelState {
                        channel_id: Some(pb::ChannelId {
                            value: created.id.0.to_string(),
                        }),
                        name: created.name.clone(),
                        members: vec![],
                        info: Some(pb::ChannelInfo {
                            channel_id: Some(pb::ChannelId {
                                value: created.id.0.to_string(),
                            }),
                            name: created.name,
                            channel_type: created.channel_type,
                            description: created.description,
                            parent_channel_id: created.parent_id.map(|pid| pb::ChannelId {
                                value: pid.0.to_string(),
                            }),
                            user_limit: created.max_members.unwrap_or_default().max(0) as u32,
                            bitrate: created.bitrate_bps.max(0) as u32,
                            opus_profile: created.opus_profile,
                            ..Default::default()
                        }),
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
                                    channel_type: renamed.channel_type,
                                    description: renamed.description,
                                    user_limit: renamed.max_members.unwrap_or_default().max(0)
                                        as u32,
                                    bitrate: renamed.bitrate_bps.max(0) as u32,
                                    opus_profile: renamed.opus_profile,
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
                        .ok_or(ControlError::InvalidArgument("target_user_id missing"))?;
                    let target = UserId(uuid::Uuid::parse_str(&target.value)
                        .map_err(|_| ControlError::InvalidArgument("invalid target_user_id"))?);
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
                Some(pb::client_to_server::Payload::PermListRoles(_)) => {
                    let roles = self.control.perm_list_roles(&ctx).await?;
                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId { value: session_id.clone() }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::PermListRoles(pb::PermListRolesResponse {
                            roles: roles.into_iter().map(|r| pb::PermRole { role_id: r.role_id, name: r.name, color: r.color.max(0) as u32, position: r.role_position.max(0) as u32, is_everyone: r.is_everyone, is_system: false }).collect(),
                            roles_with_caps: vec![],
                        })),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await { warn!("control write failed: {:#}", e); break; }
                }
                Some(pb::client_to_server::Payload::PermUpsertRole(r)) => {
                    let role = self.control.perm_upsert_role(&ctx, (!r.role_id.is_empty()).then_some(r.role_id.as_str()), &r.name, r.color as i32, r.position as i32).await?;
                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId { value: session_id.clone() }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::PermUpsertRole(pb::PermUpsertRoleResponse { role: Some(pb::PermRole { role_id: role.role_id, name: role.name, color: role.color.max(0) as u32, position: role.role_position.max(0) as u32, is_everyone: role.is_everyone, is_system: false }) })),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await { warn!("control write failed: {:#}", e); break; }
                }
                Some(pb::client_to_server::Payload::PermDeleteRole(r)) => {
                    self.control.perm_delete_role(&ctx, &r.role_id).await?;
                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId { value: session_id.clone() }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::PermDeleteRole(pb::PermDeleteRoleResponse {})),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await { warn!("control write failed: {:#}", e); break; }
                }
                Some(pb::client_to_server::Payload::PermSetRoleCaps(r)) => {
                    let caps = r.caps.into_iter().map(|c| (c.cap, c.effect)).collect::<Vec<_>>();
                    self.control.perm_set_role_caps(&ctx, &r.role_id, &caps).await?;
                    let resp = pb::ServerToClient { request_id: req_id, session_id: Some(pb::SessionId { value: session_id.clone() }), sent_at: Some(now_ts()), error: None, event_seq: 0, payload: Some(pb::server_to_client::Payload::PermSetRoleCaps(pb::PermSetRoleCapsResponse {})) };
                    if let Err(e) = write_delimited(&mut send, &resp).await { warn!("control write failed: {:#}", e); break; }
                }
                Some(pb::client_to_server::Payload::PermAssignRoles(r)) => {
                    let target = parse_user_id(r.user_id.as_ref())?;
                    self.control.perm_assign_roles(&ctx, target, &r.role_ids).await?;
                    let resp = pb::ServerToClient { request_id: req_id, session_id: Some(pb::SessionId { value: session_id.clone() }), sent_at: Some(now_ts()), error: None, event_seq: 0, payload: Some(pb::server_to_client::Payload::PermAssignRoles(pb::PermAssignRolesResponse { role_ids: r.role_ids.clone() })) };
                    if let Err(e) = write_delimited(&mut send, &resp).await { warn!("control write failed: {:#}", e); break; }
                }
                Some(pb::client_to_server::Payload::PermListChanOvr(r)) => {
                    let channel_id = parse_channel_id(r.channel_id.as_ref())?;
                    let rows = self.control.perm_list_channel_overrides(&ctx, channel_id).await?;
                    let overrides = rows.into_iter().map(|row| {
                        let target = if let Some(role_id) = row.role_id { pb::perm_channel_override::Target::RoleId(role_id) } else { pb::perm_channel_override::Target::UserId(pb::UserId { value: row.user_id.expect("user id").0.to_string() }) };
                        pb::PermChannelOverride { channel_id: Some(pb::ChannelId { value: row.channel_id.0.to_string() }), target: Some(target), cap: row.cap, effect: row.effect }
                    }).collect();
                    let resp = pb::ServerToClient { request_id: req_id, session_id: Some(pb::SessionId { value: session_id.clone() }), sent_at: Some(now_ts()), error: None, event_seq: 0, payload: Some(pb::server_to_client::Payload::PermListChanOvr(pb::PermListChannelOverridesResponse { overrides, role_overrides: vec![], user_overrides: vec![] })) };
                    if let Err(e) = write_delimited(&mut send, &resp).await { warn!("control write failed: {:#}", e); break; }
                }
                Some(pb::client_to_server::Payload::PermSetChanOvr(r)) => {
                    let o = r.r#override.ok_or(ControlError::InvalidArgument("override missing"))?;
                    let channel_id = parse_channel_id(o.channel_id.as_ref())?;
                    let (role_id, user_id) = match o.target {
                        Some(pb::perm_channel_override::Target::RoleId(role_id)) => (Some(role_id), None),
                        Some(pb::perm_channel_override::Target::UserId(user_id)) => (None, Some(parse_user_id(Some(&user_id))?)),
                        None => return Err(ControlError::InvalidArgument("override target missing").into()),
                    };
                    let rec = vp_control::PermChannelOverrideRecord { channel_id, role_id, user_id, cap: o.cap, effect: o.effect };
                    self.control.perm_set_channel_override(&ctx, &rec).await?;
                    let resp = pb::ServerToClient { request_id: req_id, session_id: Some(pb::SessionId { value: session_id.clone() }), sent_at: Some(now_ts()), error: None, event_seq: 0, payload: Some(pb::server_to_client::Payload::PermSetChanOvr(pb::PermSetChannelOverrideResponse {})) };
                    if let Err(e) = write_delimited(&mut send, &resp).await { warn!("control write failed: {:#}", e); break; }
                }
                Some(pb::client_to_server::Payload::PermAuditQuery(r)) => {
                    let rows = self.control.perm_audit_query(&ctx, r.limit as i64).await?;
                    let resp = pb::ServerToClient { request_id: req_id, session_id: Some(pb::SessionId { value: session_id.clone() }), sent_at: Some(now_ts()), error: None, event_seq: 0, payload: Some(pb::server_to_client::Payload::PermAuditQuery(pb::PermAuditQueryResponse { rows: rows.into_iter().map(|row| pb::PermAuditRow { action: row.action, target_type: row.target_type, target_id: row.target_id, created_at: Some(pb::Timestamp { unix_millis: row.created_at.timestamp_millis() }) }).collect() })) };
                    if let Err(e) = write_delimited(&mut send, &resp).await { warn!("control write failed: {:#}", e); break; }
                }
                Some(pb::client_to_server::Payload::PermEvalEffective(r)) => {
                    let target = parse_user_id(r.user_id.as_ref())?;
                    let channel_id = if let Some(ch) = r.channel_id.as_ref() { Some(parse_channel_id(Some(ch))?) } else { None };
                    let entries = self.control.perm_eval_effective(&ctx, target, channel_id, &r.caps).await?;
                    let resp = pb::ServerToClient { request_id: req_id, session_id: Some(pb::SessionId { value: session_id.clone() }), sent_at: Some(now_ts()), error: None, event_seq: 0, payload: Some(pb::server_to_client::Payload::PermEvalEffective(pb::PermEvaluateEffectiveResponse { entries: entries.into_iter().map(|(cap,allowed)| pb::PermEvaluateEntry { cap, allowed }).collect(), explain: vec![] })) };
                    if let Err(e) = write_delimited(&mut send, &resp).await { warn!("control write failed: {:#}", e); break; }
                }
                Some(pb::client_to_server::Payload::PermListUsers(_)) => {
                    let (users, editor_highest_role_position, editor_is_admin) =
                        self.control.perm_list_users(&ctx).await?;
                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId {
                            value: session_id.clone(),
                        }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::PermListUsers(
                            pb::PermListUsersResponse {
                                users: users
                                    .into_iter()
                                    .map(|u| pb::PermUserSummary {
                                        user_id: Some(pb::UserId {
                                            value: u.user_id.0.to_string(),
                                        }),
                                        display_name: u.display_name,
                                        joined_at: u.joined_at.map(|t| pb::Timestamp {
                                            unix_millis: t.timestamp_millis(),
                                        }),
                                        last_seen: u.last_seen.map(|t| pb::Timestamp {
                                            unix_millis: t.timestamp_millis(),
                                        }),
                                        highest_role_position: u.highest_role_position,
                                        role_ids: u.role_ids,
                                        is_admin: u.is_admin,
                                    })
                                    .collect(),
                                editor_highest_role_position,
                                editor_is_admin,
                            },
                        )),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::StartScreenShareRequest(r)) => {
                    let ch = parse_channel_id(r.channel_id.as_ref())?;

                    let streamer_caps = self.membership.streamer_encode_codecs(user_id);
                    let mut viewer_caps = HashMap::new();
                    for viewer in self.membership.members_of(ch).unwrap_or_default().into_iter().filter(|v| *v != user_id) {
                        viewer_caps.insert(viewer, self.membership.viewer_decode_codecs(viewer));
                    }
                    let plan = negotiate_codecs(&streamer_caps, &viewer_caps)?;
                    let streamer_media_caps = self.membership.media_capabilities(user_id);
                    let mut viewer_media_caps = self.membership.channel_member_capabilities(ch);
                    viewer_media_caps.remove(&user_id);
                    let requested_1440p60 = r.layers.iter().any(|l| l.width >= 2560 || l.height >= 1440);
                    let allow_1440p60 = requested_1440p60
                        && allows_1440p60(plan.primary, streamer_media_caps.as_ref(), &viewer_media_caps);

                    let primary_tag = random_stream_tag()?;
                    self.video
                        .register_stream(
                            primary_tag,
                            vp_media::stream_forwarder::StreamRegistration {
                                sender_id: user_id,
                                channel_id: ch,
                                codec: plan.primary as i32,
                            },
                        )
                        .await;
                    let mut primary_viewers = plan.primary_viewers.clone();
                    if !primary_viewers.contains(&user_id) {
                        primary_viewers.push(user_id);
                    }
                    self.video
                        .set_stream_subscribers(primary_tag, primary_viewers.iter().copied())
                        .await;

                    for viewer in &primary_viewers {
                        self.push.send_to(*viewer, pb::ServerToClient {
                            request_id: None,
                            session_id: None,
                            sent_at: Some(now_ts()),
                            error: None,
                            event_seq: 0,
                            payload: Some(pb::server_to_client::Payload::SubscribeStream(pb::SubscribeStream { stream_tag: primary_tag, codec: plan.primary as i32 })),
                        }).await;
                    }

                    let fallback_tag = if let Some(fallback_codec) = plan.fallback {
                        let tag = random_stream_tag()?;
                        self.video.register_stream(tag, vp_media::stream_forwarder::StreamRegistration { sender_id: user_id, channel_id: ch, codec: fallback_codec as i32 }).await;
                        self.video.set_stream_subscribers(tag, plan.remaining_viewers.iter().copied()).await;
                        for viewer in &plan.remaining_viewers {
                            self.push.send_to(*viewer, pb::ServerToClient {
                                request_id: None,
                                session_id: None,
                                sent_at: Some(now_ts()),
                                error: None,
                                event_seq: 0,
                                payload: Some(pb::server_to_client::Payload::SubscribeStream(pb::SubscribeStream { stream_tag: tag, codec: fallback_codec as i32 })),
                            }).await;
                        }
                        Some((tag, fallback_codec))
                    } else {
                        None
                    };

                    let stream_id = format!("{:016x}", primary_tag);
                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId { value: session_id.clone() }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::StartScreenShareResponse(
                            pb::StartScreenShareResponse {
                                stream_id: Some(pb::StreamId { value: stream_id }),
                                accepted_layer_ids: r
                                    .layers
                                    .iter()
                                    .filter(|layer| allow_1440p60 || (layer.width <= 1920 && layer.height <= 1080))
                                    .map(|l| l.layer_id)
                                    .collect(),
                                primary_stream_tag: primary_tag,
                                primary_codec: plan.primary as i32,
                                fallback_stream_tag: fallback_tag.as_ref().map(|v| v.0),
                                fallback_codec: fallback_tag.as_ref().map(|v| v.1 as i32),
                            },
                        )),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::StopScreenShareRequest(r)) => {
                    if let Some(sid) = r.stream_id.as_ref() {
                        if let Ok(stream_tag) = u64::from_str_radix(&sid.value, 16) {
                            for viewer in self.video.subscribers_for_stream(stream_tag).await {
                                self.push.send_to(viewer, pb::ServerToClient {
                                    request_id: None,
                                    session_id: None,
                                    sent_at: Some(now_ts()),
                                    error: None,
                                    event_seq: 0,
                                    payload: Some(pb::server_to_client::Payload::UnsubscribeStream(pb::UnsubscribeStream { stream_tag })),
                                }).await;
                            }
                            self.video.unregister_stream(stream_tag).await;
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
                        payload: Some(pb::server_to_client::Payload::StopScreenShareResponse(
                            pb::StopScreenShareResponse {},
                        )),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::CapabilitiesUpdate(r)) => {
                    if let Some(caps) = r.caps {
                        self.membership.set_media_capabilities(user_id, caps);
                    }
                    let resp = pb::ServerToClient {
                        request_id: req_id,
                        session_id: Some(pb::SessionId { value: session_id.clone() }),
                        sent_at: Some(now_ts()),
                        error: None,
                        event_seq: 0,
                        payload: Some(pb::server_to_client::Payload::CapabilitiesUpdateAck(pb::CapabilitiesUpdateAck {})),
                    };
                    if let Err(e) = write_delimited(&mut send, &resp).await {
                        warn!("control write failed: {:#}", e);
                        break;
                    }
                }
                Some(pb::client_to_server::Payload::RequestRecovery(r)) => {
                    let now = Instant::now();
                    let should_forward = recovery_forwarded_at
                        .get(&r.stream_tag)
                        .map(|t| now.duration_since(*t) >= Duration::from_millis(500))
                        .unwrap_or(true);
                    if should_forward {
                        recovery_forwarded_at.insert(r.stream_tag, now);
                        self.video.note_recovery_request();
                        if let Some(sender_uid) = self.video.sender_for_stream(r.stream_tag).await {
                            self.push.send_to(sender_uid, pb::ServerToClient {
                                request_id: None,
                                session_id: None,
                                sent_at: Some(now_ts()),
                                error: None,
                                event_seq: 0,
                                payload: Some(pb::server_to_client::Payload::RequestRecovery(pb::RequestRecovery { stream_tag: r.stream_tag })),
                            }).await;
                        }
                    }
                }
                _ => {
                    // Ignore other messages for now.
                }
            }
            Ok(())
            };

            if let Err(err) = request_result {
                if matches!(err.downcast_ref::<ControlError>(), Some(ControlError::PermissionDenied(_))) {
                    debug!(
                        session_id = %session_id,
                        user_id = %user_id.0,
                        error = %err,
                        "permission denied; keeping connection alive"
                    );
                } else {
                    warn!(
                        session_id = %session_id,
                        user_id = %user_id.0,
                        error = %err,
                        "request failed"
                    );
                }

                let resp = pb::ServerToClient {
                    request_id: req_id,
                    session_id: Some(pb::SessionId {
                        value: session_id.clone(),
                    }),
                    sent_at: Some(now_ts()),
                    error: Some(error_from_anyhow(&err)),
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

        match self.control.disconnect_user(&ctx).await {
            Ok(channels) => {
                self.membership.remove_user(user_id);
                for ch in channels {
                    if let Some(mut cur) = self.membership.members_of(ch) {
                        cur.retain(|u| *u != user_id);
                        let max = self.membership.max_talkers_of(ch).unwrap_or(4);
                        self.membership.set_channel_state(ch, max, cur);
                    }
                }
            }
            Err(e) => {
                warn!(
                    user_id = %user_id.0,
                    error = %e,
                    "disconnect cleanup failed"
                );
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
                    channel_type: channel.channel_type,
                    description: channel.description.clone(),
                    parent_channel_id: channel.parent_id.map(|pid| pb::ChannelId {
                        value: pid.0.to_string(),
                    }),
                    user_limit: channel.max_members.unwrap_or_default().max(0) as u32,
                    bitrate: channel.bitrate_bps.max(0) as u32,
                    opus_profile: channel.opus_profile,
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

fn random_stream_tag() -> Result<u64> {
    let mut buf = [0u8; 8];
    ring::rand::SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| anyhow!("rng failed"))?;
    Ok(u64::from_le_bytes(buf))
}

#[derive(Clone, Debug)]
struct CodecPlan {
    primary: pb::VideoCodec,
    primary_viewers: Vec<UserId>,
    fallback: Option<pb::VideoCodec>,
    remaining_viewers: Vec<UserId>,
}

fn codec_rank() -> [pb::VideoCodec; 3] {
    [
        pb::VideoCodec::Av1,
        pb::VideoCodec::Vp9,
        pb::VideoCodec::Vp8,
    ]
}

fn codec_hw_encode(caps: &pb::ClientMediaCapabilities, codec: pb::VideoCodec) -> bool {
    match codec {
        pb::VideoCodec::Av1 => caps.hw_encode_av1,
        pb::VideoCodec::Vp9 => caps.hw_encode_vp9,
        pb::VideoCodec::Vp8 => caps.hw_encode_vp8,
        _ => false,
    }
}

fn codec_hw_decode(caps: &pb::ClientMediaCapabilities, codec: pb::VideoCodec) -> bool {
    match codec {
        pb::VideoCodec::Av1 => caps.hw_decode_av1,
        pb::VideoCodec::Vp9 => caps.hw_decode_vp9,
        pb::VideoCodec::Vp8 => caps.hw_decode_vp8,
        _ => false,
    }
}

fn allows_1440p60(
    codec: pb::VideoCodec,
    streamer_caps: Option<&pb::ClientMediaCapabilities>,
    viewer_caps: &HashMap<UserId, pb::ClientMediaCapabilities>,
) -> bool {
    let Some(streamer_caps) = streamer_caps else {
        return false;
    };
    if !codec_hw_encode(streamer_caps, codec) {
        return false;
    }
    viewer_caps
        .values()
        .all(|caps| codec_hw_decode(caps, codec))
}

fn negotiate_codecs(
    streamer: &[pb::VideoCodec],
    viewers: &HashMap<UserId, Vec<pb::VideoCodec>>,
) -> Result<CodecPlan> {
    let mut effective_streamer = streamer.to_vec();
    if effective_streamer.is_empty() {
        warn!("streamer missing codec capabilities; defaulting to VP8 encode support");
        effective_streamer.push(pb::VideoCodec::Vp8);
    }

    let sset: HashSet<i32> = effective_streamer.iter().map(|c| *c as i32).collect();

    if viewers.is_empty() {
        let primary = codec_rank()
            .into_iter()
            .find(|c| sset.contains(&(*c as i32)))
            .ok_or(ControlError::FailedPrecondition(
                "streamer missing codec capabilities",
            ))?;
        let fallback =
            if primary != pb::VideoCodec::Vp8 && sset.contains(&(pb::VideoCodec::Vp8 as i32)) {
                Some(pb::VideoCodec::Vp8)
            } else {
                None
            };

        return Ok(CodecPlan {
            primary,
            primary_viewers: Vec::new(),
            fallback,
            remaining_viewers: Vec::new(),
        });
    }

    let mut support: HashMap<UserId, HashSet<i32>> = HashMap::new();
    for (uid, di) in viewers {
        let set: HashSet<i32> = di
            .iter()
            .map(|c| *c as i32)
            .filter(|c| sset.contains(c))
            .collect();
        support.insert(*uid, set);
    }
    let primary = codec_rank()
        .into_iter()
        .find(|c| support.values().any(|set| set.contains(&(*c as i32))))
        .ok_or(ControlError::FailedPrecondition("viewer unsupported"))?;

    let mut primary_viewers = Vec::new();
    let mut remaining = Vec::new();
    for (uid, set) in &support {
        if set.contains(&(primary as i32)) {
            primary_viewers.push(*uid);
        } else {
            remaining.push(*uid);
        }
    }

    let fallback = if remaining.is_empty() {
        None
    } else {
        codec_rank()
            .into_iter()
            .find(|c| {
                remaining.iter().all(|uid| {
                    support
                        .get(uid)
                        .map(|s| s.contains(&(*c as i32)))
                        .unwrap_or(false)
                })
            })
            .or_else(|| {
                if sset.contains(&(pb::VideoCodec::Vp8 as i32)) {
                    Some(pb::VideoCodec::Vp8)
                } else {
                    None
                }
            })
    };

    if !remaining.is_empty() && fallback.is_none() {
        return Err(ControlError::FailedPrecondition("viewer unsupported").into());
    }

    Ok(CodecPlan {
        primary,
        primary_viewers,
        fallback,
        remaining_viewers: remaining,
    })
}

fn parse_user_id(u: Option<&pb::UserId>) -> Result<UserId> {
    let u = u.ok_or(ControlError::InvalidArgument("user_id missing"))?;
    Ok(UserId(uuid::Uuid::parse_str(&u.value).map_err(|_| {
        ControlError::InvalidArgument("invalid user_id")
    })?))
}

fn parse_channel_id(ch: Option<&pb::ChannelId>) -> Result<ChannelId> {
    let ch = ch.ok_or(ControlError::InvalidArgument("channel_id missing"))?;
    Ok(ChannelId(uuid::Uuid::parse_str(&ch.value).map_err(
        |_| ControlError::InvalidArgument("invalid channel_id"),
    )?))
}

fn error_from_anyhow(err: &anyhow::Error) -> pb::Error {
    let (code, message) = if let Some(control_err) = err.downcast_ref::<ControlError>() {
        match control_err {
            ControlError::NotFound(msg) => (pb::error::Code::NotFound as i32, *msg),
            ControlError::AlreadyExists(msg) => (pb::error::Code::AlreadyExists as i32, *msg),
            ControlError::InvalidArgument(msg) => (pb::error::Code::InvalidArgument as i32, *msg),
            ControlError::PermissionDenied(msg) => (pb::error::Code::PermissionDenied as i32, *msg),
            ControlError::ResourceExhausted(msg) => {
                (pb::error::Code::ResourceExhausted as i32, *msg)
            }
            ControlError::FailedPrecondition(msg) => {
                (pb::error::Code::FailedPrecondition as i32, *msg)
            }
            ControlError::Db(_) => (pb::error::Code::Unavailable as i32, "database unavailable"),
            ControlError::Anyhow(_) => (pb::error::Code::Internal as i32, "internal error"),
        }
    } else {
        (pb::error::Code::Internal as i32, "internal error")
    };

    pb::Error {
        code,
        message: message.to_string(),
        detail: format!("{:#}", err),
    }
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

fn is_video_datagram(d: &[u8]) -> bool {
    d.len() >= 2 && d[0] == vp_voice::VIDEO_VERSION && d[1] == vp_voice::DATAGRAM_KIND_VIDEO
}

#[cfg(test)]
mod tests {
    use super::{
        allows_1440p60, error_from_anyhow, is_video_datagram, negotiate_codecs,
        normalize_preferred_display_name,
    };
    use crate::proto::voiceplatform::v1 as pb;
    use std::collections::HashMap;
    use vp_control::ControlError;

    #[test]
    fn permission_denied_maps_to_permission_denied_error_response() {
        let err = anyhow::Error::new(ControlError::PermissionDenied("denied"));
        let mapped = error_from_anyhow(&err);
        assert_eq!(mapped.code, pb::error::Code::PermissionDenied as i32);
    }

    #[test]
    fn voice_flags_0x02_is_not_video_datagram() {
        // Voice packets use byte[1] as flags; 0x02 (e.g., FEC) must not route as video.
        let voice_like = [vp_voice::VOICE_VERSION, 0x02, 0, 0];
        assert!(!is_video_datagram(&voice_like));

        let video = [vp_voice::VIDEO_VERSION, vp_voice::DATAGRAM_KIND_VIDEO, 0, 0];
        assert!(is_video_datagram(&video));
    }

    #[test]
    fn negotiate_codecs_primary_and_fallback() {
        let streamer = vec![
            pb::VideoCodec::Av1,
            pb::VideoCodec::Vp9,
            pb::VideoCodec::Vp8,
        ];
        let a = vp_control::ids::UserId::new();
        let b = vp_control::ids::UserId::new();
        let viewers = HashMap::from([
            (a, vec![pb::VideoCodec::Av1, pb::VideoCodec::Vp9]),
            (b, vec![pb::VideoCodec::Vp8]),
        ]);

        let plan = negotiate_codecs(&streamer, &viewers).expect("plan");
        assert_eq!(plan.primary, pb::VideoCodec::Av1);
        assert_eq!(plan.fallback, Some(pb::VideoCodec::Vp8));
    }

    #[test]
    fn negotiate_codecs_rejects_unsupported() {
        let streamer = vec![pb::VideoCodec::Av1];
        let a = vp_control::ids::UserId::new();
        let viewers = HashMap::from([(a, vec![pb::VideoCodec::Vp8])]);
        let err = negotiate_codecs(&streamer, &viewers).expect_err("should fail");
        let control = err
            .downcast_ref::<ControlError>()
            .expect("should return control error");
        match control {
            ControlError::FailedPrecondition(msg) => assert_eq!(*msg, "viewer unsupported"),
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn negotiate_codecs_accepts_no_viewers() {
        let streamer = vec![pb::VideoCodec::Vp9, pb::VideoCodec::Vp8];
        let viewers = HashMap::new();

        let plan = negotiate_codecs(&streamer, &viewers).expect("plan");
        assert_eq!(plan.primary, pb::VideoCodec::Vp9);
        assert!(plan.primary_viewers.is_empty());
        assert!(plan.remaining_viewers.is_empty());
        assert_eq!(plan.fallback, Some(pb::VideoCodec::Vp8));
    }

    #[test]
    fn negotiate_codecs_defaults_streamer_caps_when_missing() {
        let streamer = vec![];
        let viewers = HashMap::new();

        let plan = negotiate_codecs(&streamer, &viewers).expect("plan");
        assert_eq!(plan.primary, pb::VideoCodec::Vp8);
        assert_eq!(plan.fallback, None);
    }

    #[test]
    fn allows_1440_requires_hw_encode_and_decode() {
        let streamer = pb::ClientMediaCapabilities {
            decode: vec![pb::VideoCodec::Av1 as i32],
            encode: vec![pb::VideoCodec::Av1 as i32],
            hw_encode_av1: true,
            hw_encode_vp9: false,
            hw_encode_vp8: false,
            hw_decode_av1: false,
            hw_decode_vp9: false,
            hw_decode_vp8: false,
        };
        let viewer = pb::ClientMediaCapabilities {
            decode: vec![pb::VideoCodec::Av1 as i32],
            encode: vec![],
            hw_encode_av1: false,
            hw_encode_vp9: false,
            hw_encode_vp8: false,
            hw_decode_av1: true,
            hw_decode_vp9: false,
            hw_decode_vp8: false,
        };
        let uid = vp_control::ids::UserId::new();
        let viewers = HashMap::from([(uid, viewer.clone())]);
        assert!(allows_1440p60(
            pb::VideoCodec::Av1,
            Some(&streamer),
            &viewers
        ));

        let mut bad_viewers = viewers.clone();
        bad_viewers.insert(
            uid,
            pb::ClientMediaCapabilities {
                hw_decode_av1: false,
                ..viewer
            },
        );
        assert!(!allows_1440p60(
            pb::VideoCodec::Av1,
            Some(&streamer),
            &bad_viewers
        ));
    }
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
