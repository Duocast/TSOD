use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::media_audio_loopback::AudioLoopbackBackend;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 1;
const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * 20;

pub struct PipeWireMonitor {
    queue: Arc<Mutex<VecDeque<i16>>>,
    stream: Option<cpal::Stream>,
}

impl PipeWireMonitor {
    pub fn new() -> Result<Self> {
        Ok(Self {
            queue: Arc::new(Mutex::new(VecDeque::with_capacity(FRAME_SAMPLES * 8))),
            stream: None,
        })
    }

    fn select_monitor_device(host: &cpal::Host) -> Result<cpal::Device> {
        let mut devices = host
            .input_devices()
            .context("enumerate PipeWire input devices")?;
        devices
            .find(|d| {
                d.name()
                    .map(|n| n.to_ascii_lowercase().contains("monitor"))
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow!("no PipeWire monitor input device was found"))
    }
}

impl AudioLoopbackBackend for PipeWireMonitor {
    fn backend_name(&self) -> &'static str {
        "pipewire-monitor"
    }

    fn start(&mut self) -> Result<()> {
        let host = cpal::available_hosts()
            .into_iter()
            .find(|id| *id == cpal::HostId::PipeWire)
            .map(cpal::host_from_id)
            .transpose()?
            .unwrap_or_else(cpal::default_host);
        let device = Self::select_monitor_device(&host)?;
        let supported = device
            .supported_input_configs()
            .context("query monitor input configs")?
            .find(|cfg| {
                cfg.sample_format() == cpal::SampleFormat::F32
                    && cfg.min_sample_rate().0 <= SAMPLE_RATE
                    && cfg.max_sample_rate().0 >= SAMPLE_RATE
                    && cfg.channels() >= 1
            })
            .ok_or_else(|| anyhow!("no 48kHz float monitor format supported"))?;
        let config = cpal::StreamConfig {
            channels: CHANNELS,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };
        let queue = self.queue.clone();
        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _| {
                if let Ok(mut q) = queue.lock() {
                    for &s in data {
                        q.push_back((s * 32767.0).clamp(-32768.0, 32767.0) as i16);
                    }
                    while q.len() > FRAME_SAMPLES * 20 {
                        q.pop_front();
                    }
                }
            },
            move |err| {
                tracing::warn!(error=?err, "[audio-share] pipewire monitor stream error");
            },
            None,
        )?;
        stream.play().context("start PipeWire monitor stream")?;
        self.stream = Some(stream);
        let _ = supported;
        Ok(())
    }

    fn stop(&mut self) {
        self.stream = None;
        if let Ok(mut q) = self.queue.lock() {
            q.clear();
        }
    }

    fn read_frame(&self, pcm: &mut [i16]) -> bool {
        if pcm.len() != FRAME_SAMPLES {
            return false;
        }
        let Ok(mut q) = self.queue.lock() else {
            return false;
        };
        if q.len() < FRAME_SAMPLES {
            return false;
        }
        for s in pcm.iter_mut() {
            *s = q.pop_front().unwrap_or_default();
        }
        true
    }
}
