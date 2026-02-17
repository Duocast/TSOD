use anyhow::{anyhow, Context, Result};
use std::process::Command;
use tracing::debug;

#[derive(Clone, Debug)]
pub struct TcRunner {
    pub dry_run: bool,
}

impl TcRunner {
    pub fn new(dry_run: bool) -> Self {
        Self { dry_run }
    }

    pub fn tc(&self, args: &[&str]) -> Result<String> {
        debug!("tc {}", args.join(" "));
        if self.dry_run {
            return Ok(String::new());
        }

        let out = Command::new("tc")
            .args(args)
            .output()
            .with_context(|| format!("failed to exec tc {:?}", args))?;

        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            Err(anyhow!("tc {:?} failed: {}", args, stderr.trim()))
        }
    }

    pub fn ip(&self, args: &[&str]) -> Result<String> {
        debug!("ip {}", args.join(" "));
        if self.dry_run {
            return Ok(String::new());
        }

        let out = Command::new("ip")
            .args(args)
            .output()
            .with_context(|| format!("failed to exec ip {:?}", args))?;

        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            Err(anyhow!("ip {:?} failed: {}", args, stderr.trim()))
        }
    }
}
