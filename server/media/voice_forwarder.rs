use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use bytes::{BufMut, Bytes, BytesMut};
use tokio::sync::{mpsc, RwLock};
use tracing::warn;
use vp_control::ids::{ChannelId, UserId};

use crate::datagram_send_policy::now_ms;

#[async_trait::async_trait]
pub trait DatagramTx: Send + Sync {
    async fn send(&self, bytes: Bytes) -> Result<()>;
    fn session_id(&self) -> &str;
    fn max_datagram_size(&self) -> Option<usize>;
    fn send_voice(
        &self,
        now_ms: u64,
        channel_id: ChannelId,
        pkt: Bytes,
        prune_tx: &mpsc::Sender<()>,
        metrics: &dyn crate::datagram_send_policy::DatagramSendPolicyMetrics,
    );
    fn send_video_best_effort(
        &self,
        now_ms: u64,
        channel_id: ChannelId,
        pkt: Bytes,
        prune_tx: &mpsc::Sender<()>,
        metrics: &dyn crate::datagram_send_policy::DatagramSendPolicyMetrics,
    );
}

#[async_trait::async_trait]
pub trait SessionRegistry: Send + Sync {
    async fn get_sessions(&self, user: UserId) -> Vec<(String, Arc<dyn DatagramTx>)>;
}

#[async_trait::async_trait]
pub trait MembershipProvider: Send + Sync {
    async fn resolve_channel_for_sender(&self, sender: UserId, route_key: u32)
        -> Option<ChannelId>;
    async fn list_members(&self, channel: ChannelId) -> Vec<UserId>;
    async fn is_muted(&self, channel: ChannelId, sender: UserId) -> bool;
    async fn is_deafened(&self, channel: ChannelId, user: UserId) -> bool;
    async fn max_talkers(&self, channel: ChannelId) -> usize;
}

pub trait VoiceMetrics:
    crate::datagram_send_policy::DatagramSendPolicyMetrics + Send + Sync
{
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

#[async_trait::async_trait]
impl DatagramTx for crate::datagram_send_policy::SessionSendCtx {
    async fn send(&self, bytes: Bytes) -> Result<()> {
        self.conn
            .send_datagram(bytes)
            .map_err(|e: quinn::SendDatagramError| anyhow!(e.to_string()))
    }
    fn session_id(&self) -> &str {
        &self.session_id
    }
    fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }
    fn send_voice(
        &self,
        now_ms: u64,
        channel_id: ChannelId,
        pkt: Bytes,
        prune_tx: &mpsc::Sender<()>,
        metrics: &dyn crate::datagram_send_policy::DatagramSendPolicyMetrics,
    ) {
        crate::datagram_send_policy::SessionSendCtx::send_voice(
            self, now_ms, channel_id, pkt, prune_tx, metrics,
        )
    }
    fn send_video_best_effort(
        &self,
        now_ms: u64,
        channel_id: ChannelId,
        pkt: Bytes,
        prune_tx: &mpsc::Sender<()>,
        metrics: &dyn crate::datagram_send_policy::DatagramSendPolicyMetrics,
    ) {
        crate::datagram_send_policy::SessionSendCtx::send_video_best_effort(
            self, now_ms, channel_id, pkt, prune_tx, metrics,
        )
    }
}

impl crate::datagram_send_policy::DatagramSendPolicyMetrics for NoopMetrics {
    fn inc_no_datagrams(&self) {}
    fn inc_oversize_drop(&self) {}
    fn inc_conn_lost(&self) {}
    fn inc_send_err_other(&self) {}
    fn inc_prune_evt_dropped(&self) {}
    fn inc_video_dropped_due_to_space(&self) {}
}

#[derive(Clone, Debug)]
pub struct VoiceForwarderConfig {
    pub max_datagram_bytes: usize,
    pub min_datagram_bytes: usize,
    pub sender_pps_limit: u32,
    pub sender_bps_limit: u32,
    pub talker_activity_window: Duration,
    pub vad_required_for_talker: bool,
}
impl Default for VoiceForwarderConfig {
    fn default() -> Self {
        Self {
            max_datagram_bytes: vp_voice::MAX_INBOUND_VOICE_DATAGRAM_BYTES,
            min_datagram_bytes: vp_voice::CLIENT_VOICE_HEADER_BYTES,
            sender_pps_limit: 200,
            sender_bps_limit: 512 * 1024,
            talker_activity_window: Duration::from_millis(800),
            vad_required_for_talker: false,
        }
    }
}

pub struct VoiceForwarder {
    cfg: VoiceForwarderConfig,
    sessions: Arc<dyn SessionRegistry>,
    membership: Arc<dyn MembershipProvider>,
    metrics: Arc<dyn VoiceMetrics>,
    prune_tx: mpsc::Sender<()>,
    talkers: RwLock<HashMap<ChannelId, TalkerSet>>,
    rate: RwLock<HashMap<(UserId, u32), RateState>>,
}

impl VoiceForwarder {
    pub fn new(
        cfg: VoiceForwarderConfig,
        sessions: Arc<dyn SessionRegistry>,
        membership: Arc<dyn MembershipProvider>,
        metrics: Arc<dyn VoiceMetrics>,
        prune_tx: mpsc::Sender<()>,
    ) -> Self {
        Self {
            cfg,
            sessions,
            membership,
            metrics,
            prune_tx,
            talkers: RwLock::new(HashMap::new()),
            rate: RwLock::new(HashMap::new()),
        }
    }

    pub async fn handle_incoming(&self, sender: UserId, datagram: Bytes) {
        self.metrics.inc_rx_packets();
        self.metrics.inc_rx_bytes(datagram.len());
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
        if !self
            .allow_rate(sender, parsed.ssrc, datagram.len() as u32, parsed.ts_ms)
            .await
        {
            self.metrics.inc_drop_rate_limited();
            return;
        }
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
        if self.membership.is_muted(channel, sender).await
            || self.membership.is_deafened(channel, sender).await
        {
            self.metrics.inc_drop_muted();
            return;
        }
        let vad_ok = !self.cfg.vad_required_for_talker || parsed.vad;
        if vad_ok && !self.allow_talker(channel, sender).await {
            self.metrics.inc_drop_talker_limit();
            return;
        }

        let members = self.membership.list_members(channel).await;
        let mut recipients = Vec::new();
        for uid in members {
            if uid == sender || self.membership.is_deafened(channel, uid).await {
                continue;
            }
            recipients.extend(
                self.sessions
                    .get_sessions(uid)
                    .await
                    .into_iter()
                    .map(|(_, s)| s),
            );
        }

        let now = now_ms();
        let mut forwarded = 0;
        for sess in recipients {
            let max_wire = sess
                .max_datagram_size()
                .unwrap_or(vp_voice::QUIC_MAX_DATAGRAM_BYTES);
            if let Some(outbound) =
                build_forwarded_voice_datagram(max_wire, &parsed, sender, channel, &datagram)
            {
                debug_assert!(outbound.len() <= max_wire);
                sess.send_voice(
                    now,
                    channel,
                    outbound,
                    &self.prune_tx,
                    self.metrics.as_ref(),
                );
                forwarded += 1;
            } else {
                crate::datagram_send_policy::DatagramSendPolicyMetrics::inc_oversize_drop(
                    self.metrics.as_ref(),
                );
            }
        }
        self.metrics.inc_forwarded(forwarded);
    }

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

pub fn build_forwarded_voice_datagram(
    max_wire: usize,
    parsed: &VoicePacket,
    sender: UserId,
    channel: ChannelId,
    datagram: &Bytes,
) -> Option<Bytes> {
    let payload = &datagram[vp_voice::CLIENT_VOICE_HEADER_BYTES..];
    let total = vp_voice::FORWARDED_VOICE_HEADER_BYTES + payload.len();
    // Receivers enforce APP_MEDIA_MTU regardless of advertised QUIC datagram max.
    let max_app = max_wire.min(vp_voice::APP_MEDIA_MTU);
    if total > max_app {
        return None;
    }
    let mut out = BytesMut::with_capacity(total);
    out.put_u8(1);
    out.put_u8(parsed.flags);
    out.put_u16(vp_voice::FORWARDED_VOICE_HEADER_BYTES as u16);
    out.put_u32(parsed.channel_route);
    out.put_u32(parsed.ssrc);
    out.put_u32(parsed.seq);
    out.put_u32(parsed.ts_ms);
    out.extend_from_slice(sender.0.as_bytes());
    out.extend_from_slice(channel.0.as_bytes());
    out.extend_from_slice(payload);
    Some(out.freeze())
}

#[derive(Clone, Copy, Debug)]
pub struct VoicePacket {
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
        if b[0] != 1 {
            return Err(anyhow!("bad version"));
        }
        let flags = b[1];
        if u16::from_be_bytes([b[2], b[3]]) as usize != vp_voice::CLIENT_VOICE_HEADER_BYTES {
            return Err(anyhow!("bad header len"));
        }
        Ok(Self {
            flags,
            channel_route: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            ssrc: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
            seq: u32::from_be_bytes([b[12], b[13], b[14], b[15]]),
            ts_ms: u32::from_be_bytes([b[16], b[17], b[18], b[19]]),
            vad: (flags & 0x01) != 0,
        })
    }
}

const REFILL_QUANTUM: Duration = Duration::from_millis(10);
const STREAM_IDLE_RESET: Duration = Duration::from_secs(10);
struct RateState {
    last: Instant,
    tokens_pkts: u32,
    tokens_bytes: u32,
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
        let secs = elapsed.as_secs_f32();
        self.tokens_pkts = (self.tokens_pkts + (secs * pps_limit as f32) as u32).min(pps_limit);
        self.tokens_bytes = (self.tokens_bytes + (secs * bps_limit as f32) as u32).min(bps_limit);
        self.last = now;
    }
    fn check_monotonic_ts(&mut self, ts: u32, now: Instant) -> bool {
        if now.duration_since(self.last_seen) > STREAM_IDLE_RESET {
            self.last_ts_ms = None;
        }
        if let Some(prev) = self.last_ts_ms {
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
struct TalkerSet {
    window: Duration,
    last_seen: HashMap<UserId, Instant>,
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
            if let Some(cur) = self.last_seen.get(&u).copied() {
                if cur == t && now.duration_since(cur) > self.window {
                    self.last_seen.remove(&u);
                }
            }
        }
    }
}

fn _log_forward_failures(failure_count: usize) {
    warn!(failure_count, "voice forwarding failures");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_forwarded_voice_respects_max() {
        let sender = UserId::new();
        let channel = ChannelId::new();
        let mut bytes = BytesMut::new();
        bytes.extend_from_slice(&[1, 0]);
        bytes.put_u16(vp_voice::CLIENT_VOICE_HEADER_BYTES as u16);
        bytes.put_u32(1);
        bytes.put_u32(2);
        bytes.put_u32(3);
        bytes.put_u32(4);
        bytes.extend_from_slice(&[7; 32]);
        let datagram = bytes.freeze();
        let parsed = VoicePacket::parse(&datagram).unwrap();

        let too_small = vp_voice::FORWARDED_VOICE_HEADER_BYTES + 31;
        assert!(
            build_forwarded_voice_datagram(too_small, &parsed, sender, channel, &datagram)
                .is_none()
        );
        assert!(build_forwarded_voice_datagram(
            vp_voice::FORWARDED_VOICE_HEADER_BYTES + 32,
            &parsed,
            sender,
            channel,
            &datagram
        )
        .is_some());
    }
}
