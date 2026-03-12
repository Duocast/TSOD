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
