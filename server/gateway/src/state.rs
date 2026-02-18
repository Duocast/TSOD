use anyhow::{anyhow, Result};
use bytes::Bytes;
use std::{collections::{HashMap, HashSet}, sync::Arc};
use tokio::sync::{mpsc, RwLock};

use vp_control::{
    ids::{ChannelId, ServerId, UserId},
    model::{ChannelCreate, JoinChannel},
    repo::PgControlRepo,
    service::{ControlService, RequestContext},
};

use vp_media::voice_forwarder as vf;

pub struct GatewayState {
    pub control: ControlService<PgControlRepo>,
    pub sessions: Arc<SessionRegistryImpl>,
    pub pushes: Arc<PushHub>,
    pub membership: Arc<MembershipCache>,
    pub voice: Arc<vf::VoiceForwarder>,
}

impl GatewayState {
    pub fn new(control: ControlService<PgControlRepo>) -> Self {
        let sessions = Arc::new(SessionRegistryImpl::default());
        let pushes = Arc::new(PushHub::default());
        let membership = Arc::new(MembershipCache::default());

        let voice_cfg = vf::VoiceForwarderConfig {
            max_datagram_bytes: 1400,
            min_datagram_bytes: vf::VOICE_HEADER_LEN,
            sender_pps_limit: 200,
            sender_bps_limit: 200_000,
            per_receiver_queue: 64,
            talker_activity_window: std::time::Duration::from_millis(600),
            max_ts_skew_ms: 5_000,
            vad_required_for_talker: false,
        };

        let voice = Arc::new(vf::VoiceForwarder::new(
            voice_cfg,
            sessions.clone(),
            membership.clone(),
            Arc::new(vf::NoopMetrics),
        ));

        Self { control, sessions, pushes, membership, voice }
    }
}

/// Control-stream push hub.
/// IMPORTANT: one writer per conn owns SendStream; we enqueue to that writer via mpsc.
#[derive(Default)]
pub struct PushHub {
    inner: RwLock<HashMap<UserId, mpsc::Sender<crate::proto::voiceplatform::v1::ServerToClient>>>,
}

impl PushHub {
    pub async fn register(
        &self,
        user: UserId,
        tx: mpsc::Sender<crate::proto::voiceplatform::v1::ServerToClient>,
    ) {
        self.inner.write().await.insert(user, tx);
    }

    pub async fn unregister(&self, user: UserId) {
        self.inner.write().await.remove(&user);
    }

    pub async fn send_to(
        &self,
        user: UserId,
        msg: crate::proto::voiceplatform::v1::ServerToClient,
    ) {
        if let Some(tx) = self.inner.read().await.get(&user) {
            let _ = tx.try_send(msg); // drop if backpressured
        }
    }
}

/// QUIC datagram tx for voice_forwarder
pub struct QuinnDatagramTx {
    conn: quinn::Connection,
}
#[async_trait::async_trait]
impl vf::DatagramTx for QuinnDatagramTx {
    async fn send(&self, bytes: Bytes) -> Result<()> {
        self.conn.send_datagram(bytes).map_err(|e| anyhow!("send_datagram: {e}"))
    }
}

#[derive(Default)]
pub struct SessionRegistryImpl {
    inner: RwLock<HashMap<UserId, Arc<dyn vf::DatagramTx>>>,
}

impl SessionRegistryImpl {
    pub async fn register(&self, user: UserId, tx: Arc<dyn vf::DatagramTx>) {
        self.inner.write().await.insert(user, tx);
    }
    pub async fn unregister(&self, user: UserId) {
        self.inner.write().await.remove(&user);
    }
}

#[async_trait::async_trait]
impl vf::SessionRegistry for SessionRegistryImpl {
    async fn get_datagram_tx(&self, user: UserId) -> Option<Arc<dyn vf::DatagramTx>> {
        self.inner.read().await.get(&user).cloned()
    }
}

/// Membership cache used for both fast fanout and voice-forwarder decisions.
/// Source of truth is vp-control; cache is updated on join/leave/mute actions.
#[derive(Default)]
pub struct MembershipCache {
    user_channel: RwLock<HashMap<UserId, ChannelId>>,
    channel_members: RwLock<HashMap<ChannelId, HashMap<UserId, MemberState>>>,
    channel_max_talkers: RwLock<HashMap<ChannelId, usize>>,
}

#[derive(Clone, Debug)]
pub struct MemberState {
    pub display_name: String,
    pub muted: bool,
    pub deafened: bool,
}

impl MembershipCache {
    pub async fn set_channel_state(
        &self,
        channel: ChannelId,
        members: Vec<(UserId, MemberState)>,
        max_talkers: usize,
    ) {
        let mut map = HashMap::new();
        for (uid, st) in members {
            self.user_channel.write().await.insert(uid, channel);
            map.insert(uid, st);
        }
        self.channel_members.write().await.insert(channel, map);
        self.channel_max_talkers.write().await.insert(channel, max_talkers);
    }

    pub async fn remove_member(&self, channel: ChannelId, user: UserId) {
        self.user_channel.write().await.remove(&user);
        if let Some(ch) = self.channel_members.write().await.get_mut(&channel) {
            ch.remove(&user);
        }
    }

    pub async fn members(&self, channel: ChannelId) -> Vec<UserId> {
        self.channel_members
            .read()
            .await
            .get(&channel)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub async fn member_state(&self, channel: ChannelId, user: UserId) -> Option<MemberState> {
        self.channel_members
            .read()
            .await
            .get(&channel)
            .and_then(|m| m.get(&user).cloned())
    }

    pub async fn channel_for_user(&self, user: UserId) -> Option<ChannelId> {
        self.user_channel.read().await.get(&user).cloned()
    }
}

#[async_trait::async_trait]
impl vf::MembershipProvider for MembershipCache {
    async fn resolve_channel_for_sender(&self, sender: UserId, route_key: u32) -> Option<ChannelId> {
        let ch = self.channel_for_user(sender).await?;
        let expected = channel_route_hash(&ch);
        if expected == route_key { Some(ch) } else { None }
    }

    async fn list_members(&self, channel: ChannelId) -> Vec<UserId> {
        self.members(channel).await
    }

    async fn is_muted(&self, channel: ChannelId, sender: UserId) -> bool {
        self.member_state(channel, sender).await.map(|s| s.muted).unwrap_or(true)
    }

    async fn max_talkers(&self, channel: ChannelId) -> usize {
        self.channel_max_talkers.read().await.get(&channel).cloned().unwrap_or(4)
    }
}

/// MUST match client hashing if you route by hash.
pub fn channel_route_hash(ch: &ChannelId) -> u32 {
    // FNV-1a 32-bit over UUID string bytes
    const OFF: u32 = 0x811C9DC5;
    const PRIME: u32 = 0x0100_0193;
    let s = ch.0.to_string();
    let mut h = OFF;
    for &b in s.as_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Helper to build RequestContext
pub fn mk_ctx(server: ServerId, user: UserId, is_admin: bool) -> RequestContext {
    RequestContext { server_id: server, user_id: user, is_admin }
}

/// Control helpers used by gateway
pub async fn control_join(
    control: &ControlService<PgControlRepo>,
    ctx: &RequestContext,
    channel: ChannelId,
    display_name: String,
) -> Result<Vec<vp_control::model::Member>> {
    let join = JoinChannel { channel_id: channel, display_name };
    Ok(control.join_channel(ctx, join).await?)
}

pub async fn control_create_channel(
    control: &ControlService<PgControlRepo>,
    ctx: &RequestContext,
    name: String,
    parent: Option<ChannelId>,
) -> Result<vp_control::model::Channel> {
    Ok(control.create_channel(ctx, ChannelCreate {
        name,
        parent_id: parent,
        max_members: None,
        max_talkers: Some(4),
    }).await?)
}
