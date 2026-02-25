use std::{
    collections::{hash_map::DefaultHasher, HashSet},
    hash::{Hash, Hasher},
    sync::Arc,
};

use anyhow::Result;
use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::mpsc;

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

    pub async fn broadcast(&self, users: &[UserId], msg: &pb::ServerToClient) {
        for u in users {
            self.send_to(*u, msg.clone()).await;
        }
    }

    pub fn connected_users(&self) -> Vec<UserId> {
        self.inner.iter().map(|entry| *entry.key()).collect()
    }
}

pub fn channel_route_key(channel_id: ChannelId) -> u32 {
    let mut h = DefaultHasher::new();
    channel_id.0.hash(&mut h);
    (h.finish() & 0xFFFF_FFFF) as u32
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
        self.conn.send_datagram(bytes)?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct SessionMap {
    inner: Arc<DashMap<UserId, Arc<dyn DatagramTx>>>,
}

impl SessionMap {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    pub fn register(&self, user: UserId, tx: Arc<dyn DatagramTx>) {
        self.inner.insert(user, tx);
    }

    pub fn unregister(&self, user: UserId) {
        self.inner.remove(&user);
    }
}

#[async_trait::async_trait]
impl SessionRegistry for SessionMap {
    async fn get_datagram_tx(&self, user: UserId) -> Option<Arc<dyn DatagramTx>> {
        self.inner.get(&user).map(|e| e.value().clone())
    }
}

#[derive(Clone, Debug)]
struct UserPresence {
    channel: ChannelId,
    route: u32,
    muted: bool,
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

    pub fn set_user(&self, user: UserId, channel: ChannelId, muted: bool) {
        self.users.insert(
            user,
            UserPresence {
                channel,
                route: channel_route_key(channel),
                muted,
            },
        );
    }

    pub fn remove_user(&self, user: UserId) {
        self.users.remove(&user);
    }

    pub fn update_mute(&self, user: UserId, channel: ChannelId, muted: bool) {
        self.users.insert(
            user,
            UserPresence {
                channel,
                route: channel_route_key(channel),
                muted,
            },
        );
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

    async fn max_talkers(&self, channel: ChannelId) -> usize {
        self.channels
            .get(&channel)
            .map(|e| e.max_talkers)
            .unwrap_or(4)
    }
}

#[cfg(test)]
mod tests {
    use super::PushHub;
    use crate::proto::voiceplatform::v1 as pb;
    use tokio::sync::mpsc;
    use vp_control::ids::UserId;

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
}

pub type Sessions = SessionMap;
