use anyhow::{anyhow, Context, Result};
use cpal::{traits::DeviceTrait, traits::HostTrait, traits::StreamTrait};
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb, HeapCons,
};
use std::sync::{Arc, Mutex};

pub struct Capture {
    _stream: cpal::Stream,
    cons: Arc<Mutex<HeapCons<i16>>>,
    frame_samples: usize,
}

// Safety: cpal::Stream is Send but not Sync on some platforms due to internal
// raw pointers. We only access the ring buffer consumer from one thread at a
// time behind a Mutex, and the stream itself is never accessed after creation.
unsafe impl Send for Capture {}
unsafe impl Sync for Capture {}

impl Capture {
    pub fn start(sample_rate: u32, channels: u16, frame_ms: u32) -> Result<Self> {
        let host = cpal::default_host();
        let dev = host.default_input_device().ok_or(anyhow!("no input device"))?;

        let (stream_cfg, actual_channels) =
            compatible_input_config(&dev, sample_rate, channels)?;

        let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels as usize;

        let rb = HeapRb::<i16>::new(frame_samples * 50); // ~1s buffer
        let (prod, cons) = rb.split();
        let prod = Arc::new(Mutex::new(prod));
        let cons = Arc::new(Mutex::new(cons));

        let target_ch = channels;
        let stream = dev.build_input_stream(
            &stream_cfg,
            move |data: &[i16], _| {
                if let Ok(mut p) = prod.lock() {
                    if actual_channels == target_ch {
                        for &s in data {
                            let _ = p.try_push(s);
                        }
                    } else {
                        // Downmix: pick first channel from each interleaved frame
                        for chunk in data.chunks(actual_channels as usize) {
                            if let Some(&s) = chunk.first() {
                                let _ = p.try_push(s);
                            }
                        }
                    }
                }
            },
            move |err| {
                eprintln!("capture err: {err}");
            },
            None,
        )?;
        stream.play()?;

        Ok(Self { _stream: stream, cons, frame_samples })
    }

    pub fn read_frame(&self, out: &mut [i16]) -> bool {
        if out.len() != self.frame_samples {
            return false;
        }
        let mut got = 0usize;
        if let Ok(mut c) = self.cons.lock() {
            while got < out.len() {
                if let Some(v) = c.try_pop() {
                    out[got] = v;
                    got += 1;
                } else {
                    break;
                }
            }
        }
        got == out.len()
    }
}

/// Enumerate input device names.
pub fn enumerate_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devs| {
            devs.filter_map(|d| d.name().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Find a compatible input config, preferring the requested rate/channels
/// but falling back to the device's native capabilities.
fn compatible_input_config(
    dev: &cpal::Device,
    target_rate: u32,
    target_channels: u16,
) -> Result<(cpal::StreamConfig, u16)> {
    // Try exact match first
    if let Ok(ranges) = dev.supported_input_configs() {
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
    if let Ok(ranges) = dev.supported_input_configs() {
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
    let default = dev.default_input_config()
        .context("no supported input configuration")?;
    let ch = default.channels();
    Ok((default.config(), ch))
}
