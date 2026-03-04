use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::Result;
use bytes::Bytes;
use dashmap::DashMap;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::egress::EgressScheduler;
use crate::proto::voiceplatform::v1 as pb;

use vp_control::ids::{ChannelId, UserId};
use vp_media::stream_forwarder::ViewerProvider;
use vp_media::voice_forwarder::{DatagramTx, MembershipProvider, SessionRegistry};

#[derive(Clone)]
pub struct PushHub {
    inner: Arc<DashMap<(UserId, String), mpsc::Sender<pb::ServerToClient>>>,
}

impl PushHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    pub fn register(&self, user: UserId, session_id: &str, tx: mpsc::Sender<pb::ServerToClient>) {
        self.inner.insert((user, session_id.to_string()), tx);
    }

    pub fn unregister(&self, user: UserId, session_id: &str) {
        self.inner.remove(&(user, session_id.to_string()));
    }

    pub async fn send_to(&self, user: UserId, msg: pb::ServerToClient) {
        let targets = self
            .inner
            .iter()
            .filter(|entry| entry.key().0 == user)
            .map(|entry| entry.value().clone())
            .collect::<Vec<_>>();
        for tx in targets {
            let _ = tx.send(msg.clone()).await;
        }
    }

    pub async fn send(&self, user: UserId, msg: pb::ServerToClient) {
        self.send_to(user, msg).await;
    }

    pub fn connected_users(&self) -> Vec<UserId> {
        let mut seen = HashSet::new();
        self.inner
            .iter()
            .filter_map(|entry| {
                let uid = entry.key().0;
                seen.insert(uid).then_some(uid)
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct VoiceTelemetrySample {
    pub loss_rate: f32,
    pub rtt_ms: u32,
    pub jitter_ms: u32,
    pub goodput_bps: u32,
    pub playout_delay_ms: u32,
}

#[derive(Clone)]
pub struct VoiceTelemetryCache {
    inner: Arc<DashMap<UserId, VoiceTelemetrySample>>,
}

impl VoiceTelemetryCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    pub fn upsert(&self, user_id: UserId, sample: VoiceTelemetrySample) {
        self.inner.insert(user_id, sample);
    }

    pub fn remove(&self, user_id: UserId) {
        self.inner.remove(&user_id);
    }
}

pub fn channel_route_key(channel_id: ChannelId) -> u32 {
    vp_route_hash::channel_route_hash(channel_id.0)
}

#[derive(Clone)]
pub struct QuinnDatagramTx {
    egress: Arc<EgressScheduler>,
}

impl QuinnDatagramTx {
    pub fn new(conn: quinn::Connection) -> Self {
        Self {
            egress: EgressScheduler::new(conn),
        }
    }
}

#[async_trait::async_trait]
impl DatagramTx for QuinnDatagramTx {
    async fn send(&self, bytes: Bytes) -> Result<()> {
        if bytes.len() >= vp_voice::VIDEO_HEADER_BYTES
            && bytes[0] == vp_voice::VIDEO_VERSION
            && bytes[1] == vp_voice::DATAGRAM_KIND_VIDEO
        {
            let stream_tag = u64::from_le_bytes([
                bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9],
            ]);
            let flags = bytes[11];
            let frame_seq = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
            let is_keyframe = (flags & vp_voice::VIDEO_FLAG_KEYFRAME) != 0;
            self.egress.enqueue_video(
                bytes,
                stream_tag,
                frame_seq,
                is_keyframe,
                Instant::now() + std::time::Duration::from_millis(80),
            );
        } else {
            self.egress.enqueue_voice(bytes);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct SessionMap {
    inner: Arc<DashMap<(UserId, String), Arc<dyn DatagramTx>>>,
}

impl SessionMap {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    pub fn register(&self, user: UserId, session_id: &str, tx: Arc<dyn DatagramTx>) {
        self.inner.insert((user, session_id.to_string()), tx);
    }

    pub fn unregister(&self, user: UserId, session_id: &str) {
        self.inner.remove(&(user, session_id.to_string()));
    }
}

#[async_trait::async_trait]
impl SessionRegistry for SessionMap {
    async fn get_sessions(&self, user: UserId) -> Vec<(String, Arc<dyn DatagramTx>)> {
        self.inner
            .iter()
            .filter(|entry| entry.key().0 == user)
            .map(|entry| (entry.key().1.clone(), entry.value().clone()))
            .collect()
    }
}

#[derive(Clone, Debug)]
struct UserPresence {
    channel: ChannelId,
    route: u32,
    muted: bool,
    deafened: bool,
}

#[derive(Clone, Debug)]
struct ChannelRuntime {
    max_talkers: usize,
    members: Vec<UserId>,
}

#[derive(Clone)]
pub struct MembershipCache {
    users: Arc<DashMap<UserId, UserPresence>>,
    channels: Arc<DashMap<ChannelId, ChannelRuntime>>,
    media_caps: Arc<DashMap<UserId, pb::ClientMediaCapabilities>>,
}

impl MembershipCache {
    pub fn new() -> Self {
        Self {
            users: Arc::new(DashMap::new()),
            channels: Arc::new(DashMap::new()),
            media_caps: Arc::new(DashMap::new()),
        }
    }

    pub fn set_channel(&self, channel: ChannelId, max_talkers: usize, members: Vec<UserId>) {
        self.channels.insert(
            channel,
            ChannelRuntime {
                max_talkers,
                members,
            },
        );
    }

    pub fn set_channel_state(&self, channel: ChannelId, max_talkers: usize, members: Vec<UserId>) {
        self.set_channel(channel, max_talkers, members);
    }

    pub fn set_user(&self, user: UserId, channel: ChannelId, muted: bool, deafened: bool) {
        self.users.insert(
            user,
            UserPresence {
                channel,
                route: channel_route_key(channel),
                muted,
                deafened,
            },
        );
    }

    pub fn remove_user(&self, user: UserId) {
        self.users.remove(&user);
        self.media_caps.remove(&user);
    }

    pub fn add_channel_member(&self, channel: ChannelId, user: UserId) {
        if let Some(mut runtime) = self.channels.get_mut(&channel) {
            if !runtime.members.contains(&user) {
                runtime.members.push(user);
            }
        }
    }

    pub fn remove_channel_member(&self, channel: ChannelId, user: UserId) {
        if let Some(mut runtime) = self.channels.get_mut(&channel) {
            runtime.members.retain(|member| *member != user);
        }
    }

    pub fn set_media_capabilities(&self, user: UserId, caps: pb::ClientMediaCapabilities) {
        self.media_caps.insert(user, caps);
    }

    pub fn media_capabilities(&self, user: UserId) -> Option<pb::ClientMediaCapabilities> {
        self.media_caps.get(&user).map(|v| v.clone())
    }

    pub fn viewer_decode_codecs(&self, user: UserId) -> Vec<pb::VideoCodec> {
        self.media_capabilities(user)
            .map(|caps| {
                caps.decode
                    .into_iter()
                    .filter_map(|c| pb::VideoCodec::try_from(c).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    pub fn streamer_encode_codecs(&self, user: UserId) -> Vec<pb::VideoCodec> {
        self.media_capabilities(user)
            .map(|caps| {
                caps.encode
                    .into_iter()
                    .filter_map(|c| pb::VideoCodec::try_from(c).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    pub fn channel_member_capabilities(
        &self,
        channel: ChannelId,
    ) -> HashMap<UserId, pb::ClientMediaCapabilities> {
        let members = self
            .channels
            .get(&channel)
            .map(|entry| entry.members.clone())
            .unwrap_or_default();
        members
            .into_iter()
            .filter_map(|uid| self.media_capabilities(uid).map(|caps| (uid, caps)))
            .collect()
    }

    pub fn update_voice_state(
        &self,
        user: UserId,
        channel: ChannelId,
        muted: bool,
        deafened: bool,
    ) {
        self.users.insert(
            user,
            UserPresence {
                channel,
                route: channel_route_key(channel),
                muted,
                deafened,
            },
        );
    }

    pub fn update_mute(&self, user: UserId, channel: ChannelId, muted: bool) {
        let deafened = self
            .users
            .get(&user)
            .map(|entry| entry.deafened)
            .unwrap_or(false);
        self.update_voice_state(user, channel, muted, deafened);
    }

    pub fn update_deafen(&self, user: UserId, channel: ChannelId, deafened: bool) {
        let muted = self
            .users
            .get(&user)
            .map(|entry| entry.muted)
            .unwrap_or(false);
        self.update_voice_state(user, channel, muted, deafened);
    }

    pub fn members_of(&self, channel: ChannelId) -> Option<Vec<UserId>> {
        self.channels.get(&channel).map(|e| e.members.clone())
    }

    pub fn max_talkers_of(&self, channel: ChannelId) -> Option<usize> {
        self.channels.get(&channel).map(|e| e.max_talkers)
    }
}

#[async_trait::async_trait]
impl MembershipProvider for MembershipCache {
    async fn resolve_channel_for_sender(
        &self,
        sender: UserId,
        route_key: u32,
    ) -> Option<ChannelId> {
        let u = self.users.get(&sender)?;
        if u.route == route_key {
            Some(u.channel)
        } else {
            None
        }
    }

    async fn list_members(&self, channel: ChannelId) -> Vec<UserId> {
        self.channels
            .get(&channel)
            .map(|e| e.members.clone())
            .unwrap_or_default()
    }

    async fn is_muted(&self, _channel: ChannelId, sender: UserId) -> bool {
        self.users.get(&sender).map(|e| e.muted).unwrap_or(false)
    }

    async fn is_deafened(&self, _channel: ChannelId, user: UserId) -> bool {
        self.users.get(&user).map(|e| e.deafened).unwrap_or(false)
    }

    async fn max_talkers(&self, channel: ChannelId) -> usize {
        self.channels
            .get(&channel)
            .map(|e| e.max_talkers)
            .unwrap_or(4)
    }
}

#[async_trait::async_trait]
impl ViewerProvider for MembershipCache {
    async fn list_viewers(&self, channel: ChannelId, exclude_sender: UserId) -> Vec<UserId> {
        self.channels
            .get(&channel)
            .map(|e| {
                e.members
                    .iter()
                    .copied()
                    .filter(|u| *u != exclude_sender)
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{DatagramTx, MembershipCache, PushHub, SessionMap};
    use crate::proto::voiceplatform::v1 as pb;
    use anyhow::Result;
    use bytes::Bytes;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use vp_control::ids::{ChannelId, UserId};
    use vp_media::voice_forwarder::SessionRegistry;

    #[tokio::test]
    async fn pushhub_sends_to_all_sessions_for_same_user() {
        let hub = PushHub::new();
        let user = UserId(uuid::Uuid::new_v4());
        let (tx1, mut rx1) = mpsc::channel::<pb::ServerToClient>(4);
        let (tx2, mut rx2) = mpsc::channel::<pb::ServerToClient>(4);

        hub.register(user, "s1", tx1);
        hub.register(user, "s2", tx2);

        hub.send_to(
            user,
            pb::ServerToClient {
                payload: Some(pb::server_to_client::Payload::ServerHint(
                    pb::ServerHint::default(),
                )),
                ..Default::default()
            },
        )
        .await;

        assert!(rx1.recv().await.is_some());
        assert!(rx2.recv().await.is_some());

        hub.unregister(user, "s1");
        hub.unregister(user, "s2");
    }

    struct TestDatagramTx;

    #[async_trait::async_trait]
    impl DatagramTx for TestDatagramTx {
        async fn send(&self, _bytes: Bytes) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn session_map_tracks_multiple_sessions_per_user() {
        let sessions = SessionMap::new();
        let user = UserId(uuid::Uuid::new_v4());

        let tx1: Arc<dyn DatagramTx> = Arc::new(TestDatagramTx);
        let tx2: Arc<dyn DatagramTx> = Arc::new(TestDatagramTx);
        sessions.register(user, "s1", tx1.clone());
        sessions.register(user, "s2", tx2.clone());

        let found = sessions.get_sessions(user).await;
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|(_, tx)| Arc::ptr_eq(tx, &tx1)));
        assert!(found.iter().any(|(_, tx)| Arc::ptr_eq(tx, &tx2)));

        sessions.unregister(user, "s1");
        let found = sessions.get_sessions(user).await;
        assert_eq!(found.len(), 1);
        assert!(Arc::ptr_eq(&found[0].1, &tx2));

        sessions.unregister(user, "s2");
        assert!(sessions.get_sessions(user).await.is_empty());
    }

    #[test]
    fn membership_cache_tracks_media_caps() {
        let membership = MembershipCache::new();
        let user = UserId(uuid::Uuid::new_v4());
        membership.set_media_capabilities(
            user,
            pb::ClientMediaCapabilities {
                decode: vec![pb::VideoCodec::Av1 as i32, pb::VideoCodec::Vp8 as i32],
                encode: vec![pb::VideoCodec::Vp9 as i32],
                ..Default::default()
            },
        );

        assert_eq!(
            membership.viewer_decode_codecs(user),
            vec![pb::VideoCodec::Av1, pb::VideoCodec::Vp8]
        );
        assert_eq!(
            membership.streamer_encode_codecs(user),
            vec![pb::VideoCodec::Vp9]
        );

        membership.remove_user(user);
        assert!(membership.media_capabilities(user).is_none());
    }

    #[test]
    fn membership_cache_updates_channel_members() {
        let membership = MembershipCache::new();
        let channel = ChannelId(uuid::Uuid::new_v4());
        let user = UserId(uuid::Uuid::new_v4());

        membership.set_channel(channel, 4, vec![]);
        membership.add_channel_member(channel, user);
        membership.add_channel_member(channel, user);

        let members = membership
            .members_of(channel)
            .expect("channel should exist in cache");
        assert_eq!(members, vec![user]);

        membership.remove_channel_member(channel, user);
        let members = membership
            .members_of(channel)
            .expect("channel should exist in cache");
        assert!(members.is_empty());
    }
}

pub type Sessions = SessionMap;
