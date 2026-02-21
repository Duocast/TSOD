use anyhow::{anyhow, Result};
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
        let cfg = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let rb = HeapRb::<i16>::new(sample_rate as usize * channels as usize); // ~1s
        let (prod, cons) = rb.split();
        let prod = Arc::new(Mutex::new(prod));
        let cons = Arc::new(Mutex::new(cons));

        let stream = dev.build_output_stream(
            &cfg,
            move |out: &mut [i16], _| {
                if let Ok(mut c) = cons.lock() {
                    for o in out.iter_mut() {
                        *o = c.try_pop().unwrap_or(0);
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
