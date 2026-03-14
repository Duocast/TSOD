use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::media_audio_loopback::AudioLoopbackBackend;

const SAMPLE_RATE: u32 = 48_000;
const FRAME_MS: usize = 20;

pub struct PipeWireMonitor {
    queue: Arc<Mutex<VecDeque<i16>>>,
    stream: Option<cpal::Stream>,
    stream_failed: Arc<AtomicBool>,
    restart_count: Arc<AtomicU64>,
    underflow_count: Arc<AtomicU64>,
    stall_count: Arc<AtomicU64>,
    channels: usize,
}

impl PipeWireMonitor {
    pub fn new() -> Result<Self> {
        Ok(Self {
            queue: Arc::new(Mutex::new(VecDeque::with_capacity(
                (SAMPLE_RATE as usize / 1000) * FRAME_MS * 2 * 8,
            ))),
            stream: None,
            stream_failed: Arc::new(AtomicBool::new(false)),
            restart_count: Arc::new(AtomicU64::new(0)),
            underflow_count: Arc::new(AtomicU64::new(0)),
            stall_count: Arc::new(AtomicU64::new(0)),
            channels: 2,
        })
    }

    fn frame_samples(&self) -> usize {
        (SAMPLE_RATE as usize / 1000) * FRAME_MS * self.channels
    }

    fn monitor_rank(name: &str) -> Option<usize> {
        let lowered = name.to_ascii_lowercase();
        if lowered.contains("default") && lowered.contains("monitor") {
            Some(0)
        } else if lowered.contains("monitor") {
            Some(1)
        } else {
            None
        }
    }

    fn select_monitor_device(host: &cpal::Host) -> Result<cpal::Device> {
        let mut selected: Option<(usize, String, cpal::Device)> = None;
        for device in host
            .input_devices()
            .context("enumerate PipeWire input devices")?
        {
            let name = match device.description() {
                Ok(desc) => desc.to_string(),
                Err(_) => continue,
            };
            let Some(rank) = Self::monitor_rank(&name) else {
                continue;
            };
            match &selected {
                Some((best_rank, best_name, _))
                    if rank > *best_rank || (rank == *best_rank && name >= *best_name) => {}
                _ => selected = Some((rank, name, device)),
            }
        }

        let (_, name, device) =
            selected.ok_or_else(|| anyhow!("no PipeWire monitor input device was found"))?;
        tracing::info!(device=%name, "[audio-share] selected PipeWire monitor device");
        Ok(device)
    }

    fn start_stream(&mut self) -> Result<()> {
        let host = cpal::default_host();
        let device = Self::select_monitor_device(&host)?;

        let mut channel_count = 1u16;
        let mut best = 0u16;
        for cfg in device
            .supported_input_configs()
            .context("query PipeWire monitor input configs")?
        {
            if cfg.sample_format() == cpal::SampleFormat::F32
                && cfg.min_sample_rate() <= SAMPLE_RATE
                && cfg.max_sample_rate() >= SAMPLE_RATE
            {
                let c = cfg.channels();
                if c > best {
                    best = c;
                    channel_count = c;
                }
            }
        }
        if best == 0 {
            return Err(anyhow!("no 48kHz float monitor format supported"));
        }
        if channel_count > 2 {
            channel_count = 2;
        }
        self.channels = channel_count as usize;

        let config = cpal::StreamConfig {
            channels: channel_count,
            sample_rate: SAMPLE_RATE,
            buffer_size: cpal::BufferSize::Default,
        };

        let queue = self.queue.clone();
        let stream_failed = self.stream_failed.clone();
        let channels = self.channels;
        let frame_samples = self.frame_samples();
        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _| {
                if let Ok(mut q) = queue.lock() {
                    for frame in data.chunks_exact(channels) {
                        if channels >= 2 {
                            q.push_back((frame[0] * 32767.0).clamp(-32768.0, 32767.0) as i16);
                            q.push_back((frame[1] * 32767.0).clamp(-32768.0, 32767.0) as i16);
                        } else {
                            let m = (frame[0] * 32767.0).clamp(-32768.0, 32767.0) as i16;
                            q.push_back(m);
                        }
                    }
                    while q.len() > frame_samples * 20 {
                        q.pop_front();
                    }
                }
            },
            move |err| {
                stream_failed.store(true, Ordering::Relaxed);
                tracing::warn!(error=?err, "[audio-share] pipewire monitor stream error");
            },
            None,
        )?;
        stream.play().context("start PipeWire monitor stream")?;
        self.stream = Some(stream);
        self.stream_failed.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn recover_if_needed(&mut self) {
        if !self.stream_failed.load(Ordering::Relaxed) {
            return;
        }
        self.stream_failed.store(false, Ordering::Relaxed);
        self.stop();
        self.restart_count.fetch_add(1, Ordering::Relaxed);
        match self.start_stream() {
            Ok(()) => tracing::info!(
                restarts = self.restart_count.load(Ordering::Relaxed),
                "[audio-share] recovered PipeWire monitor stream"
            ),
            Err(error) => {
                self.stream_failed.store(true, Ordering::Relaxed);
                tracing::warn!(
                    ?error,
                    "[audio-share] failed to recover PipeWire monitor stream"
                );
            }
        }
    }
}

impl AudioLoopbackBackend for PipeWireMonitor {
    fn backend_name(&self) -> &'static str {
        "pipewire-monitor"
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn start(&mut self) -> Result<()> {
        self.start_stream()
    }

    fn stop(&mut self) {
        self.stream = None;
        if let Ok(mut q) = self.queue.lock() {
            q.clear();
        }
    }

    fn read_frame(&mut self, pcm: &mut [i16]) -> bool {
        self.recover_if_needed();
        if pcm.len() != self.frame_samples() {
            return false;
        }
        let Ok(mut q) = self.queue.lock() else {
            return false;
        };
        if q.len() < self.frame_samples() {
            self.underflow_count.fetch_add(1, Ordering::Relaxed);
            self.stall_count.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        for s in pcm.iter_mut() {
            *s = q.pop_front().unwrap_or_default();
        }
        true
    }
}
