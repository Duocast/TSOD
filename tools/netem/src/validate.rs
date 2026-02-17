use anyhow::{anyhow, Result};

pub fn pct_0_100(v: f32, name: &'static str) -> Result<f32> {
    if (0.0..=100.0).contains(&v) {
        Ok(v)
    } else {
        Err(anyhow!("{name} must be 0..=100, got {v}"))
    }
}

pub fn nonneg_ms(v: u64, name: &'static str) -> Result<u64> {
    Ok(v) // u64 already nonnegative
}

pub fn require_linux() -> Result<()> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err(anyhow!("netem tool is Linux-only (requires `tc`)"))
    }
}
