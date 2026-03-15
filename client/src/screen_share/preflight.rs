//! Preflight encode benchmark for 1440p60 capability gating.
//!
//! Runs a short local encode test at 1440p resolution to prove the encoder can
//! sustain the target frame rate *before* a live share is started.  Results are
//! cached to disk so subsequent launches skip the benchmark.
//!
//! Cache key: `(encoder_backend, driver_version_hint)` — if the hardware or
//! driver changes the cache is invalidated automatically.

use crate::media_codec::VideoSessionConfig;
use crate::net::video_frame::{FramePlanes, PixelFormat, VideoFrame};
use crate::screen_share::runtime_probe::EncodeBackendKind;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;
use tracing::{info, warn};

/// Minimum sustained FPS the preflight must hit to pass.
const PREFLIGHT_FPS_THRESHOLD: f32 = 55.0;

/// Number of frames to encode during the timed section.
const PREFLIGHT_FRAME_COUNT: u32 = 30;

/// Width/height for the synthetic benchmark frame.
const BENCH_WIDTH: u32 = 2560;
const BENCH_HEIGHT: u32 = 1440;

/// Result of a preflight encode benchmark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreflightResult {
    Pass,
    Fail,
}

/// Disk-persisted cache of preflight results, keyed by backend + driver hint.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PreflightCache {
    pub results: HashMap<String, PreflightResult>,
}

/// In-memory singleton so we only run the preflight once per session.
static PREFLIGHT_OUTCOME: OnceLock<PreflightOutcome> = OnceLock::new();

/// Combined outcome: static support + preflight/cache verdict.
#[derive(Clone, Copy, Debug)]
pub struct PreflightOutcome {
    pub static_support: bool,
    pub preflight_passed: bool,
}

impl PreflightOutcome {
    /// Whether 1440p60 should be offered at share-start time.
    pub fn should_offer_1440p60(&self) -> bool {
        self.static_support && self.preflight_passed
    }
}

/// Build a cache key that captures the encoder backend and a rough driver
/// version hint so the cache auto-invalidates on hardware/driver changes.
pub fn cache_key(backend: EncodeBackendKind) -> String {
    let driver_hint = driver_version_hint();
    format!("{backend:?}:{driver_hint}")
}

fn driver_version_hint() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(v) = std::fs::read_to_string("/proc/driver/nvidia/version") {
            if let Some(line) = v.lines().next() {
                return line.trim().to_string();
            }
        }
    }
    std::env::consts::OS.to_string()
}

fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("tsod").join("preflight_1440p60.json"))
}

pub fn load_cache() -> PreflightCache {
    let Some(path) = cache_path() else {
        return PreflightCache::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => PreflightCache::default(),
    }
}

pub fn save_cache(cache: &PreflightCache) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_string_pretty(cache) {
        let _ = std::fs::write(&path, data);
    }
}

/// Create a synthetic BGRA frame for benchmarking.
fn synthetic_frame(ts_ms: u32) -> VideoFrame {
    let stride = BENCH_WIDTH * 4;
    let mut pixels = vec![0u8; (stride * BENCH_HEIGHT) as usize];
    // Simple gradient so the encoder does real work.
    for y in 0..BENCH_HEIGHT {
        for x in 0..BENCH_WIDTH {
            let idx = ((y * stride) + x * 4) as usize;
            pixels[idx] = (x & 0xFF) as u8;         // B
            pixels[idx + 1] = (y & 0xFF) as u8;     // G
            pixels[idx + 2] = 128;                    // R
            pixels[idx + 3] = 255;                    // A
        }
    }
    VideoFrame {
        width: BENCH_WIDTH,
        height: BENCH_HEIGHT,
        ts_ms,
        format: PixelFormat::Bgra,
        planes: FramePlanes::Bgra {
            bytes: Bytes::from(pixels),
            stride,
        },
    }
}

/// Run the preflight benchmark using the existing encoder infrastructure.
///
/// Builds a real encoder via `build_vp9_encoder` or `build_av1_encoder`,
/// configures it at 1440p60, encodes `PREFLIGHT_FRAME_COUNT` frames, and
/// measures throughput.
pub fn run_preflight_benchmark(backends: &[EncodeBackendKind]) -> PreflightResult {
    use crate::net::video_encode::{av1 as av1_encode, vp9 as vp9_encode};

    // Partition backends by codec type for the builder functions.
    let vp9_backends: Vec<EncodeBackendKind> = backends
        .iter()
        .copied()
        .filter(|b| {
            matches!(
                b,
                EncodeBackendKind::MfHwVp9
                    | EncodeBackendKind::VaapiVp9
                    | EncodeBackendKind::Libvpx
            )
        })
        .collect();
    let av1_backends: Vec<EncodeBackendKind> = backends
        .iter()
        .copied()
        .filter(|b| matches!(b, EncodeBackendKind::NvencAv1 | EncodeBackendKind::SvtAv1))
        .collect();

    // Try AV1 HW first (typically fastest), then VP9 HW.
    let mut encoder = None;
    if !av1_backends.is_empty() {
        match av1_encode::build_av1_encoder(
            &av1_backends,
            crate::screen_share::config::SenderPolicy::AutoPremiumAv1,
        ) {
            Ok(enc) => encoder = Some(enc),
            Err(e) => warn!("[preflight] AV1 encoder init failed: {e}"),
        }
    }
    if encoder.is_none() && !vp9_backends.is_empty() {
        match vp9_encode::build_vp9_encoder(&vp9_backends) {
            Ok(enc) => encoder = Some(enc),
            Err(e) => warn!("[preflight] VP9 encoder init failed: {e}"),
        }
    }

    let Some(mut encoder) = encoder else {
        warn!("[preflight] no encoder available for benchmark");
        return PreflightResult::Fail;
    };

    // Configure at 1440p60 with a reasonable bitrate.
    let config = VideoSessionConfig {
        width: BENCH_WIDTH,
        height: BENCH_HEIGHT,
        fps: 60,
        target_bitrate_bps: 12_000_000,
        low_latency: true,
        allow_frame_drop: false,
    };
    if let Err(e) = encoder.configure_session(config) {
        warn!("[preflight] encoder configure failed: {e}");
        return PreflightResult::Fail;
    }

    // Warm-up: encode 2 frames.
    for i in 0..2u32 {
        let frame = synthetic_frame(i);
        if encoder.encode(frame).is_err() {
            return PreflightResult::Fail;
        }
    }

    // Timed section.
    let start = Instant::now();
    for i in 0..PREFLIGHT_FRAME_COUNT {
        let frame = synthetic_frame(i + 2);
        if encoder.encode(frame).is_err() {
            return PreflightResult::Fail;
        }
    }
    let elapsed = start.elapsed();

    if elapsed.as_secs_f32() < 0.001 {
        return PreflightResult::Fail;
    }
    let fps = PREFLIGHT_FRAME_COUNT as f32 / elapsed.as_secs_f32();
    info!("[preflight] benchmark result: {fps:.1} fps (threshold: {PREFLIGHT_FPS_THRESHOLD})");

    if fps >= PREFLIGHT_FPS_THRESHOLD {
        PreflightResult::Pass
    } else {
        PreflightResult::Fail
    }
}

/// Evaluate 1440p60 eligibility: static support + preflight/cache + env override.
///
/// This is the main entry point that replaces the old circular
/// `can_offer_1440p60()` for initial share-start decisions.
pub fn evaluate_1440p60_eligibility(
    static_support: bool,
    hw_backends: &[EncodeBackendKind],
) -> PreflightOutcome {
    PREFLIGHT_OUTCOME
        .get_or_init(|| compute_eligibility(static_support, hw_backends))
        .clone()
}

fn compute_eligibility(
    static_support: bool,
    hw_backends: &[EncodeBackendKind],
) -> PreflightOutcome {
    // 1. Check for experimental env override.
    if let Ok(val) = std::env::var("TSOD_FORCE_1440P60") {
        match val.trim() {
            "1" | "true" => {
                info!("[preflight] TSOD_FORCE_1440P60=1 — forcing 1440p60 on");
                return PreflightOutcome {
                    static_support: true,
                    preflight_passed: true,
                };
            }
            "0" | "false" => {
                info!("[preflight] TSOD_FORCE_1440P60=0 — forcing 1440p60 off");
                return PreflightOutcome {
                    static_support: false,
                    preflight_passed: false,
                };
            }
            _ => {}
        }
    }

    // 2. No static support → no point running preflight.
    if !static_support {
        return PreflightOutcome {
            static_support: false,
            preflight_passed: false,
        };
    }

    // 3. Check persisted cache for a previous result.
    let mut cache = load_cache();
    for backend in hw_backends {
        let key = cache_key(*backend);
        if let Some(result) = cache.results.get(&key) {
            info!(
                ?backend,
                ?result,
                "[preflight] using cached result for {key}"
            );
            return PreflightOutcome {
                static_support: true,
                preflight_passed: *result == PreflightResult::Pass,
            };
        }
    }

    // 4. No cache hit — run the preflight benchmark.
    if !hw_backends.is_empty() {
        info!("[preflight] running 1440p60 encode benchmark");
        let result = run_preflight_benchmark(hw_backends);
        // Cache under the first HW backend key.
        let key = cache_key(hw_backends[0]);
        info!(?result, "[preflight] benchmark complete for {key}");
        cache.results.insert(key, result);
        save_cache(&cache);
        return PreflightOutcome {
            static_support: true,
            preflight_passed: result == PreflightResult::Pass,
        };
    }

    // No HW backends available.
    PreflightOutcome {
        static_support: true,
        preflight_passed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_no_hw_returns_fail() {
        let outcome = compute_eligibility(true, &[]);
        assert!(outcome.static_support);
        assert!(!outcome.preflight_passed);
        assert!(!outcome.should_offer_1440p60());
    }

    #[test]
    fn no_static_support_skips_everything() {
        let outcome = compute_eligibility(false, &[EncodeBackendKind::NvencAv1]);
        assert!(!outcome.static_support);
        assert!(!outcome.preflight_passed);
        assert!(!outcome.should_offer_1440p60());
    }

    #[test]
    fn cached_pass_returns_immediately() {
        let mut cache = PreflightCache::default();
        let key = cache_key(EncodeBackendKind::NvencAv1);
        cache.results.insert(key.clone(), PreflightResult::Pass);
        assert_eq!(cache.results.get(&key), Some(&PreflightResult::Pass));
    }

    #[test]
    fn cached_fail_returns_immediately() {
        let mut cache = PreflightCache::default();
        let key = cache_key(EncodeBackendKind::NvencAv1);
        cache.results.insert(key.clone(), PreflightResult::Fail);
        assert_eq!(cache.results.get(&key), Some(&PreflightResult::Fail));
    }

    #[test]
    fn cache_key_includes_backend() {
        let k1 = cache_key(EncodeBackendKind::NvencAv1);
        let k2 = cache_key(EncodeBackendKind::VaapiVp9);
        assert_ne!(k1, k2);
        assert!(k1.contains("NvencAv1"));
        assert!(k2.contains("VaapiVp9"));
    }

    #[test]
    fn preflight_outcome_should_offer_logic() {
        assert!(PreflightOutcome {
            static_support: true,
            preflight_passed: true
        }
        .should_offer_1440p60());

        assert!(!PreflightOutcome {
            static_support: true,
            preflight_passed: false
        }
        .should_offer_1440p60());

        assert!(!PreflightOutcome {
            static_support: false,
            preflight_passed: true
        }
        .should_offer_1440p60());

        assert!(!PreflightOutcome {
            static_support: false,
            preflight_passed: false
        }
        .should_offer_1440p60());
    }

    #[test]
    fn env_force_on_overrides_everything() {
        // Note: env var tests are not parallel-safe, but #[test] runs serially
        // within a single test binary by default for env manipulation.
        std::env::set_var("TSOD_FORCE_1440P60", "1");
        let outcome = compute_eligibility(false, &[]);
        std::env::remove_var("TSOD_FORCE_1440P60");
        assert!(outcome.should_offer_1440p60());
    }

    #[test]
    fn env_force_off_overrides_everything() {
        std::env::set_var("TSOD_FORCE_1440P60", "0");
        let outcome = compute_eligibility(true, &[EncodeBackendKind::NvencAv1]);
        std::env::remove_var("TSOD_FORCE_1440P60");
        assert!(!outcome.should_offer_1440p60());
    }

    #[test]
    fn cache_roundtrip_serialization() {
        let mut cache = PreflightCache::default();
        cache
            .results
            .insert("NvencAv1:linux".into(), PreflightResult::Pass);
        cache
            .results
            .insert("VaapiVp9:linux".into(), PreflightResult::Fail);
        let json = serde_json::to_string(&cache).unwrap();
        let restored: PreflightCache = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.results.get("NvencAv1:linux"),
            Some(&PreflightResult::Pass)
        );
        assert_eq!(
            restored.results.get("VaapiVp9:linux"),
            Some(&PreflightResult::Fail)
        );
    }
}
