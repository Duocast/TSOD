use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

use anyhow::{anyhow, Context, Result};
use wasapi::{Direction, StreamMode};

use crate::media_audio_loopback::AudioLoopbackBackend;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const FRAME_SAMPLES_STEREO: usize = (SAMPLE_RATE as usize / 1000) * 20 * CHANNELS;

pub struct WasapiLoopback {
    queue: Arc<Mutex<VecDeque<i16>>>,
    stop: Arc<AtomicBool>,
    thread_failed: Arc<AtomicBool>,
    restart_count: Arc<AtomicU64>,
    underflow_count: Arc<AtomicU64>,
    stall_count: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WasapiLoopback {
    pub fn new() -> Result<Self> {
        Ok(Self {
            queue: Arc::new(Mutex::new(VecDeque::with_capacity(
                FRAME_SAMPLES_STEREO * 8,
            ))),
            stop: Arc::new(AtomicBool::new(false)),
            thread_failed: Arc::new(AtomicBool::new(false)),
            restart_count: Arc::new(AtomicU64::new(0)),
            underflow_count: Arc::new(AtomicU64::new(0)),
            stall_count: Arc::new(AtomicU64::new(0)),
            thread: None,
        })
    }

    fn run_capture(
        queue: Arc<Mutex<VecDeque<i16>>>,
        stop: Arc<AtomicBool>,
        thread_failed: Arc<AtomicBool>,
    ) -> Result<()> {
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
            if capture.read_from_device_to_deque(&mut packet).is_err() {
                thread_failed.store(true, Ordering::Relaxed);
                break;
            }
            while packet.len() >= CHANNELS * 4 {
                let mut frame = [0u8; CHANNELS * 4];
                for b in &mut frame {
                    *b = packet.pop_front().unwrap_or_default();
                }
                let l = f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
                let r = f32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
                if let Ok(mut q) = queue.lock() {
                    q.push_back((l * 32767.0).clamp(-32768.0, 32767.0) as i16);
                    q.push_back((r * 32767.0).clamp(-32768.0, 32767.0) as i16);
                    while q.len() > FRAME_SAMPLES_STEREO * 20 {
                        q.pop_front();
                    }
                }
            }
            let _ = event.wait_for_event(50_000);
        }
        let _ = audio_client.stop_stream();
        Ok(())
    }

    fn start_capture_thread(&mut self) -> Result<()> {
        let queue = self.queue.clone();
        let stop = self.stop.clone();
        let failed = self.thread_failed.clone();
        self.stop.store(false, Ordering::Relaxed);
        self.thread = Some(
            std::thread::Builder::new()
                .name("tsod-wasapi-loopback".into())
                .spawn(move || {
                    if let Err(e) = Self::run_capture(queue, stop, failed.clone()) {
                        failed.store(true, Ordering::Relaxed);
                        tracing::warn!(error=?e, "[audio-share] WASAPI loopback failed");
                    }
                })?,
        );
        Ok(())
    }

    fn recover_if_needed(&mut self) {
        if !self.thread_failed.load(Ordering::Relaxed) {
            return;
        }
        tracing::warn!("[audio-share] restarting WASAPI loopback after failure");
        self.thread_failed.store(false, Ordering::Relaxed);
        self.stop();
        self.restart_count.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self.start_capture_thread() {
            self.thread_failed.store(true, Ordering::Relaxed);
            tracing::warn!(error=?e, "[audio-share] failed to restart WASAPI loopback");
        }
    }
}

impl AudioLoopbackBackend for WasapiLoopback {
    fn backend_name(&self) -> &'static str {
        "wasapi-loopback"
    }

    fn channels(&self) -> usize {
        CHANNELS
    }

    fn start(&mut self) -> Result<()> {
        self.start_capture_thread()
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

    fn read_frame(&mut self, pcm: &mut [i16]) -> bool {
        if pcm.len() != FRAME_SAMPLES_STEREO {
            return false;
        }
        self.recover_if_needed();
        let Ok(mut q) = self.queue.lock() else {
            return false;
        };
        if q.len() < FRAME_SAMPLES_STEREO {
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
