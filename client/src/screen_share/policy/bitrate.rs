#[derive(Debug, Clone, Copy)]
pub struct LayerBitrateHint {
    pub layer_id: u32,
    pub target_bitrate_bps: u32,
}

pub fn bitrate_hint_for_layer(layer_id: u32) -> LayerBitrateHint {
    let target_bitrate_bps = match layer_id {
        2 => 8_000_000,
        1 => 3_500_000,
        _ => 1_500_000,
    };
    LayerBitrateHint {
        layer_id,
        target_bitrate_bps,
    }
}

pub fn bitrate_for_pressure(
    base_bitrate_bps: u32,
    pressure_level: u8,
    min_bitrate_bps: u32,
) -> u32 {
    let scaled = match pressure_level {
        0 => base_bitrate_bps,
        1 => (base_bitrate_bps as f32 * 0.9) as u32,
        2 => (base_bitrate_bps as f32 * 0.75) as u32,
        _ => (base_bitrate_bps as f32 * 0.6) as u32,
    };
    scaled.max(min_bitrate_bps).min(base_bitrate_bps)
}

#[cfg(test)]
mod tests {
    use super::bitrate_for_pressure;

    #[test]
    fn pressure_reduces_bitrate_with_floor() {
        assert_eq!(bitrate_for_pressure(3_000_000, 0, 900_000), 3_000_000);
        assert_eq!(bitrate_for_pressure(3_000_000, 2, 900_000), 2_250_000);
        assert_eq!(bitrate_for_pressure(3_000_000, 4, 2_000_000), 2_000_000);
    }
}
