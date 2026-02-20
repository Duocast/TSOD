use std::{
    collections::hash_map::DefaultHasher,
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
    inner: Arc<DashMap<UserId, mpsc::Sender<pb::ServerToClient>>>,
}

impl PushHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    pub fn register(&self, user: UserId, tx: mpsc::Sender<pb::ServerToClient>) {
        self.inner.insert(user, tx);
    }

    pub fn unregister(&self, user: UserId) {
        self.inner.remove(&user);
    }

    pub async fn send_to(&self, user: UserId, msg: pb::ServerToClient) {
        if let Some(entry) = self.inner.get(&user) {
            let _ = entry.send(msg).await;
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
        }}
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

pub type Sessions = SessionMap;
