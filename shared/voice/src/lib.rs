pub const QUIC_MAX_DATAGRAM_BYTES: usize = 1200;
/// Application-level media MTU enforced by demux loops; larger datagrams are dropped.
pub const APP_MEDIA_MTU: usize = 1152;
pub const FORWARDER_ADDED_HEADER_BYTES: usize = 32;
/// Max client->server voice datagram size so forwarded metadata still fits APP_MEDIA_MTU.
pub const MAX_INBOUND_VOICE_DATAGRAM_BYTES: usize = APP_MEDIA_MTU - FORWARDER_ADDED_HEADER_BYTES;
pub const CLIENT_VOICE_HEADER_BYTES: usize = 20;
pub const FORWARDED_VOICE_HEADER_BYTES: usize =
    CLIENT_VOICE_HEADER_BYTES + FORWARDER_ADDED_HEADER_BYTES;
pub const MAX_OPUS_PAYLOAD_BYTES: usize =
    MAX_INBOUND_VOICE_DATAGRAM_BYTES - CLIENT_VOICE_HEADER_BYTES;

pub fn outbound_payload_fits(payload_len: usize) -> bool {
    payload_len <= MAX_OPUS_PAYLOAD_BYTES
}

// ── Datagram type dispatch ─────────────────────────────────────────────
//
// Byte 0: protocol version
// Byte 1: datagram kind (voice=0x01, video/screenshare=0x02)

pub const VOICE_VERSION: u8 = 1;
pub const VIDEO_VERSION: u8 = 2;
pub const DATAGRAM_VERSION: u8 = VOICE_VERSION;
pub const DATAGRAM_KIND_VOICE: u8 = 0x01;
pub const DATAGRAM_KIND_VIDEO: u8 = 0x02;
pub const MAX_FRAGS_PER_FRAME: u16 = 1024;

/// Parse kind byte from a raw datagram. Returns None for unknown/short.
#[inline]
pub fn datagram_kind(buf: &[u8]) -> Option<u8> {
    if buf.len() < 2 {
        return None;
    }
    Some(buf[1])
}

// ── Video datagram header ──────────────────────────────────────────────
//
// Fixed 22-byte header (little-endian for multi-byte fields):
//   0:  u8  version           (1)
//   1:  u8  kind              (0x02)
//   2:  u64 stream_tag        (collision-resistant stream id)
//  10:  u8  layer_id          (simulcast/spatial layer)
//  11:  u8  flags             (bit0=is_keyframe, bit1=is_recovery, bit2=end_of_frame)
//  12:  u32 frame_seq         (monotonic frame sequence)
//  16:  u16 frag_idx          (fragment index within frame)
//  18:  u16 frag_total        (total fragments in frame)
//  20:  u32 ts_ms             (sender timestamp; monotonic-ish)
//  24:  ... payload bytes

pub const VIDEO_HEADER_BYTES: usize = 24;
pub const VIDEO_FLAG_KEYFRAME: u8 = 0x01;
pub const VIDEO_FLAG_RECOVERY: u8 = 0x02;
pub const VIDEO_FLAG_END_OF_FRAME: u8 = 0x04;
pub const MAX_VIDEO_DATAGRAM_BYTES: usize = APP_MEDIA_MTU;
pub const MAX_VIDEO_PAYLOAD_BYTES: usize = MAX_VIDEO_DATAGRAM_BYTES - VIDEO_HEADER_BYTES;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_payload_math_is_consistent() {
        assert_eq!(
            MAX_OPUS_PAYLOAD_BYTES + CLIENT_VOICE_HEADER_BYTES,
            MAX_INBOUND_VOICE_DATAGRAM_BYTES
        );
    }

    #[test]
    fn voice_forwarding_headroom_matches_app_mtu() {
        assert!(APP_MEDIA_MTU <= QUIC_MAX_DATAGRAM_BYTES);
        assert_eq!(
            MAX_INBOUND_VOICE_DATAGRAM_BYTES + FORWARDER_ADDED_HEADER_BYTES,
            APP_MEDIA_MTU
        );
        assert_eq!(
            MAX_OPUS_PAYLOAD_BYTES + FORWARDED_VOICE_HEADER_BYTES,
            APP_MEDIA_MTU
        );
    }

    #[test]
    fn outbound_payload_validation_rejects_oversized() {
        assert!(outbound_payload_fits(MAX_OPUS_PAYLOAD_BYTES));
        assert!(!outbound_payload_fits(MAX_OPUS_PAYLOAD_BYTES + 1));
    }

    #[test]
    fn video_header_fits_within_datagram() {
        assert!(VIDEO_HEADER_BYTES < MAX_VIDEO_DATAGRAM_BYTES);
        assert!(MAX_VIDEO_PAYLOAD_BYTES > 0);
    }

    #[test]
    fn datagram_kind_works() {
        assert_eq!(datagram_kind(&[]), None);
        assert_eq!(datagram_kind(&[1]), None);
        assert_eq!(
            datagram_kind(&[1, DATAGRAM_KIND_VOICE]),
            Some(DATAGRAM_KIND_VOICE)
        );
        assert_eq!(
            datagram_kind(&[1, DATAGRAM_KIND_VIDEO]),
            Some(DATAGRAM_KIND_VIDEO)
        );
    }
}
