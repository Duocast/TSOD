//! Production-grade voice forwarding for QUIC DATAGRAM media.
//!
//! Responsibilities:
//! - Parse and validate incoming voice datagrams.
//! - Enforce per-session and per-channel policy (membership, mute, talker gating).
//! - Forward datagrams to other channel members with bounded buffering.
//!
//! Design notes:
//! - Voice packets are treated as opaque payload after header validation.
//! - Forwarder does NOT decode Opus and does not do server-side mixing.
//! - Backpressure: if a receiver queue is full, drop and count.
//!
//! Packet format (minimal, fixed header; not protobuf):
//!   0:  u8  version            (currently 1)
//!   1:  u8  flags              (bit0=vad, bit1=fec, others reserved)
//!   2:  u16 header_len_bytes   (network order) (currently 20)
//!   4:  u32 channel_id_hash    (fast route key; collision-safe check via control lookup)
//!   8:  u32 ssrc               (sender stream id)
//!   12: u32 seq                (monotonic per sender ssrc)
//!   16: u32 ts_ms              (sender timestamp milliseconds mod 2^32)
//!   20: ... payload bytes      (Opus frame)
//!
//! Notes:
//! - We use a 32-bit hash for routing speed; we still validate membership against authoritative IDs.
//! - Channel ID itself is not embedded to keep header small; you can embed full UUID if desired.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, warn};

/// ---- Project-facing types you should adapt ----
/// Use your real IDs (UUID/ULID). Keep them `Copy`/cheap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UserId(pub uuid::Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChannelId(pub uuid::Uuid);

/// Sender/receiver datagram output handle. This should send a datagram to a QUIC Connection.
#[async_trait::async_trait]
pub trait DatagramTx: Send + Sync {
    async fn send(&self, bytes: Bytes) -> Result<()>;
}

/// Maps active users to their datagram sender handle (session registry).
#[async_trait::async_trait]
pub trait SessionRegistry: Send + Sync {
    async fn get_datagram_tx(&self, user: UserId) -> Option<Arc<dyn DatagramTx>>;
}

/// Authoritative membership & moderation state (backed by your control plane).
#[async_trait::async_trait]
pub trait MembershipProvider: Send + Sync {
    /// Resolve the authoritative ChannelId for an incoming packet.
    /// Given (sender, route_key/hash) return (channel_id) if sender is in some channel matching this key.
    async fn resolve_channel_for_sender(&self, sender: UserId, route_key: u32) -> Option<ChannelId>;

    /// Return the channel members (including sender). Fast path should be cached in memory at your fanout layer.
    async fn list_members(&self, channel: ChannelId) -> Vec<UserId>;

    /// True if sender is muted (server-side) in this channel.
    async fn is_muted(&self, channel: ChannelId, sender: UserId) -> bool;

    /// Channel policy: max concurrent talkers.
    async fn max_talkers(&self, channel: ChannelId) -> usize;
}

/// Metrics hook (optional). Implement with Prometheus/OpenTelemetry.
pub trait VoiceMetrics: Send + Sync {
    fn inc_rx_packets(&self);
    fn inc_rx_bytes(&self, n: usize);
    fn inc_drop_invalid(&self);
    fn inc_drop_rate_limited(&self);
    fn inc_drop_not_member(&self);
    fn inc_drop_muted(&self);
    fn inc_drop_talker_limit(&self);
    fn inc_drop_send_queue_full(&self);
    fn inc_forwarded(&self, fanout: usize);
}

/// No-op metrics default.
pub struct NoopMetrics;
impl VoiceMetrics for NoopMetrics {
    fn inc_rx_packets(&self) {}
    fn inc_rx_bytes(&self, _n: usize) {}
    fn inc_drop_invalid(&self) {}
    fn inc_drop_rate_limited(&self) {}
    fn inc_drop_not_member(&self) {}
    fn inc_drop_muted(&self) {}
    fn inc_drop_talker_limit(&self) {}
    fn inc_drop_send_queue_full(&self) {}
    fn inc_forwarded(&self, _fanout: usize) {}
}

/// Configuration knobs.
#[derive(Clone, Debug)]
pub struct VoiceForwarderConfig {
    /// Maximum size of an incoming voice datagram.
    pub max_datagram_bytes: usize,
    /// Minimum size (header).
    pub min_datagram_bytes: usize,
    /// Per-sender packets per second soft limit.
    pub sender_pps_limit: u32,
    /// Per-sender bytes per second soft limit.
    pub sender_bps_limit: u32,
    /// Bounded per-receiver queue depth (datagrams).
    pub per_receiver_queue: usize,
    /// How long a sender is considered an "active talker" since last packet.
    pub talker_activity_window: Duration,
    /// Acceptable clock skew of sender timestamp (ms).
    pub max_ts_skew_ms: u32,
    /// Whether to require VAD flag to count as talker (reduces false talker occupancy).
    pub vad_required_for_talker: bool,
}

impl Default for VoiceForwarderConfig {
    fn default() -> Self {
        Self {
            max_datagram_bytes: 1500,
            min_datagram_bytes: 20,
            sender_pps_limit: 200,        // plenty for 5–20ms frames
            sender_bps_limit: 512 * 1024, // 512kbps per sender max
            per_receiver_queue: 256,
            talker_activity_window: Duration::from_millis(800),
            max_ts_skew_ms: 2_000,
            vad_required_for_talker: false,
        }
    }
}

/// Voice forwarder main type.
pub struct VoiceForwarder {
    cfg: VoiceForwarderConfig,
    sessions: Arc<dyn SessionRegistry>,
    membership: Arc<dyn MembershipProvider>,
    metrics: Arc<dyn VoiceMetrics>,

    /// Receiver send loops keyed by user id.
    send_loops: RwLock<HashMap<UserId, mpsc::Sender<Bytes>>>,

    /// Talker state per channel.
    talkers: RwLock<HashMap<ChannelId, TalkerSet>>,

    /// Per-sender rate limit state.
    rate: RwLock<HashMap<UserId, RateState>>,
}

impl VoiceForwarder {
    pub fn new(
        cfg: VoiceForwarderConfig,
        sessions: Arc<dyn SessionRegistry>,
        membership: Arc<dyn MembershipProvider>,
        metrics: Arc<dyn VoiceMetrics>,
    ) -> Self {
        Self {
            cfg,
            sessions,
            membership,
            metrics,
            send_loops: RwLock::new(HashMap::new()),
            talkers: RwLock::new(HashMap::new()),
            rate: RwLock::new(HashMap::new()),
        }
    }

    /// Handle an incoming datagram from a specific authenticated user.
    /// The gateway should call this after mapping conn -> user_id.
    pub async fn handle_incoming(&self, sender: UserId, datagram: Bytes) {
        self.metrics.inc_rx_packets();
        self.metrics.inc_rx_bytes(datagram.len());

        // Basic length check
        if datagram.l
