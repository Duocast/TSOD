use anyhow::{anyhow, Result};
use cpal::{traits::DeviceTrait, traits::HostTrait, traits::StreamTrait};
use ringbuf::HeapRb;
use std::sync::{Arc, Mutex};

pub struct Playout {
    _stream: cpal::Stream,
    rb: Arc<Mutex<HeapRb<i16>>>,
}

impl Playout {
    pub fn start(sample_rate: u32, channels: u16) -> Result<Self> {
        let host = cpal::default_host();
        let dev = host.default_output_device().ok_or(anyhow!("no output device"))?;
        let cfg = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let rb = Arc::new(Mutex::new(HeapRb::<i16>::new(sample_rate as usize * channels as usize))); // ~1s
        let rb2 = rb.clone();

        let stream = dev.build_output_stream(
            &cfg,
            move |out: &mut [i16], _| {
                if let Ok(mut rb) = rb2.lock() {
                    for o in out.iter_mut() {
                        *o = rb.pop().unwrap_or(0);
                    }
                }
            },
            move |err| {
                eprintln!("playout err: {err}");
            },
            None,
        )?;
        stream.play()?;
        Ok(Self { _stream: stream, rb })
    }

    pub fn push_pcm(&self, pcm: &[i16]) {
        if let Ok(mut rb) = self.rb.lock() {
            for &s in pcm {
                let _ = rb.push(s);
            }
        }
    }
}
