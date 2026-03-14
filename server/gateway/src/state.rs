use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::proto::voiceplatform::v1 as pb;

use vp_control::ids::{ChannelId, UserId};
use vp_media::datagram_send_policy::SessionSendCtx;
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
pub struct SessionMap {
    inner: Arc<DashMap<(UserId, String), Arc<SessionSendCtx>>>,
    user_index: Arc<DashMap<UserId, HashSet<String>>>,
}

impl SessionMap {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
            user_index: Arc::new(DashMap::new()),
        }
    }

    pub fn register(&self, user: UserId, session_id: &str, tx: Arc<SessionSendCtx>) {
        let session_id = session_id.to_string();
        self.inner.insert((user, session_id.clone()), tx);
        self.user_index.entry(user).or_default().insert(session_id);
    }

    pub fn unregister(&self, user: UserId, session_id: &str) {
        self.inner.remove(&(user, session_id.to_string()));
        self.remove_from_user_index(user, session_id);
    }

    pub fn unregister_by_session_id(&self, session_id: &str) {
        let keys = self
            .inner
            .iter()
            .filter(|entry| entry.key().1 == session_id)
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>();
        for key in keys {
            self.inner.remove(&key);
            self.remove_from_user_index(key.0, &key.1);
        }
    }

    fn remove_from_user_index(&self, user: UserId, session_id: &str) {
        if let Some(mut sessions) = self.user_index.get_mut(&user) {
            sessions.remove(session_id);
            if sessions.is_empty() {
                drop(sessions);
                self.user_index.remove(&user);
            }
        }
    }

    pub fn has_user_sessions(&self, user: UserId) -> bool {
        self.user_index
            .get(&user)
            .map(|sessions| !sessions.is_empty())
            .unwrap_or(false)
    }

    pub fn pending_sessions(&self, max: usize) -> Vec<(UserId, String, Arc<SessionSendCtx>)> {
        self.inner
            .iter()
            .filter(|entry| {
                entry
                    .value()
                    .prune
                    .pending
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
            .take(max)
            .map(|entry| (entry.key().0, entry.key().1.clone(), entry.value().clone()))
            .collect()
    }

    pub fn has_pending(&self) -> bool {
        self.inner.iter().any(|entry| {
            entry
                .value()
                .prune
                .pending
                .load(std::sync::atomic::Ordering::Relaxed)
        })
    }

    pub fn pending_count(&self) -> usize {
        self.inner
            .iter()
            .filter(|entry| {
                entry
                    .value()
                    .prune
                    .pending
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
            .count()
    }
}

#[async_trait::async_trait]
impl SessionRegistry for SessionMap {
    async fn get_sessions(&self, user: UserId) -> Vec<(String, Arc<dyn DatagramTx>)> {
        let Some(indexed_sessions) = self.user_index.get(&user) else {
            return Vec::new();
        };

        let session_ids = indexed_sessions.iter().cloned().collect::<Vec<_>>();
        drop(indexed_sessions);

        let mut sessions = Vec::with_capacity(session_ids.len());
        for session_id in session_ids {
            if let Some(entry) = self.inner.get(&(user, session_id.clone())) {
                sessions.push((session_id, entry.value().clone() as Arc<dyn DatagramTx>));
            }
        }
        sessions
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
    use super::{MembershipCache, PushHub, ShareMetadata, StreamSessionOwnership, StreamSessionRegistry};
    use crate::proto::voiceplatform::v1 as pb;
    use tokio::sync::mpsc;
    use tokio::time::Instant;
    use vp_control::ids::{ChannelId, UserId};

    fn test_metadata() -> ShareMetadata {
        ShareMetadata { codec: pb::VideoCodec::Vp9 as i32, layers: vec![], has_audio: false }
    }
    
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

    #[test]
    fn session_user_index_lifecycle_multi_session_and_reconnect() {
        let sessions = super::SessionMap::new();
        let user = UserId(uuid::Uuid::new_v4());
        let s1 = "s1".to_string();
        let s2 = "s2".to_string();

        sessions
            .user_index
            .entry(user)
            .or_default()
            .insert(s1.clone());
        sessions
            .user_index
            .entry(user)
            .or_default()
            .insert(s2.clone());
        // reconnect of the same session id should not duplicate entries
        sessions
            .user_index
            .entry(user)
            .or_default()
            .insert(s1.clone());

        let ids = sessions
            .user_index
            .get(&user)
            .map(|set| set.clone())
            .unwrap_or_default();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("s1"));
        assert!(ids.contains("s2"));

        sessions.remove_from_user_index(user, "s1");
        let ids = sessions
            .user_index
            .get(&user)
            .map(|set| set.clone())
            .unwrap_or_default();
        assert_eq!(ids.len(), 1);
        assert!(ids.contains("s2"));

        sessions.remove_from_user_index(user, "s2");
        assert!(sessions.user_index.get(&user).is_none());
    }

    #[test]
    fn stream_registry_stop_share_removes_all_tags() {
        let mut registry = StreamSessionRegistry::new();
        let owner = UserId::new();
        let channel = ChannelId::new();
        registry.register(
            "stream-a".to_string(),
            StreamSessionOwnership {
                primary_tag: 10,
                fallback_tag: Some(11),
                owner_user_id: owner,
                channel_id: channel,
                active_layer_ids: vec![1],
                metadata: test_metadata(),
            },
        );

        let teardown = registry.teardown("stream-a");
        assert_eq!(teardown.removed_tags, vec![10, 11]);
        assert!(registry.ownership_by_stream_tag(10).is_none());
        assert!(registry.ownership_by_stream_tag(11).is_none());
        assert_eq!(registry.active_sessions(), 0);
        assert_eq!(registry.active_stream_tags(), 0);
    }

    #[test]
    fn stream_registry_recovery_for_fallback_tag_reaches_owner() {
        let mut registry = StreamSessionRegistry::new();
        let owner = UserId::new();
        let channel = ChannelId::new();
        registry.register(
            "stream-b".to_string(),
            StreamSessionOwnership {
                primary_tag: 100,
                fallback_tag: Some(101),
                owner_user_id: owner,
                channel_id: channel,
                active_layer_ids: vec![1],
                metadata: test_metadata(),
            },
        );

        let (_, ownership) = registry
            .ownership_by_stream_tag(101)
            .expect("fallback should map to ownership");
        assert_eq!(ownership.owner_user_id, owner);
        assert!(registry.should_forward_recovery(101, Instant::now()));
    }

    #[test]
    fn stream_registry_repeated_start_stop_leaves_no_orphan_tag() {
        let mut registry = StreamSessionRegistry::new();
        let owner = UserId::new();
        let channel = ChannelId::new();
        for idx in 0..5 {
            let stream_id = format!("stream-{idx}");
            registry.register(
                stream_id.clone(),
                StreamSessionOwnership {
                    primary_tag: 200 + idx,
                    fallback_tag: Some(300 + idx),
                    owner_user_id: owner,
                    channel_id: channel,
                    active_layer_ids: vec![1],
                    metadata: test_metadata(),
                },
            );
            registry.teardown(&stream_id);
        }

        assert_eq!(registry.active_sessions(), 0);
        assert_eq!(registry.active_stream_tags(), 0);
        assert_eq!(registry.orphan_cleanup_count(), 0);
    }

    #[test]
    fn active_sessions_for_channel_returns_only_matching_channel() {
        let mut registry = StreamSessionRegistry::new();
        let owner = UserId::new();
        let ch_a = ChannelId::new();
        let ch_b = ChannelId::new();
 
        registry.register(
            "s-a".to_string(),
            StreamSessionOwnership {
                primary_tag: 1,
                fallback_tag: None,
                owner_user_id: owner,
                channel_id: ch_a,
                active_layer_ids: vec![0],
                metadata: test_metadata(),
            },
        );
        registry.register(
            "s-b".to_string(),
            StreamSessionOwnership {
                primary_tag: 2,
                fallback_tag: None,
                owner_user_id: owner,
                channel_id: ch_b,
                active_layer_ids: vec![0],
                metadata: test_metadata(),
            },
        );
 
        let for_a = registry.active_sessions_for_channel(ch_a);
        assert_eq!(for_a.len(), 1);
        assert_eq!(for_a[0].0, "s-a");
 
        let for_b = registry.active_sessions_for_channel(ch_b);
        assert_eq!(for_b.len(), 1);
        assert_eq!(for_b[0].0, "s-b");
 
        assert!(registry.active_sessions_for_channel(ChannelId::new()).is_empty());
    }
}

pub type Sessions = SessionMap;

/// Metadata stored at share-start time and used to reconstruct `ScreenShareStarted`
/// events for late-joining or reconnecting viewers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareMetadata {
    /// Primary codec negotiated for this stream.
    pub codec: i32,
    /// Simulcast layers offered by the sender (width/height/fps/bitrate).
    pub layers: Vec<crate::proto::voiceplatform::v1::SimulcastLayer>,
    /// Whether the share includes system audio.
    pub has_audio: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSessionOwnership {
    pub primary_tag: u64,
    pub fallback_tag: Option<u64>,
    pub owner_user_id: UserId,
    pub channel_id: ChannelId,
    pub active_layer_ids: Vec<u8>,
    /// Rich start-time metadata for lifecycle event replay.
    pub metadata: ShareMetadata,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamTeardown {
    pub stream_id: String,
    pub removed_tags: Vec<u64>,
    pub removed_orphan_tags: usize,
}

#[derive(Clone, Debug, Default)]
pub struct StreamSessionRegistry {
    sessions_by_stream_id: HashMap<String, StreamSessionOwnership>,
    stream_id_by_tag: HashMap<u64, String>,
    viewer_preferred_layers: HashMap<(String, UserId), u8>,
    recovery_forwarded_at: HashMap<u64, Instant>,
    recovery_forwards: u64,
    keyframe_requests: u64,
    orphan_cleanup_count: u64,
}

impl StreamSessionRegistry {
    const RECOVERY_THROTTLE: Duration = Duration::from_millis(500);

    /// Returns all active sessions in a given channel as `(stream_id, ownership)` pairs.
    pub fn active_sessions_for_channel(
        &self,
        channel_id: ChannelId,
    ) -> Vec<(String, &StreamSessionOwnership)> {
        self.sessions_by_stream_id
            .iter()
            .filter(|(_, o)| o.channel_id == channel_id)
            .map(|(sid, o)| (sid.clone(), o))
            .collect()
    }
 
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        stream_id: String,
        ownership: StreamSessionOwnership,
    ) -> StreamTeardown {
        let mut teardown = self.teardown(&stream_id);
        teardown.stream_id = stream_id.clone();

        self.stream_id_by_tag
            .insert(ownership.primary_tag, stream_id.clone());
        if let Some(tag) = ownership.fallback_tag {
            self.stream_id_by_tag.insert(tag, stream_id.clone());
        }
        self.sessions_by_stream_id.insert(stream_id, ownership);
        teardown
    }

    pub fn teardown(&mut self, stream_id: &str) -> StreamTeardown {
        let mut removed_tags = Vec::new();
        if let Some(existing) = self.sessions_by_stream_id.remove(stream_id) {
            removed_tags.push(existing.primary_tag);
            if let Some(tag) = existing.fallback_tag {
                removed_tags.push(tag);
            }
        }

        let mut removed_orphan_tags = 0usize;
        for tag in &removed_tags {
            if self.stream_id_by_tag.remove(tag).is_none() {
                removed_orphan_tags += 1;
            }
            self.recovery_forwarded_at.remove(tag);
        }
        self.orphan_cleanup_count += removed_orphan_tags as u64;
        self.viewer_preferred_layers
            .retain(|(sid, _), _| sid != stream_id);

        StreamTeardown {
            stream_id: stream_id.to_string(),
            removed_tags,
            removed_orphan_tags,
        }
    }

    pub fn ownership_by_stream_tag(
        &self,
        stream_tag: u64,
    ) -> Option<(&str, &StreamSessionOwnership)> {
        let stream_id = self.stream_id_by_tag.get(&stream_tag)?;
        self.sessions_by_stream_id
            .get(stream_id)
            .map(|ownership| (stream_id.as_str(), ownership))
    }

    pub fn ownership_by_stream_id(&self, stream_id: &str) -> Option<&StreamSessionOwnership> {
        self.sessions_by_stream_id.get(stream_id)
    }

    pub fn set_viewer_preferred_layer(&mut self, stream_id: &str, viewer: UserId, layer_id: u8) {
        self.viewer_preferred_layers
            .insert((stream_id.to_string(), viewer), layer_id);
    }

    pub fn viewer_preferred_layer(&self, stream_id: &str, viewer: UserId) -> Option<u8> {
        self.viewer_preferred_layers
            .get(&(stream_id.to_string(), viewer))
            .copied()
    }
    pub fn primary_tag_for_stream_id(&self, stream_id: &str) -> Option<u64> {
        self.sessions_by_stream_id
            .get(stream_id)
            .map(|s| s.primary_tag)
    }

    pub fn note_keyframe_request(&mut self) {
        self.keyframe_requests = self.keyframe_requests.saturating_add(1);
    }

    pub fn should_forward_recovery(&mut self, stream_tag: u64, now: Instant) -> bool {
        let should_forward = self
            .recovery_forwarded_at
            .get(&stream_tag)
            .map(|at| now.duration_since(*at) >= Self::RECOVERY_THROTTLE)
            .unwrap_or(true);
        if should_forward {
            self.recovery_forwarded_at.insert(stream_tag, now);
            self.recovery_forwards = self.recovery_forwards.saturating_add(1);
        }
        should_forward
    }

    pub fn active_sessions(&self) -> usize {
        self.sessions_by_stream_id.len()
    }

    pub fn active_stream_tags(&self) -> usize {
        self.stream_id_by_tag.len()
    }

    pub fn recovery_forwards(&self) -> u64 {
        self.recovery_forwards
    }

    pub fn keyframe_requests(&self) -> u64 {
        self.keyframe_requests
    }

    pub fn orphan_cleanup_count(&self) -> u64 {
        self.orphan_cleanup_count
    }
}
