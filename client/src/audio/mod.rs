pub mod capture;
pub mod dsp;
pub mod jitter;
pub mod opus;
pub mod playout;
pub(crate) mod resample;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

pub(crate) fn pcm_peak_level(pcm: &[i16]) -> f32 {
    let peak = pcm
        .iter()
        .map(|sample| (*sample as i32).unsigned_abs() as f32 / 32768.0)
        .fold(0.0_f32, f32::max);
    peak.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::pcm_peak_level;

    #[test]
    fn pcm_peak_level_zero_input() {
        assert_eq!(pcm_peak_level(&[0, 0, 0]), 0.0);
    }

    #[test]
    fn pcm_peak_level_i16_max_is_full_scale() {
        assert!((pcm_peak_level(&[i16::MAX]) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn pcm_peak_level_i16_min_does_not_panic() {
        assert!((pcm_peak_level(&[i16::MIN]) - 1.0).abs() < 1e-6);
    }
}
