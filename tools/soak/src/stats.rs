use serde::Serialize;
use std::time::Duration;

#[derive(Default, Serialize, Clone)]
pub struct Counters {
    pub connect_ok: u64,
    pub connect_err: u64,
    pub auth_ok: u64,
    pub auth_err: u64,
    pub join_ok: u64,
    pub join_err: u64,
    pub ping_ok: u64,
    pub ping_err: u64,
    pub sessions_completed: u64,
}

#[derive(Default, Serialize, Clone)]
pub struct Timings {
    pub connect_ms_p50: u64,
    pub connect_ms_p95: u64,
    pub auth_ms_p50: u64,
    pub auth_ms_p95: u64,
}

#[derive(Default, Serialize, Clone)]
pub struct SoakReport {
    pub counters: Counters,
    pub timings: Timings,
}

pub fn quantiles_ms(samples: &mut Vec<u64>) -> (u64, u64) {
    if samples.is_empty() {
        return (0, 0);
    }
    samples.sort_unstable();
    let p50 = samples[(samples.len() * 50) / 100];
    let p95 = samples[(samples.len() * 95) / 100];
    (p50, p95)
}

pub fn dur_ms(d: Duration) -> u64 {
    d.as_millis() as u64
}
