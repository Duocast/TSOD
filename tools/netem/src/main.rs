use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use tracing::{info, Level};
use tracing_subscriber::EnvFilter;

mod tc;
mod validate;

use tc::TcRunner;

#[derive(Parser, Debug)]
#[command(name="vp-netem", about="Linux tc netem injector (loss/jitter/delay)")]
struct Args {
    /// Network interface (e.g. eth0, lo)
    #[arg(long)]
    iface: String,

    /// Also apply to ingress using IFB redirect (requires `modprobe ifb` capability)
    #[arg(long, default_value_t=false)]
    ingress: bool,

    /// IFB device name used for ingress shaping
    #[arg(long, default_value="ifb0")]
    ifb: String,

    /// Do not execute tc/ip; just print actions
    #[arg(long, default_value_t=false)]
    dry_run: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Apply/replace netem qdisc
    Apply {
        /// Base delay, e.g. "20ms"
        #[arg(long, default_value="0ms")]
        delay: String,

        /// Jitter, e.g. "5ms" (requires delay > 0)
        #[arg(long, default_value="0ms")]
        jitter: String,

        /// Packet loss percent (0..100), e.g. 1.5
        #[arg(long, default_value_t=0.0)]
        loss: f32,

        /// Packet duplication percent (0..100)
        #[arg(long, default_value_t=0.0)]
        duplicate: f32,

        /// Packet reordering percent (0..100)
        #[arg(long, default_value_t=0.0)]
        reorder: f32,

        /// Correlation percent for reorder (0..100), used by tc netem
        #[arg(long, default_value_t=0.0)]
        reorder_corr: f32,

        /// Optional distribution: normal, pareto, paretonormal
        #[arg(long)]
        distribution: Option<String>,
    },

    /// Show current qdisc state
    Show,

    /// Clear root qdisc (and ingress if configured)
    Clear,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .init();

    validate::require_linux()?;

    let args = Args::parse();
    let tc = TcRunner::new(args.dry_run);

    match args.cmd {
        Cmd::Apply { delay, jitter, loss, duplicate, reorder, reorder_corr, distribution } => {
            validate::pct_0_100(loss, "loss")?;
            validate::pct_0_100(duplicate, "duplicate")?;
            validate::pct_0_100(reorder, "reorder")?;
            validate::pct_0_100(reorder_corr, "reorder_corr")?;

            let delay_dur = humantime::parse_duration(&delay).context("parse --delay")?;
            let jitter_dur = humantime::parse_duration(&jitter).context("parse --jitter")?;
            if jitter_dur.as_millis() > 0 && delay_dur.as_millis() == 0 {
                return Err(anyhow!("--jitter requires --delay > 0"));
            }

            apply_egress(&tc, &args.iface, delay_dur, jitter_dur, loss, duplicate, reorder, reorder_corr, distribution.as_deref())?;

            if args.ingress {
                apply_ingress(&tc, &args.iface, &args.ifb, delay_dur, jitter_dur, loss, duplicate, reorder, reorder_corr, distribution.as_deref())?;
            }

            info!("netem applied");
        }
        Cmd::Show => {
            show(&tc, &args.iface)?;
            if args.ingress {
                show(&tc, &args.ifb)?;
            }
        }
        Cmd::Clear => {
            clear_egress(&tc, &args.iface)?;
            if args.ingress {
                clear_ingress(&tc, &args.iface, &args.ifb)?;
            }
            info!("netem cleared");
        }
    }

    Ok(())
}

fn apply_egress(
    tc: &TcRunner,
    iface: &str,
    delay: std::time::Duration,
    jitter: std::time::Duration,
    loss: f32,
    duplicate: f32,
    reorder: f32,
    reorder_corr: f32,
    distribution: Option<&str>,
) -> Result<()> {
    // Use "qdisc replace" for idempotency.
    let mut args: Vec<String> = vec![
        "qdisc".into(), "replace".into(),
        "dev".into(), iface.into(),
        "root".into(),
        "handle".into(), "1:".into(),
        "netem".into(),
    ];

    if delay.as_millis() > 0 {
        args.push("delay".into());
        args.push(format!("{}ms", delay.as_millis()));
        if jitter.as_millis() > 0 {
            args.push(format!("{}ms", jitter.as_millis()));
            if let Some(d) = distribution {
                args.push("distribution".into());
                args.push(d.into());
            }
        }
    }

    if loss > 0.0 {
        args.push("loss".into());
        args.push(format!("{loss}%"));
    }
    if duplicate > 0.0 {
        args.push("duplicate".into());
        args.push(format!("{duplicate}%"));
    }
    if reorder > 0.0 {
        args.push("reorder".into());
        args.push(format!("{reorder}%"));
        if reorder_corr > 0.0 {
            args.push(format!("{reorder_corr}%"));
        }
    }

    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    tc.tc(&refs)?;
    Ok(())
}

fn apply_ingress(
    tc: &TcRunner,
    iface: &str,
    ifb: &str,
    delay: std::time::Duration,
    jitter: std::time::Duration,
    loss: f32,
    duplicate: f32,
    reorder: f32,
    reorder_corr: f32,
    distribution: Option<&str>,
) -> Result<()> {
    // Ensure IFB exists and up
    tc.ip(&["link", "add", ifb, "type", "ifb"]).ok(); // may already exist
    tc.ip(&["link", "set", "dev", ifb, "up"])?;

    // Add ingress qdisc on iface
    tc.tc(&["qdisc", "replace", "dev", iface, "handle", "ffff:", "ingress"])?;

    // Redirect all ingress traffic to IFB
    // Requires act_mirred kernel module usually available.
    tc.tc(&[
        "filter","replace","dev",iface,"parent","ffff:","protocol","all","prio","1",
        "matchall","action","mirred","egress","redirect","dev",ifb
    ])?;

    // Apply netem on IFB egress root
    apply_egress(tc, ifb, delay, jitter, loss, duplicate, reorder, reorder_corr, distribution)?;
    Ok(())
}

fn show(tc: &TcRunner, iface: &str) -> Result<()> {
    let out = tc.tc(&["qdisc", "show", "dev", iface])?;
    if out.trim().is_empty() {
        info!("qdisc dev {}: <none>", iface);
    } else {
        info!("qdisc dev {}:\n{}", iface, out.trim_end());
    }
    Ok(())
}

fn clear_egress(tc: &TcRunner, iface: &str) -> Result<()> {
    // Deleting a non-existent qdisc errors; ignore.
    let _ = tc.tc(&["qdisc", "del", "dev", iface, "root"]);
    Ok(())
}

fn clear_ingress(tc: &TcRunner, iface: &str, ifb: &str) -> Result<()> {
    let _ = tc.tc(&["qdisc", "del", "dev", iface, "ingress"]);
    let _ = tc.tc(&["qdisc", "del", "dev", ifb, "root"]);
    // keep IFB device; it may be shared
    Ok(())
}
