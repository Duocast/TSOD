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

pub fn distribution(v: Option<&str>) -> Result<()> {
    match v {
        None => Ok(()),
        Some("normal" | "pareto" | "paretonormal") => Ok(()),
        Some(other) => Err(anyhow!(
            "distribution must be one of: normal, pareto, paretonormal; got {other}"
        )),
    }
}

pub fn require_linux() -> Result<()> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err(anyhow!("netem tool is Linux-only (requires `tc`)"))
    }
}

#[cfg(test)]
mod tests {
    use super::distribution;

    #[test]
    fn distribution_accepts_supported_values() {
        assert!(distribution(None).is_ok());
        assert!(distribution(Some("normal")).is_ok());
        assert!(distribution(Some("pareto")).is_ok());
        assert!(distribution(Some("paretonormal")).is_ok());
    }

    #[test]
    fn distribution_rejects_unknown_values() {
        let err = distribution(Some("gaussian")).expect_err("invalid distribution must fail");
        assert!(err
            .to_string()
            .contains("distribution must be one of: normal, pareto, paretonormal"));
    }
}
