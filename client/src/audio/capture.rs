use anyhow::{anyhow, Result};
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
        let cfg = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels as usize;

        let rb = HeapRb::<i16>::new(frame_samples * 50); // ~1s buffer
        let (prod, cons) = rb.split();
        let prod = Arc::new(Mutex::new(prod));
        let cons = Arc::new(Mutex::new(cons));

        let stream = dev.build_input_stream(
            &cfg,
            move |data: &[i16], _| {
                if let Ok(mut p) = prod.lock() {
                    for &s in data {
                        let _ = p.try_push(s);
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
