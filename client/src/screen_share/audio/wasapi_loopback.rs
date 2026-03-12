use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use anyhow::{anyhow, Context, Result};
use wasapi::{Direction, StreamMode};

use crate::media_audio_loopback::AudioLoopbackBackend;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const FRAME_SAMPLES_MONO: usize = (SAMPLE_RATE as usize / 1000) * 20;

pub struct WasapiLoopback {
    queue: Arc<Mutex<VecDeque<i16>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WasapiLoopback {
    pub fn new() -> Result<Self> {
        Ok(Self {
            queue: Arc::new(Mutex::new(VecDeque::with_capacity(FRAME_SAMPLES_MONO * 8))),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        })
    }

    fn run_capture(queue: Arc<Mutex<VecDeque<i16>>>, stop: Arc<AtomicBool>) -> Result<()> {
        wasapi::initialize_mta()
            .ok()
            .map_err(|e| anyhow!("initialize COM MTA: {e}"))?;
        let enumerator = wasapi::DeviceEnumerator::new().context("create device enumerator")?;
        let device = enumerator
            .get_default_device(&Direction::Render)
            .context("get default render device")?;
        let mut audio_client = device
            .get_iaudioclient()
            .context("get render iaudioclient")?;
        let desired_format = wasapi::WaveFormat::new(
            32,
            32,
            &wasapi::SampleType::Float,
            SAMPLE_RATE as usize,
            CHANNELS,
            None,
        );
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: 200_000,
        };
        audio_client
            .initialize_client(&desired_format, &Direction::Render, &mode)
            .context("init WASAPI loopback stream")?;
        let event = audio_client
            .set_get_eventhandle()
            .context("set loopback event handle")?;
        let capture = audio_client
            .get_audiocaptureclient()
            .context("get loopback capture client")?;
        audio_client
            .start_stream()
            .context("start loopback stream")?;

        let mut packet = VecDeque::<u8>::new();
        while !stop.load(Ordering::Relaxed) {
            let _ = capture.read_from_device_to_deque(&mut packet);
            while packet.len() >= CHANNELS * 4 {
                let mut frame = [0u8; CHANNELS * 4];
                for b in &mut frame {
                    *b = packet.pop_front().unwrap_or_default();
                }
                let l = f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
                let r = f32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
                let mono = ((l + r) * 0.5 * 32767.0).clamp(-32768.0, 32767.0) as i16;
                if let Ok(mut q) = queue.lock() {
                    q.push_back(mono);
                    while q.len() > FRAME_SAMPLES_MONO * 20 {
                        q.pop_front();
                    }
                }
            }
            let _ = event.wait_for_event(50_000);
        }
        let _ = audio_client.stop_stream();
        Ok(())
    }
}

impl AudioLoopbackBackend for WasapiLoopback {
    fn backend_name(&self) -> &'static str {
        "wasapi-loopback"
    }

    fn start(&mut self) -> Result<()> {
        let queue = self.queue.clone();
        let stop = self.stop.clone();
        self.stop.store(false, Ordering::Relaxed);
        self.thread = Some(
            std::thread::Builder::new()
                .name("tsod-wasapi-loopback".into())
                .spawn(move || {
                    if let Err(e) = Self::run_capture(queue, stop) {
                        tracing::warn!(error=?e, "[audio-share] WASAPI loopback failed");
                    }
                })?,
        );
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        if let Ok(mut q) = self.queue.lock() {
            q.clear();
        }
    }

    fn read_frame(&self, pcm: &mut [i16]) -> bool {
        if pcm.len() != FRAME_SAMPLES_MONO {
            return false;
        }
        let Ok(mut q) = self.queue.lock() else {
            return false;
        };
        if q.len() < FRAME_SAMPLES_MONO {
            return false;
        }
        for s in pcm.iter_mut() {
            *s = q.pop_front().unwrap_or_default();
        }
        true
    }
}
