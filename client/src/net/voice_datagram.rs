use bytes::{BufMut, Bytes, BytesMut};

pub const VOICE_VERSION: u8 = 1;
pub const VOICE_HDR_LEN: usize = 20;

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
