use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::warn;

use crate::proto::voiceplatform::v1 as pb;

use vp_control::ids::{ChannelId, UserId};
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

pub fn channel_route_key(channel_id: ChannelId) -> u32 {
    vp_route_hash::channel_route_hash(channel_id.0)
}

#[derive(Clone)]
pub struct QuinnDatagramTx {
    conn: quinn::Connection,
}

impl QuinnDatagramTx {
    pub fn new(conn: quinn::Connection) -> Self {
        Self { conn }
    }
}

#[async_trait::async_trait]
impl DatagramTx for QuinnDatagramTx {
    async fn send(&self, bytes: Bytes) -> Result<()> {
        if let Err(e) = self.conn.send_datagram(bytes) {
            warn!(error = ?e, "failed to forward datagram");
            return Err(e.into());
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
}

impl MembershipCache {
    pub fn new() -> Self {
        Self {
            users: Arc::new(DashMap::new()),
            channels: Arc::new(DashMap::new()),
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
