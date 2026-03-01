pub const QUIC_MAX_DATAGRAM_BYTES: usize = 1200;
pub const FORWARDER_ADDED_HEADER_BYTES: usize = 32;
pub const MAX_INBOUND_VOICE_DATAGRAM_BYTES: usize =
    QUIC_MAX_DATAGRAM_BYTES - FORWARDER_ADDED_HEADER_BYTES;
pub const CLIENT_VOICE_HEADER_BYTES: usize = 20;
pub const FORWARDED_VOICE_HEADER_BYTES: usize =
    CLIENT_VOICE_HEADER_BYTES + FORWARDER_ADDED_HEADER_BYTES;
pub const MAX_OPUS_PAYLOAD_BYTES: usize =
    MAX_INBOUND_VOICE_DATAGRAM_BYTES - CLIENT_VOICE_HEADER_BYTES;

pub fn outbound_payload_fits(payload_len: usize) -> bool {
    payload_len <= MAX_OPUS_PAYLOAD_BYTES
}

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
    fn outbound_payload_validation_rejects_oversized() {
        assert!(outbound_payload_fits(MAX_OPUS_PAYLOAD_BYTES));
        assert!(!outbound_payload_fits(MAX_OPUS_PAYLOAD_BYTES + 1));
    }
}
