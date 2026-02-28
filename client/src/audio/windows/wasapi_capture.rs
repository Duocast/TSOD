use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use ringbuf::{traits::Producer, HeapProd};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tracing::{debug, error, info};
use wasapi::{Direction, SampleType, StreamMode};

use crate::audio::resample::LinearResampler;
use crate::ui::{
    model::{AudioBackend, AudioDeviceId, AudioDeviceInfo, AudioDirection},
    UiEvent,
};

use super::wasapi_common::{default_endpoint_id, enumerate_endpoints, open_device, ComGuard};

pub struct WasapiCapture {
    thread: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    unhealthy: Arc<AtomicBool>,
}

impl WasapiCapture {
    pub fn start(
        sample_rate: u32,
        channels: u16,
        prod: HeapProd<i16>,
        preferred_device: Option<&str>,
        tx_event: Option<Sender<UiEvent>>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let unhealthy = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let unhealthy_thread = unhealthy.clone();
        let preferred_device = preferred_device.map(str::to_string);
        let tx_event_thread = tx_event.clone();
        let reported = Arc::new(AtomicBool::new(false));
        let reported_thread = reported.clone();

        let thread = std::thread::Builder::new()
            .name("tsod-wasapi-capture".to_string())
            .spawn(move || {
                if let Err(error) = run_capture_thread(
                    sample_rate,
                    channels,
                    prod,
                    preferred_device.as_deref(),
                    &stop_thread,
                ) {
                    error!("[wasapi capture] thread failed: {error:#}");
                    unhealthy_thread.store(true, Ordering::Relaxed);
                    if reported_thread
                        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        if let Some(tx) = &tx_event_thread {
                            let _ = tx.send(UiEvent::AppendLog(format!(
                                "[audio] wasapi capture thread failed: {error:#}"
                            )));
                        }
                    }
                }
            })
            .context("spawn WASAPI capture thread")?;

        Ok(Self {
            thread: Some(thread),
            stop,
            unhealthy,
        })
    }

    pub fn enumerate_input_devices() -> Vec<AudioDeviceInfo> {
        let default_id = default_endpoint_id(Direction::Capture);
        let endpoints = match enumerate_endpoints(Direction::Capture) {
            Ok(values) => values,
            Err(error) => {
                error!("[wasapi capture] enumerate endpoints failed: {error:#}");
                return Vec::new();
            }
        };

        tracing::debug!(
            count = endpoints.len(),
            "[wasapi capture] enumerated input endpoints"
        );
        for (id, friendly) in endpoints.iter().take(4) {
            tracing::debug!(endpoint_id = %id, friendly_name = %friendly, "[wasapi capture] input endpoint");
        }

        endpoints
            .into_iter()
            .map(|(endpoint_id, friendly_name)| AudioDeviceInfo {
                key: AudioDeviceId {
                    backend: AudioBackend::Wasapi,
                    direction: AudioDirection::Input,
                    id: endpoint_id.clone(),
                },
                label: friendly_name.clone(),
                display_label: friendly_name,
                is_default: default_id.as_deref() == Some(endpoint_id.as_str()),
            })
            .collect()
    }

    pub fn is_healthy(&self) -> bool {
        !self.unhealthy.load(Ordering::Relaxed)
    }
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_capture_thread(
    sample_rate: u32,
    channels: u16,
    mut prod: HeapProd<i16>,
    preferred_device: Option<&str>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let _com = ComGuard::new()?;
    let device = open_device(Direction::Capture, preferred_device)?;
    let endpoint_id = device
        .get_id()
        .unwrap_or_else(|_| "<unknown-id>".to_string());
    let friendly_name = device
        .get_friendlyname()
        .or_else(|_| device.get_description())
        .unwrap_or_else(|_| "Unknown device".to_string());

    let mut audio_client = device
        .get_iaudioclient()
        .context("get WASAPI audio client")?;
    let mix = audio_client
        .get_mixformat()
        .context("get WASAPI mix format")?;
    let device_rate = mix.get_samplespersec();
    let device_channels = mix.get_nchannels().max(1) as usize;
    let block_align = mix.get_blockalign() as usize;
    let bits_per_sample = mix.get_bitspersample() as u16;
    let valid_bits = mix.get_validbitspersample() as u16;
    let sample_type = mix.get_subformat().context("get WASAPI sample type")?;
    let bytes_per_sample = block_align / device_channels;

    info!(
        "[wasapi capture] open device id={} name={} mix: {}Hz {}ch block_align={} bits={} valid={} type={:?}",
        endpoint_id,
        friendly_name,
        device_rate,
        device_channels,
        block_align,
        bits_per_sample,
        valid_bits,
        sample_type
    );
    if bytes_per_sample == 3 || valid_bits == 24 {
        debug!(
            "[wasapi capture] 24-bit path active: bytes_per_sample={} valid_bits={} block_align={} channels={}",
            bytes_per_sample,
            valid_bits,
            block_align,
            device_channels
        );
    }

    let mode = StreamMode::EventsShared {
        autoconvert: false,
        buffer_duration_hns: 200_000,
    };
    audio_client
        .initialize_client(&mix, &Direction::Capture, &mode)
        .map_err(|error| {
            tracing::error!("[wasapi capture] initialize_client failed: {error:#}");
            error
        })
        .context("initialize WASAPI shared capture stream")?;
    let handle = audio_client
        .set_get_eventhandle()
        .context("set WASAPI capture event handle")?;
    let capture = audio_client
        .get_audiocaptureclient()
        .context("get WASAPI capture client")?;
    audio_client
        .start_stream()
        .map_err(|error| {
            tracing::error!("[wasapi capture] start_stream failed: {error:#}");
            error
        })
        .context("start WASAPI capture stream")?;

    let mut resampler = LinearResampler::new(device_rate, sample_rate);
    let target_channels = channels.max(1) as usize;
    let mut consecutive_timeouts = 0u32;
    let mut read_buf = Vec::<u8>::new();
    let mut mono = Vec::<f32>::new();
    let mut resampled = Vec::<f32>::new();

    while !stop.load(Ordering::Relaxed) {
        match handle.wait_for_event(500) {
            Ok(()) => {}
            Err(wasapi::WasapiError::EventTimeout) => {
                consecutive_timeouts = consecutive_timeouts.saturating_add(1);
                if consecutive_timeouts == 121 {
                    tracing::warn!("[wasapi capture] wait_for_event timed out repeatedly; stream remains alive");
                }
                continue;
            }
            Err(error) => {
                tracing::error!("[wasapi capture] wait_for_event failed: {error:#}");
                return Err(error).context("wait for WASAPI capture event");
            }
        }

        loop {
            let next_packet = capture
                .get_next_packet_size()
                .context("query capture packet size")?;
            let Some(packet_frames) = next_packet else {
                break;
            };
            if packet_frames == 0 {
                break;
            }

            consecutive_timeouts = 0;

            let packet_bytes = packet_frames as usize * block_align;
            if read_buf.len() < packet_bytes {
                read_buf.resize(packet_bytes, 0);
            }

            let (frames, info) = capture
                .read_from_device(&mut read_buf[..packet_bytes])
                .map_err(|error| {
                    tracing::error!("[wasapi capture] read_from_device failed: {error:#}");
                    error
                })
                .context("read WASAPI capture packet")?;

            mono.clear();
            if info.flags.silent {
                mono.resize(frames as usize, 0.0);
            } else {
                decode_interleaved_to_mono(
                    &mut mono,
                    &read_buf,
                    frames as usize,
                    block_align,
                    device_channels,
                    sample_type,
                    valid_bits,
                );
            }

            resampled.clear();
            resampler.process(&mono, &mut resampled);
            for &sample in &resampled {
                let v = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
                for _ in 0..target_channels {
                    let _ = prod.try_push(v);
                }
            }
        }
    }

    let _ = audio_client.stop_stream();
    Ok(())
}

fn decode_interleaved_to_mono(
    out: &mut Vec<f32>,
    data: &[u8],
    frames: usize,
    block_align: usize,
    channels: usize,
    sample_type: SampleType,
    valid_bits: u16,
) {
    out.clear();
    out.reserve(frames);

    if channels == 0 || block_align == 0 || block_align % channels != 0 {
        error!(
            "[wasapi capture] invalid frame layout: block_align={} channels={}",
            block_align, channels
        );
        out.resize(frames, 0.0);
        return;
    }

    let bytes_per_sample = block_align / channels;
    let scale = int_scale(valid_bits);

    match sample_type {
        SampleType::Float => {
            if bytes_per_sample != 4 {
                error!(
                    "[wasapi capture] unsupported float bytes_per_sample={} (block_align={} channels={})",
                    bytes_per_sample,
                    block_align,
                    channels
                );
                out.resize(frames, 0.0);
                return;
            }

            for frame in data.chunks_exact(block_align).take(frames) {
                let mut sum = 0.0f32;
                for ch in 0..channels {
                    let base = ch * bytes_per_sample;
                    let sample = f32::from_le_bytes([
                        frame[base],
                        frame[base + 1],
                        frame[base + 2],
                        frame[base + 3],
                    ]);
                    sum += sample;
                }
                out.push(sum / channels as f32);
            }
        }
        SampleType::Int => match bytes_per_sample {
            2 => {
                for frame in data.chunks_exact(block_align).take(frames) {
                    let mut sum = 0.0f32;
                    for ch in 0..channels {
                        let base = ch * bytes_per_sample;
                        let sample = i16::from_le_bytes([frame[base], frame[base + 1]]) as i32;
                        sum += sample as f32 / scale;
                    }
                    out.push(sum / channels as f32);
                }
            }
            3 => {
                for frame in data.chunks_exact(block_align).take(frames) {
                    let mut sum = 0.0f32;
                    for ch in 0..channels {
                        let base = ch * bytes_per_sample;
                        let mut sample = (frame[base] as i32)
                            | ((frame[base + 1] as i32) << 8)
                            | ((frame[base + 2] as i32) << 16);
                        if (sample & 0x0080_0000) != 0 {
                            sample |= !0x00FF_FFFF;
                        }
                        sum += sample as f32 / scale;
                    }
                    out.push(sum / channels as f32);
                }
            }
            4 => {
                for frame in data.chunks_exact(block_align).take(frames) {
                    let mut sum = 0.0f32;
                    for ch in 0..channels {
                        let base = ch * bytes_per_sample;
                        let mut sample = i32::from_le_bytes([
                            frame[base],
                            frame[base + 1],
                            frame[base + 2],
                            frame[base + 3],
                        ]);
                        if valid_bits > 0 && valid_bits < 32 {
                            let shift = 32 - valid_bits;
                            sample = (sample << shift) >> shift;
                        }
                        sum += sample as f32 / scale;
                    }
                    out.push(sum / channels as f32);
                }
            }
            _ => {
                error!(
                        "[wasapi capture] unsupported integer bytes_per_sample={} (block_align={} channels={} valid_bits={})",
                        bytes_per_sample,
                        block_align,
                        channels,
                        valid_bits
                    );
                out.resize(frames, 0.0);
                return;
            }
        },
    }
}

fn int_scale(valid_bits: u16) -> f32 {
    match valid_bits {
        0 => 1.0,
        1..=31 => ((1_i64 << (valid_bits - 1)) - 1) as f32,
        _ => i32::MAX as f32,
    }
}
