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
//!   20: [server forwarded only] 16-byte sender_user_id UUID
//!   36: [server forwarded only] 16-byte channel_id UUID
//!   52: ... payload bytes      (Opus frame)
//!
//! Notes:
//! - We use a 32-bit hash for routing speed; we still validate membership against authoritative IDs.
//! - Client->server packets use the minimal 20-byte header.
//! - Server->client packets are stamped with sender/channel UUID metadata for attribution.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use bytes::{BufMut, Bytes, BytesMut};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, warn};

/// ---- Project-facing types ----
use vp_control::ids::{ChannelId, UserId};

/// Sender/receiver datagram output handle. This should send a datagram to a QUIC Connection.
#[async_trait::async_trait]
pub trait DatagramTx: Send + Sync {
    async fn send(&self, bytes: Bytes) -> Result<()>;
}

/// Maps active users to their datagram sender handle (session registry).
#[async_trait::async_trait]
pub trait SessionRegistry: Send + Sync {
    async fn get_sessions(&self, user: UserId) -> Vec<(String, Arc<dyn DatagramTx>)>;
}

/// Authoritative membership & moderation state (backed by your control plane).
#[async_trait::async_trait]
pub trait MembershipProvider: Send + Sync {
    /// Resolve the authoritative ChannelId for an incoming packet.
    /// Given (sender, route_key/hash) return (channel_id) if sender is in some channel matching this key.
    async fn resolve_channel_for_sender(&self, sender: UserId, route_key: u32)
        -> Option<ChannelId>;

    /// Return the channel members (including sender). Fast path should be cached in memory at your fanout layer.
    async fn list_members(&self, channel: ChannelId) -> Vec<UserId>;

    /// True if sender is muted (server-side) in this channel.
    async fn is_muted(&self, channel: ChannelId, sender: UserId) -> bool;

    /// True if user is deafened (server-side) in this channel.
    async fn is_deafened(&self, channel: ChannelId, user: UserId) -> bool;

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
    /// Whether to require VAD flag to count as talker (reduces false talker occupancy).
    pub vad_required_for_talker: bool,
}

impl Default for VoiceForwarderConfig {
    fn default() -> Self {
        Self {
            max_datagram_bytes: vp_voice::MAX_INBOUND_VOICE_DATAGRAM_BYTES,
            min_datagram_bytes: vp_voice::CLIENT_VOICE_HEADER_BYTES,
            sender_pps_limit: 200,        // plenty for 5–20ms frames
            sender_bps_limit: 512 * 1024, // 512kbps per sender max
            per_receiver_queue: 256,
            talker_activity_window: Duration::from_millis(800),
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

    /// Receiver send loops keyed by user+session id.
    send_loops: RwLock<HashMap<(UserId, String), mpsc::Sender<Bytes>>>,

    /// Talker state per channel.
    talkers: RwLock<HashMap<ChannelId, TalkerSet>>,

    /// Per-sender/stream rate limit state.
    rate: RwLock<HashMap<(UserId, u32), RateState>>,
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
        if datagram.len() < self.cfg.min_datagram_bytes
            || datagram.len() > self.cfg.max_datagram_bytes
        {
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

        // Per-sender rate limiting
        if !self
            .allow_rate(sender, parsed.ssrc, datagram.len() as u32, parsed.ts_ms)
            .await
        {
            self.metrics.inc_drop_rate_limited();
            return;
        }

        // Resolve authoritative channel based on route key + membership
        let channel = match self
            .membership
            .resolve_channel_for_sender(sender, parsed.channel_route)
            .await
        {
            Some(c) => c,
            None => {
                self.metrics.inc_drop_not_member();
                return;
            }
        };

        // Moderation gate
        if self.membership.is_muted(channel, sender).await
            || self.membership.is_deafened(channel, sender).await
        {
            self.metrics.inc_drop_muted();
            return;
        }

        // Talker gating: limit concurrent talkers per channel
        let vad_ok = !self.cfg.vad_required_for_talker || parsed.vad;
        if vad_ok && !self.allow_talker(channel, sender).await {
            self.metrics.inc_drop_talker_limit();
            return;
        }

        // Fanout: list channel members and forward to everyone except sender.
        let members = self.membership.list_members(channel).await;
        let outbound = stamp_sender_metadata(&parsed, sender, channel, &datagram);
        let mut forwarded = 0usize;
        let mut send_failures = 0usize;

        for uid in members {
            if uid == sender || self.membership.is_deafened(channel, uid).await {
                continue;
            }

            let (ok, failures) = self.enqueue_to_receiver(uid, outbound.clone()).await;
            forwarded += ok;
            send_failures += failures;
            for _ in 0..failures {
                self.metrics.inc_drop_send_queue_full();
            }
        }

        if send_failures > 0 {
            log_forward_failures(send_failures);
        }

        self.metrics.inc_forwarded(forwarded);
    }

    /// Ensure per-session send loops exist and enqueue datagram to all active sessions.
    /// Returns (success_count, failure_count).
    async fn enqueue_to_receiver(&self, receiver: UserId, datagram: Bytes) -> (usize, usize) {
        let mut ok = 0usize;
        let mut failures = 0usize;

        for (session_id, dtx) in self.sessions.get_sessions(receiver).await {
            if self
                .enqueue_to_receiver_session(receiver, session_id, dtx, datagram.clone())
                .await
            {
                ok += 1;
            } else {
                failures += 1;
            }
        }

        (ok, failures)
    }

    async fn enqueue_to_receiver_session(
        &self,
        receiver: UserId,
        session_id: String,
        dtx: Arc<dyn DatagramTx>,
        datagram: Bytes,
    ) -> bool {
        let key = (receiver, session_id);
        if let Some(tx) = self.send_loops.read().await.get(&key).cloned() {
            match tx.try_send(datagram) {
                Ok(()) => return true,
                Err(mpsc::error::TrySendError::Full(_)) => return false,
                Err(mpsc::error::TrySendError::Closed(datagram)) => {
                    self.send_loops.write().await.remove(&key);
                    return self.enqueue_slow_path(key, dtx, datagram).await;
                }
            }
        }

        self.enqueue_slow_path(key, dtx, datagram).await
    }

    async fn enqueue_slow_path(
        &self,
        key: (UserId, String),
        dtx: Arc<dyn DatagramTx>,
        datagram: Bytes,
    ) -> bool {
        let (tx, mut rx) = mpsc::channel::<Bytes>(self.cfg.per_receiver_queue);

        self.send_loops.write().await.insert(key, tx.clone());

        tokio::spawn(async move {
            while let Some(pkt) = rx.recv().await {
                if let Err(e) = dtx.send(pkt).await {
                    debug!(error = %e, "receiver session send loop ended");
                    break;
                }
            }
        });

        tx.try_send(datagram).is_ok()
    }

    pub async fn unregister_session(&self, user: UserId, session_id: &str) {
        self.send_loops
            .write()
            .await
            .remove(&(user, session_id.to_string()));
    }

    /// Token bucket-ish per sender (pps + bps).
    async fn allow_rate(&self, sender: UserId, ssrc: u32, bytes: u32, ts_ms: u32) -> bool {
        self.allow_rate_at(sender, ssrc, bytes, ts_ms, Instant::now())
            .await
    }

    async fn allow_rate_at(
        &self,
        sender: UserId,
        ssrc: u32,
        bytes: u32,
        ts_ms: u32,
        now: Instant,
    ) -> bool {
        let mut map = self.rate.write().await;
        let st = map.entry((sender, ssrc)).or_insert_with(|| {
            RateState::new(self.cfg.sender_pps_limit, self.cfg.sender_bps_limit)
        });

        if !st.check_monotonic_ts(ts_ms, now) {
            return false;
        }

        st.refill(self.cfg.sender_pps_limit, self.cfg.sender_bps_limit, now);

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
        let set = map
            .entry(channel)
            .or_insert_with(|| TalkerSet::new(self.cfg.talker_activity_window));

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

const REFILL_QUANTUM: Duration = Duration::from_millis(10);
const STREAM_IDLE_RESET: Duration = Duration::from_secs(10);

const CLIENT_HEADER_LEN: usize = vp_voice::CLIENT_VOICE_HEADER_BYTES;
const FORWARDED_HEADER_LEN: usize = vp_voice::FORWARDED_VOICE_HEADER_BYTES;

fn stamp_sender_metadata(
    parsed: &VoicePacket,
    sender: UserId,
    channel: ChannelId,
    datagram: &Bytes,
) -> Bytes {
    let payload = &datagram[CLIENT_HEADER_LEN..];
    let mut out = BytesMut::with_capacity(FORWARDED_HEADER_LEN + payload.len());

    out.put_u8(1); // version
    out.put_u8(parsed.flags);
    out.put_u16(FORWARDED_HEADER_LEN as u16);
    out.put_u32(parsed.channel_route);
    out.put_u32(parsed.ssrc);
    out.put_u32(parsed.seq);
    out.put_u32(parsed.ts_ms);
    out.extend_from_slice(sender.0.as_bytes());
    out.extend_from_slice(channel.0.as_bytes());
    out.extend_from_slice(payload);

    out.freeze()
}

/// Parsed voice packet header view.
#[derive(Clone, Copy, Debug)]
struct VoicePacket {
    flags: u8,
    channel_route: u32,
    ssrc: u32,
    seq: u32,
    ts_ms: u32,
    vad: bool,
}

impl VoicePacket {
    fn parse(b: &Bytes) -> Result<Self> {
        if b.len() < vp_voice::CLIENT_VOICE_HEADER_BYTES {
            return Err(anyhow!("short"));
        }
        let version = b[0];
        if version != 1 {
            return Err(anyhow!("bad version"));
        }
        let flags = b[1];
        let header_len = u16::from_be_bytes([b[2], b[3]]);
        if header_len as usize != vp_voice::CLIENT_VOICE_HEADER_BYTES {
            return Err(anyhow!("bad header len"));
        }

        let channel_route = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
        let ssrc = u32::from_be_bytes([b[8], b[9], b[10], b[11]]);
        let seq = u32::from_be_bytes([b[12], b[13], b[14], b[15]]);
        let ts_ms = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);

        let vad = (flags & 0x01) != 0;
        Ok(Self {
            flags,
            channel_route,
            ssrc,
            seq,
            ts_ms,
            vad,
        })
    }
}

/// Per-sender limiter state.
struct RateState {
    last: Instant,
    tokens_pkts: u32,
    tokens_bytes: u32,

    // For optional monotonic timestamp checks.
    last_ts_ms: Option<u32>,
    last_seen: Instant,
}

impl RateState {
    fn new(pps_limit: u32, bps_limit: u32) -> Self {
        Self {
            last: Instant::now(),
            tokens_pkts: pps_limit,
            tokens_bytes: bps_limit,
            last_ts_ms: None,
            last_seen: Instant::now(),
        }
    }

    fn refill(&mut self, pps_limit: u32, bps_limit: u32, now: Instant) {
        let elapsed = now.duration_since(self.last);
        if elapsed < REFILL_QUANTUM {
            return;
        }

        // Refill proportionally (simple token bucket)
        let secs = elapsed.as_secs_f32();
        let add_pkts = (secs * pps_limit as f32) as u32;
        let add_bytes = (secs * bps_limit as f32) as u32;

        // Cap burst to 1 second worth
        self.tokens_pkts = (self.tokens_pkts + add_pkts).min(pps_limit);
        self.tokens_bytes = (self.tokens_bytes + add_bytes).min(bps_limit);
        self.last = now;
    }

    fn check_monotonic_ts(&mut self, ts: u32, now: Instant) -> bool {
        if now.duration_since(self.last_seen) > STREAM_IDLE_RESET {
            self.last_ts_ms = None;
        }
        if let Some(prev) = self.last_ts_ms {
            // Allow wrap; reject huge backwards jumps.
            let diff = ts.wrapping_sub(prev);
            if diff > 10_000 && diff < (u32::MAX - 10_000) {
                return false;
            }
        }
        self.last_ts_ms = Some(ts);
        self.last_seen = now;
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
        Self {
            window,
            last_seen: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn touch(&mut self, user: UserId) {
        let now = Instant::now();
        self.last_seen.insert(user, now);
        self.order.push_back((user, now));
    }

    fn is_active(&self, user: UserId) -> bool {
        self.last_seen
            .get(&user)
            .map(|t| t.elapsed() <= self.window)
            .unwrap_or(false)
    }

    fn active_count(&self) -> usize {
        self.last_seen
            .values()
            .filter(|t| t.elapsed() <= self.window)
            .count()
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

fn log_forward_failures(failure_count: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
    static SUPPRESSED: AtomicU64 = AtomicU64::new(0);

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let last = LAST_LOG_MS.load(Ordering::Relaxed);

    if now_ms.saturating_sub(last) >= 5_000 {
        let suppressed = SUPPRESSED.swap(0, Ordering::Relaxed);
        LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
        warn!(
            failure_count,
            suppressed, "voice forwarding failed for some receiver sessions"
        );
    } else {
        SUPPRESSED.fetch_add(failure_count as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DatagramTx, MembershipProvider, NoopMetrics, RateState, SessionRegistry, VoiceForwarder,
        VoiceForwarderConfig, STREAM_IDLE_RESET,
    };
    use anyhow::Result;
    use bytes::Bytes;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};
    use tokio::sync::Notify;
    use vp_control::ids::{ChannelId, UserId};

    #[test]
    fn refill_uses_configured_limits() {
        let mut state = RateState::new(50, 1000);
        let now = Instant::now();
        state.last = now - Duration::from_secs(1);

        state.refill(50, 1000, now);

        assert_eq!(state.tokens_pkts, 50);
        assert_eq!(state.tokens_bytes, 1000);
    }

    #[test]
    fn refill_accumulates_under_sub_quantum_pacing() {
        let mut state = RateState::new(100, 1000);
        let start = Instant::now();
        state.tokens_pkts = 0;
        state.tokens_bytes = 0;
        state.last = start;

        state.refill(100, 1000, start + Duration::from_millis(5));
        assert_eq!(state.tokens_pkts, 0);

        state.refill(100, 1000, start + Duration::from_millis(10));
        assert!(state.tokens_pkts > 0);
        assert!(state.tokens_bytes > 0);
    }

    #[test]
    fn first_packet_is_not_rate_limited_cold_start() {
        let state = RateState::new(200, 512 * 1024);
        assert!(state.tokens_pkts >= 1);
        assert!(state.tokens_bytes >= 120);
    }

    #[test]
    fn new_ssrc_resets_monotonic() {
        let mut st1 = RateState::new(200, 1024);
        let now = Instant::now();
        assert!(st1.check_monotonic_ts(1000, now));

        let mut st2 = RateState::new(200, 1024);
        assert!(st2.check_monotonic_ts(0, now));
    }

    #[tokio::test]
    async fn allow_rate_accepts_new_ssrc_with_low_timestamp() {
        let fwd = VoiceForwarder::new(
            VoiceForwarderConfig::default(),
            Arc::new(TestSessionRegistry),
            Arc::new(TestMembership),
            Arc::new(NoopMetrics),
        );
        let user = UserId::new();
        let now = Instant::now();

        assert!(fwd.allow_rate_at(user, 111, 1, 1000, now).await);
        assert!(
            fwd.allow_rate_at(user, 222, 1, 0, now + Duration::from_millis(1))
                .await
        );
    }
    #[test]
    fn idle_reset_clears_monotonic() {
        let mut st = RateState::new(200, 1024);
        let now = Instant::now();
        assert!(st.check_monotonic_ts(50_000, now));
        assert!(!st.check_monotonic_ts(0, now + Duration::from_secs(1)));

        assert!(st.check_monotonic_ts(0, now + STREAM_IDLE_RESET + Duration::from_millis(1)));
    }

    struct TestSessionRegistry;

    #[async_trait::async_trait]
    impl SessionRegistry for TestSessionRegistry {
        async fn get_sessions(&self, _user: UserId) -> Vec<(String, Arc<dyn DatagramTx>)> {
            Vec::new()
        }
    }

    struct TestMembership;

    #[async_trait::async_trait]
    impl MembershipProvider for TestMembership {
        async fn resolve_channel_for_sender(
            &self,
            _sender: UserId,
            _route_key: u32,
        ) -> Option<ChannelId> {
            None
        }

        async fn list_members(&self, _channel: ChannelId) -> Vec<UserId> {
            Vec::new()
        }

        async fn is_muted(&self, _channel: ChannelId, _sender: UserId) -> bool {
            false
        }

        async fn is_deafened(&self, _channel: ChannelId, _user: UserId) -> bool {
            false
        }

        async fn max_talkers(&self, _channel: ChannelId) -> usize {
            1
        }
    }

    struct FakeDatagramTx {
        closed: Arc<AtomicBool>,
        exited: Arc<Notify>,
    }

    impl Drop for FakeDatagramTx {
        fn drop(&mut self) {
            self.closed.store(true, Ordering::SeqCst);
            self.exited.notify_waiters();
        }
    }

    #[async_trait::async_trait]
    impl DatagramTx for FakeDatagramTx {
        async fn send(&self, _bytes: Bytes) -> Result<()> {
            if self.closed.load(Ordering::SeqCst) {
                anyhow::bail!("closed");
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn unregister_session_removes_send_loop() {
        let fwd = VoiceForwarder::new(
            VoiceForwarderConfig::default(),
            Arc::new(TestSessionRegistry),
            Arc::new(TestMembership),
            Arc::new(NoopMetrics),
        );
        let user = UserId::new();
        let session_id = "s1".to_string();
        let key = (user, session_id.clone());

        let closed = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(Notify::new());
        let dtx: Arc<dyn DatagramTx> = Arc::new(FakeDatagramTx {
            closed: closed.clone(),
            exited: exited.clone(),
        });

        assert!(
            fwd.enqueue_to_receiver_session(
                user,
                session_id.clone(),
                dtx,
                Bytes::from_static(b"x")
            )
            .await
        );
        assert!(fwd.send_loops.read().await.contains_key(&key));

        fwd.unregister_session(user, &session_id).await;
        assert!(!fwd.send_loops.read().await.contains_key(&key));

        let _ = tokio::time::timeout(Duration::from_secs(1), exited.notified()).await;
    }
}
