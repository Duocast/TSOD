#![allow(dead_code)]
use bytes::{BufMut, Bytes, BytesMut};

pub const VOICE_VERSION: u8 = 1;
pub const VOICE_HDR_LEN: usize = vp_voice::CLIENT_VOICE_HEADER_BYTES;
pub const VOICE_FORWARDED_HDR_LEN: usize = vp_voice::FORWARDED_VOICE_HEADER_BYTES;

pub fn outbound_payload_fits(payload_len: usize) -> bool {
    vp_voice::outbound_payload_fits(payload_len)
}

pub fn make_voice_datagram(
    channel_route_hash: u32,
    ssrc: u32,
    seq: u32,
    ts_ms: u32,
    vad: bool,
    payload: &[u8],
) -> Bytes {
    let mut b = BytesMut::with_capacity(VOICE_HDR_LEN + payload.len());
    b.put_u8(VOICE_VERSION);
    let flags = if vad { 0x01 } else { 0x00 };
    b.put_u8(flags);
    b.put_u16(VOICE_HDR_LEN as u16); // header_len
    b.put_u32(channel_route_hash);
    b.put_u32(ssrc);
    b.put_u32(seq);
    b.put_u32(ts_ms);
    b.extend_from_slice(payload);
    b.freeze()
}

#[cfg(test)]
mod tests {
    use super::outbound_payload_fits;

    #[test]
    fn oversized_payloads_are_rejected() {
        assert!(outbound_payload_fits(vp_voice::MAX_OPUS_PAYLOAD_BYTES));
        assert!(!outbound_payload_fits(vp_voice::MAX_OPUS_PAYLOAD_BYTES + 1));
    }
}
