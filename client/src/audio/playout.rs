use anyhow::{anyhow, Context, Result};
use cpal::{traits::DeviceTrait, traits::HostTrait, traits::StreamTrait};
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb, HeapProd,
};
use std::sync::{Arc, Mutex};

pub struct Playout {
    _stream: cpal::Stream,
    prod: Arc<Mutex<HeapProd<i16>>>,
}

// Safety: cpal::Stream is Send but not Sync on some platforms due to internal
// raw pointers. We only access the ring buffer producer from one thread at a
// time behind a Mutex, and the stream itself is never accessed after creation.
unsafe impl Send for Playout {}
unsafe impl Sync for Playout {}

impl Playout {
    pub fn start(sample_rate: u32, channels: u16) -> Result<Self> {
        let host = cpal::default_host();
        let dev = host.default_output_device().ok_or(anyhow!("no output device"))?;

        let (stream_cfg, actual_channels) =
            compatible_output_config(&dev, sample_rate, channels)?;

        let rb = HeapRb::<i16>::new(sample_rate as usize * channels as usize); // ~1s
        let (prod, cons) = rb.split();
        let prod = Arc::new(Mutex::new(prod));
        let cons = Arc::new(Mutex::new(cons));

        let target_ch = channels;
        let stream = dev.build_output_stream(
            &stream_cfg,
            move |out: &mut [i16], _| {
                if let Ok(mut c) = cons.lock() {
                    if actual_channels == target_ch {
                        for o in out.iter_mut() {
                            *o = c.try_pop().unwrap_or(0);
                        }
                    } else {
                        // Upmix: duplicate mono sample to all output channels
                        for frame in out.chunks_mut(actual_channels as usize) {
                            let sample = c.try_pop().unwrap_or(0);
                            for o in frame.iter_mut() {
                                *o = sample;
                            }
                        }
                    }
                }
            },
            move |err| {
                eprintln!("playout err: {err}");
            },
            None,
        )?;
        stream.play()?;
        Ok(Self { _stream: stream, prod })
    }

    pub fn push_pcm(&self, pcm: &[i16]) {
        if let Ok(mut p) = self.prod.lock() {
            for &s in pcm {
                let _ = p.try_push(s);
            }
        }
    }
}

/// Enumerate output device names.
pub fn enumerate_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|devs| {
            devs.filter_map(|d| d.name().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Find a compatible output config, preferring the requested rate/channels
/// but falling back to the device's native capabilities.
fn compatible_output_config(
    dev: &cpal::Device,
    target_rate: u32,
    target_channels: u16,
) -> Result<(cpal::StreamConfig, u16)> {
    // Try exact match first
    if let Ok(ranges) = dev.supported_output_configs() {
        for range in ranges {
            if range.channels() == target_channels
                && range.min_sample_rate().0 <= target_rate
                && range.max_sample_rate().0 >= target_rate
            {
                return Ok((
                    cpal::StreamConfig {
                        channels: target_channels,
                        sample_rate: cpal::SampleRate(target_rate),
                        buffer_size: cpal::BufferSize::Default,
                    },
                    target_channels,
                ));
            }
        }
    }

    // Try any channel count at our sample rate
    if let Ok(ranges) = dev.supported_output_configs() {
        for range in ranges {
            if range.min_sample_rate().0 <= target_rate
                && range.max_sample_rate().0 >= target_rate
            {
                let ch = range.channels();
                return Ok((
                    cpal::StreamConfig {
                        channels: ch,
                        sample_rate: cpal::SampleRate(target_rate),
                        buffer_size: cpal::BufferSize::Default,
                    },
                    ch,
                ));
            }
        }
    }

    // Last resort: device default config
    let default = dev.default_output_config()
        .context("no supported output configuration")?;
    let ch = default.channels();
    Ok((default.config(), ch))
}
