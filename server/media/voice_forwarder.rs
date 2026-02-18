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

use vp_control::ids::{ChannelId, UserId};

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
        if datagram.len() < self.cfg.min_datagram_bytes || datagram.len() > self.cfg.max_datagram_bytes {
            self.metrics.inc_drop_invalid();
            return;
        }

        let parsed = match VoicePacket::parse(&datagram) {
            Ok(p) => p,
            Err(_) => {
                self.metrics.inc_drop_invalid();
                return;
            }
        };

        // Timestamp sanity (prevents weirdness, not security)
        if !parsed.ts_sane(self.cfg.max_ts_skew_ms) {
            self.metrics.inc_drop_invalid();
            return;
        }

        // Per-sender rate limiting
        if !self.allow_rate(sender, datagram.len() as u32).await {
            self.metrics.inc_drop_rate_limited();
            return;
        }

        // Resolve authoritative channel based on route key + membership
        let channel = match self.membership.resolve_channel_for_sender(sender, parsed.channel_route).await {
            Some(c) => c,
            None => {
                self.metrics.inc_drop_not_member();
                return;
            }
        };

        // Moderation gate
        if self.membership.is_muted(channel, sender).await {
            self.metrics.inc_drop_muted();
            return;
        }

        // Talker gating: limit concurrent talkers per channel
        let vad_ok = !self.cfg.vad_required_for_talker || parsed.vad;
        if vad_ok {
            if !self.allow_talker(channel, sender).await {
                self.metrics.inc_drop_talker_limit();
                return;
            }
        }

        // Fanout: list channel members and forward to everyone except sender
        let members = self.membership.list_members(channel).await;
        let mut forwarded = 0usize;

        for uid in members {
            if uid == sender {
                continue;
            }

            if self.enqueue_to_receiver(uid, datagram.clone()).await {
                forwarded += 1;
            } else {
                self.metrics.inc_drop_send_queue_full();
            }
        }

        self.metrics.inc_forwarded(forwarded);
    }

    /// Ensure a per-receiver send loop exists and enqueue datagram.
    async fn enqueue_to_receiver(&self, receiver: UserId, datagram: Bytes) -> bool {
        // Fast path: sender exists
        if let Some(tx) = self.send_loops.read().await.get(&receiver).cloned() {
            return tx.try_send(datagram).is_ok();
        }

        // Slow path: create send loop if session exists
        let Some(dtx) = self.sessions.get_datagram_tx(receiver).await else {
            return false;
        };

        let (tx, mut rx) = mpsc::channel::<Bytes>(self.cfg.per_receiver_queue);

        // Insert before spawn to avoid races
        self.send_loops.write().await.insert(receiver, tx.clone());

        tokio::spawn(async move {
            // Simple forward loop: if send fails, exit (session likely gone).
            while let Some(pkt) = rx.recv().await {
                if let Err(e) = dtx.send(pkt).await {
                    debug!("receiver send loop ended: {}", e);
                    break;
                }
            }
        });

        tx.try_send(datagram).is_ok()
    }

    /// Token bucket-ish per sender (pps + bps).
    async fn allow_rate(&self, sender: UserId, bytes: u32) -> bool {
        let mut map = self.rate.write().await;
        let st = map.entry(sender).or_insert_with(RateState::new);

        st.refill();

        if st.tokens_pkts == 0 || st.tokens_bytes < bytes {
            return false;
        }
        st.tokens_pkts -= 1;
        st.tokens_bytes -= bytes;
        true
    }

    /// Track active talkers in window; deny if too many.
    async fn allow_talker(&self, channel: ChannelId, sender: UserId) -> bool {
        let max = self.membership.max_talkers(channel).await.max(1);

        let mut map = self.talkers.write().await;
        let set = map.entry(channel).or_insert_with(|| TalkerSet::new(self.cfg.talker_activity_window));

        set.prune();

        if set.is_active(sender) {
            set.touch(sender);
            return true;
        }

        if set.active_count() >= max {
            return false;
        }

        set.touch(sender);
        true
    }
}

/// Parsed voice packet header view.
#[derive(Clone, Copy, Debug)]
struct VoicePacket {
    version: u8,
    flags: u8,
    header_len: u16,
    channel_route: u32,
    _ssrc: u32,
    _seq: u32,
    ts_ms: u32,
    vad: bool,
}

impl VoicePacket {
    fn parse(b: &Bytes) -> Result<Self> {
        if b.len() < 20 {
            return Err(anyhow!("short"));
        }
        let version = b[0];
        if version != 1 {
            return Err(anyhow!("bad version"));
        }
        let flags = b[1];
        let header_len = u16::from_be_bytes([b[2], b[3]]);
        if header_len != 20 {
            return Err(anyhow!("bad header len"));
        }

        let channel_route = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
        let ssrc = u32::from_be_bytes([b[8], b[9], b[10], b[11]]);
        let seq = u32::from_be_bytes([b[12], b[13], b[14], b[15]]);
        let ts_ms = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);

        let vad = (flags & 0x01) != 0;
        Ok(Self {
            version,
            flags,
            header_len,
            channel_route,
            _ssrc: ssrc,
            _seq: seq,
            ts_ms,
            vad,
        })
    }

    fn ts_sane(&self, max_skew_ms: u32) -> bool {
        // We don't have sender clock sync; this just rejects wildly bogus ts (0, huge jumps).
        // For production you can do per-sender monotonic checks in RateState.
        let now = (unix_ms() & 0xFFFF_FFFF) as u32;
        let diff = now.wrapping_sub(self.ts_ms);
        diff <= max_skew_ms || (u32::MAX - diff) <= max_skew_ms
    }
}

/// Per-sender limiter state.
struct RateState {
    last: Instant,
    tokens_pkts: u32,
    tokens_bytes: u32,

    // For optional monotonic timestamp checks.
    last_ts_ms: Option<u32>,
}

impl RateState {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            tokens_pkts: 0,
            tokens_bytes: 0,
            last_ts_ms: None,
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last);
        if elapsed < Duration::from_millis(10) {
            return;
        }

        // Refill proportionally (simple token bucket)
        let secs = elapsed.as_secs_f32();
        let add_pkts = (secs * 200.0) as u32; // will be overwritten by cfg in integration if desired
        let add_bytes = (secs * 512_000.0) as u32;

        // Cap burst to 1 second worth
        self.tokens_pkts = (self.tokens_pkts + add_pkts).min(200);
        self.tokens_bytes = (self.tokens_bytes + add_bytes).min(512_000);
        self.last = now;
    }

    #[allow(dead_code)]
    fn check_monotonic_ts(&mut self, ts: u32) -> bool {
        if let Some(prev) = self.last_ts_ms {
            // allow wrap; reject huge backwards jumps
            let diff = ts.wrapping_sub(prev);
            if diff > 10_000 && diff < (u32::MAX - 10_000) {
                return false;
            }
        }
        self.last_ts_ms = Some(ts);
        true
    }
}

/// Active talker tracking with time window.
struct TalkerSet {
    window: Duration,
    // user -> last_seen
    last_seen: HashMap<UserId, Instant>,
    // pruning queue
    order: VecDeque<(UserId, Instant)>,
}

impl TalkerSet {
    fn new(window: Duration) -> Self {
        Self { window, last_seen: HashMap::new(), order: VecDeque::new() }
    }

    fn touch(&mut self, user: UserId) {
        let now = Instant::now();
        self.last_seen.insert(user, now);
        self.order.push_back((user, now));
    }

    fn is_active(&self, user: UserId) -> bool {
        self.last_seen.get(&user).map(|t| t.elapsed() <= self.window).unwrap_or(false)
    }

    fn active_count(&self) -> usize {
        self.last_seen.values().filter(|t| t.elapsed() <= self.window).count()
    }

    fn prune(&mut self) {
        let now = Instant::now();
        while let Some((u, t)) = self.order.front().cloned() {
            if now.duration_since(t) <= self.window {
                break;
            }
            self.order.pop_front();
            // Only remove if it wasn't touched again later.
            if let Some(cur) = self.last_seen.get(&u).copied() {
                if cur == t && now.duration_since(cur) > self.window {
                    self.last_seen.remove(&u);
                }
            }
        }
    }
}

fn unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
