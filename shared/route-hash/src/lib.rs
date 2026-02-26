use uuid::Uuid;

const FNV_OFFSET_BASIS: u32 = 0x811C9DC5;
const FNV_PRIME: u32 = 0x01000193;

/// Computes the canonical 32-bit route hash for a channel UUID.
///
/// Algorithm: FNV-1a (32-bit)
/// Input bytes: UUID raw bytes (`Uuid::as_bytes()` in RFC 4122 byte order)
/// Output: native `u32` value of the hash accumulator
pub fn channel_route_hash(channel_id: Uuid) -> u32 {
    let mut h = FNV_OFFSET_BASIS;
    for &b in channel_id.as_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::channel_route_hash;
    use uuid::Uuid;

    #[test]
    fn route_hash_is_stable_for_known_uuid() {
        let id = Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap();
        assert_eq!(channel_route_hash(id), 0xC586_1100);
    }
}
