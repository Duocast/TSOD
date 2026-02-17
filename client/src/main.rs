mod app;
mod config;
mod net;
mod proto;
mod audio;

use anyhow::Result;
use clap::Parser;
use config::Config;
use net::{control::ControlClient, quic, voice_datagram::make_voice_datagram};
use tracing::{info, Level};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .init();

    let cfg = Config::parse();

    let endpoint = quic::make_endpoint()?;
    let addr = cfg.server.parse()?;
    let conn = endpoint
        .connect(addr, quic::server_name())?
        .await?;

    info!("connected");

    // Control stream
    let (send, recv) = conn.open_bi().await?;
    let mut ctl = ControlClient::new(send, recv);
    ctl.hello_and_auth(&cfg.alpn, &cfg.dev_token).await?;
    info!("authed");

    if let Some(ch) = cfg.channel_id.as_deref() {
        ctl.join_channel(ch).await?;
        info!("joined channel {}", ch);
    }

    // Audio setup (simple mono 48k, 20ms frames)
    let sample_rate = 48_000u32;
    let channels = 1u16;
    let frame_ms = 20u32;
    let mut codec = audio::opus::OpusCodec::new(sample_rate, channels as u8)?;
    let capture = audio::capture::Capture::start(sample_rate, channels, frame_ms)?;
    let _playout = audio::playout::Playout::start(sample_rate, channels)?;

    // Voice send loop
    let mut seq: u32 = 0;
    let ssrc: u32 = rand::random();
    let channel_route_hash: u32 = 0x12345678; // TODO: compute from ChannelId UUID in spec

    let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels as usize;
    let mut pcm = vec![0i16; frame_samples];
    let mut enc_out = vec![0u8; 4000];

    loop {
        // Minimal keepalive ping
        let _ = ctl.ping().await;

        if capture.read_frame(&mut pcm) {
            let n = codec.encode(&pcm, &mut enc_out)?;
            let ts_ms = (unix_ms() & 0xFFFF_FFFF) as u32;

            let d = make_voice_datagram(
                channel_route_hash,
                ssrc,
                seq,
                ts_ms,
                true,
                &enc_out[..n],
            );
            seq = seq.wrapping_add(1);

            // QUIC DATAGRAM send
            if let Err(e) = conn.send_datagram(d) {
                info!("send_datagram error: {}", e);
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

fn unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
