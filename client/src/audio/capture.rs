use anyhow::{anyhow, Result};
use cpal::{traits::DeviceTrait, traits::HostTrait, traits::StreamTrait};
use ringbuf::HeapRb;
use std::sync::{Arc, Mutex};

pub struct Capture {
    _stream: cpal::Stream,
    rb: Arc<Mutex<HeapRb<i16>>>,
    frame_samples: usize,
}

impl Capture {
    pub fn start(sample_rate: u32, channels: u16, frame_ms: u32) -> Result<Self> {
        let host = cpal::default_host();
        let dev = host.default_input_device().ok_or(anyhow!("no input device"))?;
        let cfg = dev.default_input_config()?;
        let cfg = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels as usize;

        let rb = Arc::new(Mutex::new(HeapRb::<i16>::new(frame_samples * 50))); // ~1s buffer
        let rb2 = rb.clone();

        let stream = dev.build_input_stream(
            &cfg,
            move |data: &[i16], _| {
                if let Ok(mut rb) = rb2.lock() {
                    for &s in data {
                        let _ = rb.push(s);
                    }
                }
            },
            move |err| {
                eprintln!("capture err: {err}");
            },
            None,
        )?;
        stream.play()?;

        Ok(Self { _stream: stream, rb, frame_samples })
    }

    pub fn read_frame(&self, out: &mut [i16]) -> bool {
        if out.len() != self.frame_samples {
            return false;
        }
        let mut got = 0usize;
        if let Ok(mut rb) = self.rb.lock() {
            while got < out.len() {
                if let Some(v) = rb.pop() {
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
