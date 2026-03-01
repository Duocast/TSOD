//! Video/screenshare datagram builder and parser.
//!
//! Wire format (24-byte fixed header, little-endian multi-byte fields):
//!   0:  u8  version         (1)
//!   1:  u8  kind            (0x02 = video/screenshare)
//!   2:  u64 stream_tag      (collision-resistant stream id) [LE]
//!  10:  u8  layer_id        (simulcast/spatial layer)
//!  11:  u8  flags           (bit0=keyframe, bit1=recovery, bit2=end_of_frame)
//!  12:  u32 frame_seq       (monotonic frame sequence) [LE]
//!  16:  u16 frag_idx        (fragment index within frame) [LE]
//!  18:  u16 frag_total      (total fragments in frame) [LE]
//!  20:  u32 ts_ms           (sender timestamp ms, monotonic-ish) [LE]
//!  24:  ... payload bytes   (encoded video fragment)

use bytes::{BufMut, Bytes, BytesMut};

pub const VIDEO_HDR_LEN: usize = vp_voice::VIDEO_HEADER_BYTES;
pub const MAX_VIDEO_PAYLOAD: usize = vp_voice::MAX_VIDEO_PAYLOAD_BYTES;

/// Check that a video payload fits in a single datagram.
pub fn video_payload_fits(payload_len: usize) -> bool {
    payload_len <= MAX_VIDEO_PAYLOAD
}

/// Parsed video datagram header (zero-copy view).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoHeader {
    pub stream_tag: u64,
    pub layer_id: u8,
    pub flags: u8,
    pub frame_seq: u32,
    pub frag_idx: u16,
    pub frag_total: u16,
    pub ts_ms: u32,
}

impl VideoHeader {
    pub fn is_keyframe(&self) -> bool {
        self.flags & vp_voice::VIDEO_FLAG_KEYFRAME != 0
    }

    pub fn is_recovery(&self) -> bool {
        self.flags & vp_voice::VIDEO_FLAG_RECOVERY != 0
    }

    pub fn is_end_of_frame(&self) -> bool {
        self.flags & vp_voice::VIDEO_FLAG_END_OF_FRAME != 0
    }

    pub fn is_priority(&self) -> bool {
        self.is_keyframe() || self.is_recovery()
    }

    /// Parse from raw datagram bytes. Returns None on malformed input.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < VIDEO_HDR_LEN {
            return None;
        }
        if buf[0] != vp_voice::DATAGRAM_VERSION {
            return None;
        }
        if buf[1] != vp_voice::DATAGRAM_KIND_VIDEO {
            return None;
        }
        let stream_tag = u64::from_le_bytes([
            buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
        ]);
        let layer_id = buf[10];
        let flags = buf[11];
        let frame_seq = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let frag_idx = u16::from_le_bytes([buf[16], buf[17]]);
        let frag_total = u16::from_le_bytes([buf[18], buf[19]]);
        let ts_ms = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);

        // Basic validation: frag_total must be >= 1, frag_idx < frag_total.
        if frag_total == 0 || frag_idx >= frag_total {
            return None;
        }

        Some(Self {
            stream_tag,
            layer_id,
            flags,
            frame_seq,
            frag_idx,
            frag_total,
            ts_ms,
        })
    }
}

/// Serialize a video datagram header + payload into a `Bytes`.
///
/// For allocation-free steady-state, callers should use `write_into` with
/// a pre-allocated `BytesMut` instead.
pub fn make_video_datagram(hdr: &VideoHeader, payload: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(VIDEO_HDR_LEN + payload.len());
    write_header_into(&mut buf, hdr);
    buf.extend_from_slice(payload);
    buf.freeze()
}

/// Write the video header into a pre-allocated `BytesMut` (allocation-free).
#[inline]
pub fn write_header_into(buf: &mut BytesMut, hdr: &VideoHeader) {
    buf.put_u8(vp_voice::DATAGRAM_VERSION);
    buf.put_u8(vp_voice::DATAGRAM_KIND_VIDEO);
    buf.put_u64_le(hdr.stream_tag);
    buf.put_u8(hdr.layer_id);
    buf.put_u8(hdr.flags);
    buf.put_u32_le(hdr.frame_seq);
    buf.put_u16_le(hdr.frag_idx);
    buf.put_u16_le(hdr.frag_total);
    buf.put_u32_le(hdr.ts_ms);
}

/// Write header + payload into a pre-allocated `BytesMut`, returning frozen `Bytes`.
/// The caller should ensure `buf` has enough capacity (VIDEO_HDR_LEN + payload.len()).
#[inline]
pub fn write_video_datagram_into(buf: &mut BytesMut, hdr: &VideoHeader, payload: &[u8]) -> Bytes {
    buf.clear();
    buf.reserve(VIDEO_HDR_LEN + payload.len());
    write_header_into(buf, hdr);
    buf.extend_from_slice(payload);
    buf.split().freeze()
}

/// Return the payload slice from a parsed video datagram buffer.
#[inline]
pub fn video_payload(buf: &[u8]) -> &[u8] {
    if buf.len() <= VIDEO_HDR_LEN {
        &[]
    } else {
        &buf[VIDEO_HDR_LEN..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_header() {
        let hdr = VideoHeader {
            stream_tag: 0xDEADBEEF_CAFEBABE,
            layer_id: 2,
            flags: vp_voice::VIDEO_FLAG_KEYFRAME | vp_voice::VIDEO_FLAG_END_OF_FRAME,
            frame_seq: 42,
            frag_idx: 0,
            frag_total: 3,
            ts_ms: 12345,
        };
        let payload = b"hello video";
        let datagram = make_video_datagram(&hdr, payload);

        assert_eq!(datagram.len(), VIDEO_HDR_LEN + payload.len());
        let parsed = VideoHeader::parse(&datagram).expect("should parse");
        assert_eq!(parsed, hdr);
        assert_eq!(video_payload(&datagram), payload);
    }

    #[test]
    fn reject_short_buffer() {
        assert!(VideoHeader::parse(&[0u8; 10]).is_none());
    }

    #[test]
    fn reject_bad_version() {
        let mut buf = [0u8; VIDEO_HDR_LEN];
        buf[0] = 99;
        buf[1] = vp_voice::DATAGRAM_KIND_VIDEO;
        assert!(VideoHeader::parse(&buf).is_none());
    }

    #[test]
    fn reject_bad_kind() {
        let mut buf = [0u8; VIDEO_HDR_LEN];
        buf[0] = vp_voice::DATAGRAM_VERSION;
        buf[1] = 0xFF;
        assert!(VideoHeader::parse(&buf).is_none());
    }

    #[test]
    fn reject_invalid_fragment_info() {
        let hdr = VideoHeader {
            stream_tag: 1,
            layer_id: 0,
            flags: 0,
            frame_seq: 0,
            frag_idx: 5,
            frag_total: 3, // frag_idx >= frag_total
            ts_ms: 0,
        };
        let datagram = make_video_datagram(&hdr, &[]);
        assert!(VideoHeader::parse(&datagram).is_none());
    }

    #[test]
    fn reject_zero_frag_total() {
        let mut buf = BytesMut::with_capacity(VIDEO_HDR_LEN);
        buf.put_u8(vp_voice::DATAGRAM_VERSION);
        buf.put_u8(vp_voice::DATAGRAM_KIND_VIDEO);
        buf.put_u64_le(1);
        buf.put_u8(0);
        buf.put_u8(0);
        buf.put_u32_le(0);
        buf.put_u16_le(0);
        buf.put_u16_le(0); // frag_total = 0
        buf.put_u32_le(0);
        assert!(VideoHeader::parse(&buf).is_none());
    }

    #[test]
    fn flags_helpers() {
        let hdr = VideoHeader {
            stream_tag: 1,
            layer_id: 0,
            flags: vp_voice::VIDEO_FLAG_KEYFRAME | vp_voice::VIDEO_FLAG_RECOVERY,
            frame_seq: 0,
            frag_idx: 0,
            frag_total: 1,
            ts_ms: 0,
        };
        assert!(hdr.is_keyframe());
        assert!(hdr.is_recovery());
        assert!(!hdr.is_end_of_frame());
        assert!(hdr.is_priority());
    }

    #[test]
    fn write_into_reuses_buffer() {
        let hdr = VideoHeader {
            stream_tag: 42,
            layer_id: 1,
            flags: 0,
            frame_seq: 100,
            frag_idx: 0,
            frag_total: 1,
            ts_ms: 999,
        };
        let mut buf = BytesMut::with_capacity(VIDEO_HDR_LEN + 64);
        let payload = b"reuse test";

        let b1 = write_video_datagram_into(&mut buf, &hdr, payload);
        let parsed = VideoHeader::parse(&b1).unwrap();
        assert_eq!(parsed, hdr);
        assert_eq!(video_payload(&b1), payload);

        // Second write reuses the same BytesMut allocation.
        let b2 = write_video_datagram_into(&mut buf, &hdr, b"second");
        assert_eq!(video_payload(&b2), b"second");
    }

    #[test]
    fn video_payload_fits_check() {
        assert!(video_payload_fits(MAX_VIDEO_PAYLOAD));
        assert!(!video_payload_fits(MAX_VIDEO_PAYLOAD + 1));
    }
}
