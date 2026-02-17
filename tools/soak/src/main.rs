use anyhow::{Context, Result};
use clap::Parser;
use std::{sync::Arc, time::{Duration, Instant}};
use tokio::{sync::Mutex, time::sleep};
use tracing::{info, warn, Level};
use tracing_subscriber::EnvFilter;

mod stats;
mod tls;
mod quic_client;

use stats::{SoakReport, dur_ms, quantiles_ms};

pub mod pb {
    pub mod voiceplatform {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/voiceplatform.v1.rs"));
        }
    }
}
use pb::voiceplatform::v1 as pb;

#[derive(Parser, Debug, Clone)]
#[command(name="vp-soak", about="QUIC connect/disconnect soak tester")]
struct Args {
    #[arg(long, default_value="127.0.0.1:4433")]
    server: String,

    /// ServerName for TLS SNI (often "localhost" in dev)
    #[arg(long, default_value="localhost")]
    server_name: String,

    /// Bind address for client endpoint (usually "[::]:0")
    #[arg(long, default_value="[::]:0")]
    bind: String,

    #[arg(long, default_value="vp-control/1")]
    alpn: String,

    #[arg(long, default_value="dev")]
    dev_token: String,

    #[arg(long)]
    join_channel: Option<String>,

    /// Run for N iterations per worker
    #[arg(long)]
    iterations: Option<u64>,

    /// Run for duration seconds (overrides iterations if set)
    #[arg(long)]
    duration_secs: Option<u64>,

    /// Number of concurrent workers
    #[arg(long, default_value_t=8)]
    concurrency: usize,

    /// Hold time between connect and disconnect (min ms)
    #[arg(long, default_value_t=100)]
    hold_min_ms: u64,

    /// Hold time (max ms)
    #[arg(long, default_value_t=800)]
    hold_max_ms: u64,

    /// Ping every N seconds while connected (0 disables)
    #[arg(long, default_value_t=2)]
    ping_every_secs: u64,

    /// Connect timeout seconds
    #[arg(long, default_value_t=5)]
    connect_timeout_secs: u64,

    /// TLS pin (sha256 hex of leaf cert DER); also reads VP_TLS_PIN_SHA256_HEX
    #[arg(long)]
    pin_sha256_hex: Option<String>,

    /// Allow insecure TLS (accept any cert) explicitly
    #[arg(long, default_value_t=false)]
    insecure: bool,

    /// Write JSON report to this path
    #[arg(long)]
    report_json: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .init();

    let args = Args::parse();
    let pin = args.pin_sha256_hex.clone().or_else(|| std::env::var("VP_TLS_PIN_SHA256_HEX").ok());

    let (endpoint, server_name) = tls::make_endpoint(&args.bind, &args.server_name, pin, args.insecure)?;

    let stop_at = args.duration_secs.map(|s| Instant::now() + Duration::from_secs(s));

    let report = Arc::new(Mutex::new(SoakReport::default()));
    let connect_samples = Arc::new(Mutex::new(Vec::<u64>::new()));
    let auth_samples = Arc::new(Mutex::new(Vec::<u64>::new()));

    let mut handles = vec![];

    for worker_id in 0..args.concurrency {
        let args = args.clone();
        let endpoint = endpoint.clone();
        let server_name = server_name.clone();
        let report = report.clone();
        let connect_samples = connect_samples.clone();
        let auth_samples = auth_samples.clone();

        handles.push(tokio::spawn(async move {
            worker_loop(worker_id, args, endpoint, server_name, stop_at, report, connect_samples, auth_samples).await
        }));
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received; stopping");
        }
        _ = async {
            for h in handles {
                let _ = h.await;
            }
        } => {}
    }

    // finalize stats
    let mut rep = report.lock().await.clone();
    {
        let mut c = connect_samples.lock().await;
        let (p50, p95) = quantiles_ms(&mut c);
        rep.timings.connect_ms_p50 = p50;
        rep.timings.connect_ms_p95 = p95;
    }
    {
        let mut a = auth_samples.lock().await;
        let (p50, p95) = quantiles_ms(&mut a);
        rep.timings.auth_ms_p50 = p50;
        rep.timings.auth_ms_p95 = p95;
    }

    info!("report: {}", serde_json::to_string_pretty(&rep)?);

    if let Some(path) = args.report_json.as_deref() {
        std::fs::write(path, serde_json::to_vec_pretty(&rep)?)?;
        info!("wrote {}", path);
    }

    Ok(())
}

async fn worker_loop(
    worker_id: usize,
    args: Args,
    endpoint: quinn::Endpoint,
    server_name: rustls::pki_types::ServerName<'static>,
    stop_at: Option<Instant>,
    report: Arc<Mutex<SoakReport>>,
    connect_samples: Arc<Mutex<Vec<u64>>>,
    auth_samples: Arc<Mutex<Vec<u64>>>,
) -> Result<()> {
    let addr = args.server.parse().context("parse server addr")?;
    let connect_timeout = Duration::from_secs(args.connect_timeout_secs);

    let mut iter: u64 = 0;
    loop {
        if let Some(stop) = stop_at {
            if Instant::now() >= stop {
                break;
            }
        }
        if let Some(max) = args.iterations {
            if iter >= max {
                break;
            }
        }
        iter += 1;

        // connect
        let t0 = Instant::now();
        let connecting = endpoint.connect(addr, server_name.clone()).context("connect start")?;
        let conn = match tokio::time::timeout(connect_timeout, connecting).await {
            Ok(Ok(c)) => {
                report.lock().await.counters.connect_ok += 1;
                connect_samples.lock().await.push(dur_ms(t0.elapsed()));
                c
            }
            Ok(Err(e)) => {
                report.lock().await.counters.connect_err += 1;
                warn!("[w{}] connect err: {}", worker_id, e);
                continue;
            }
            Err(_) => {
                report.lock().await.counters.connect_err += 1;
                warn!("[w{}] connect timeout", worker_id);
                continue;
            }
        };

        // control stream
        let (send, recv) = match conn.open_bi().await {
            Ok(v) => v,
            Err(e) => {
                report.lock().await.counters.connect_err += 1;
                warn!("[w{}] open_bi err: {}", worker_id, e);
                continue;
            }
        };
        let mut ctrl = quic_client::Ctrl::new(send, recv);

        // auth
        let t1 = Instant::now();
        match ctrl.hello_auth(&args.alpn, &args.dev_token).await {
            Ok(()) => {
                report.lock().await.counters.auth_ok += 1;
                auth_samples.lock().await.push(dur_ms(t1.elapsed()));
            }
            Err(e) => {
                report.lock().await.counters.auth_err += 1;
                warn!("[w{}] auth err: {}", worker_id, e);
                continue;
            }
        }

        // optional join
        if let Some(ch) = args.join_channel.as_deref() {
            match ctrl.join(ch).await {
                Ok(()) => report.lock().await.counters.join_ok += 1,
                Err(e) => {
                    report.lock().await.counters.join_err += 1;
                    warn!("[w{}] join err: {}", worker_id, e);
                }
            }
        }

        // hold with optional ping
        let hold = rand_range_ms(args.hold_min_ms, args.hold_max_ms);
        let hold_dur = Duration::from_millis(hold);

        if args.ping_every_secs == 0 {
            sleep(hold_dur).await;
        } else {
            let ping_every = Duration::from_secs(args.ping_every_secs);
            let end = Instant::now() + hold_dur;
            while Instant::now() < end {
                match ctrl.ping().await {
                    Ok(()) => report.lock().await.counters.ping_ok += 1,
                    Err(_) => {
                        report.lock().await.counters.ping_err += 1;
                        break;
                    }
                }
                sleep(ping_every).await;
            }
        }

        // disconnect
        conn.close(0u32.into(), b"soak");
        report.lock().await.counters.sessions_completed += 1;
    }

    Ok(())
}

fn rand_range_ms(min: u64, max: u64) -> u64 {
    if max <= min { return min; }
    let r: u64 = rand::random();
    min + (r % (max - min + 1))
}
