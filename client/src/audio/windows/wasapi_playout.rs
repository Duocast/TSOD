use anyhow::{anyhow, Context, Result};
use ringbuf::{traits::Consumer, HeapCons};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tracing::{debug, error, info};
use wasapi::{BufferFlags, Direction, SampleType, StreamMode};

use crate::{
    audio::resample::LinearResampler,
    ui::model::{AudioBackend, AudioDeviceId, AudioDeviceInfo, AudioDirection},
};

use super::wasapi_common::{default_endpoint_id, enumerate_endpoints, open_device, ComGuard};

pub struct WasapiPlayout {
    thread: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    unhealthy: Arc<AtomicBool>,
}

impl WasapiPlayout {
    pub fn start(
        sample_rate: u32,
        channels: u16,
        cons: HeapCons<i16>,
        preferred_device: Option<&str>,
        _preferred_mode: Option<&str>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let unhealthy = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let unhealthy_thread = unhealthy.clone();
        let preferred_device = preferred_device.map(str::to_string);

        let thread = std::thread::Builder::new()
            .name("tsod-wasapi-playout".to_string())
            .spawn(move || {
                if let Err(error) = run_playout_thread(
                    sample_rate,
                    channels,
                    cons,
                    preferred_device.as_deref(),
                    &stop_thread,
                ) {
                    error!("[wasapi playout] thread failed: {error:#}");
                    unhealthy_thread.store(true, Ordering::Relaxed);
                }
            })
            .context("spawn WASAPI playout thread")?;

        Ok(Self {
            thread: Some(thread),
            stop,
            unhealthy,
        })
    }

    pub fn enumerate_output_devices() -> Vec<AudioDeviceInfo> {
        let default_id = default_endpoint_id(Direction::Render);
        let endpoints = match enumerate_endpoints(Direction::Render) {
            Ok(values) => values,
            Err(error) => {
                error!("[wasapi playout] enumerate endpoints failed: {error:#}");
                return Vec::new();
            }
        };

        tracing::debug!(
            count = endpoints.len(),
            "[wasapi playout] enumerated output endpoints"
        );
        for (id, friendly) in endpoints.iter().take(4) {
            tracing::debug!(endpoint_id = %id, friendly_name = %friendly, "[wasapi playout] output endpoint");
        }

        endpoints
            .into_iter()
            .map(|(endpoint_id, friendly_name)| AudioDeviceInfo {
                key: AudioDeviceId {
                    backend: AudioBackend::Wasapi,
                    direction: AudioDirection::Output,
                    id: endpoint_id.clone(),
                },
                label: friendly_name.clone(),
                display_label: friendly_name,
                is_default: default_id.as_deref() == Some(endpoint_id.as_str()),
            })
            .collect()
    }

    pub fn enumerate_playback_modes() -> Vec<String> {
        vec![
            super::super::playout::PLAYBACK_MODE_AUTO.to_string(),
            super::super::playout::PLAYBACK_MODE_WASAPI.to_string(),
        ]
    }

    pub fn is_healthy(&self) -> bool {
        !self.unhealthy.load(Ordering::Relaxed)
    }
}

impl Drop for WasapiPlayout {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_playout_thread(
    sample_rate: u32,
    channels: u16,
    mut cons: HeapCons<i16>,
    preferred_device: Option<&str>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let _com = ComGuard::new()?;
    let device = open_device(Direction::Render, preferred_device)?;
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
        "[wasapi playout] open device id={} name={} mix: {}Hz {}ch block_align={} bits={} valid={} type={:?}",
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
            "[wasapi playout] 24-bit path active: bytes_per_sample={} valid_bits={} block_align={} channels={}",
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
        .initialize_client(&mix, &Direction::Render, &mode)
        .map_err(|error| {
            tracing::error!("[wasapi playout] initialize_client failed: {error:#}");
            error
        })
        .context("initialize WASAPI shared render stream")?;
    let handle = audio_client
        .set_get_eventhandle()
        .context("set WASAPI render event handle")?;
    let render = audio_client
        .get_audiorenderclient()
        .context("get WASAPI render client")?;
    audio_client
        .start_stream()
        .map_err(|error| {
            tracing::error!("[wasapi playout] start_stream failed: {error:#}");
            error
        })
        .context("start WASAPI render stream")?;

    let mut resampler = LinearResampler::new(sample_rate, device_rate);
    let source_channels = channels.max(1) as usize;
    let mut source_mono = Vec::<f32>::new();
    let mut source_resampled = Vec::<f32>::new();

    while !stop.load(Ordering::Relaxed) {
        handle
            .wait_for_event(500)
            .map_err(|error| {
                tracing::error!("[wasapi playout] wait_for_event failed: {error:#}");
                error
            })
            .context("wait for WASAPI render event")?;
        let avail = audio_client
            .get_available_space_in_frames()
            .context("query WASAPI render space")? as usize;
        if avail == 0 {
            continue;
        }

        fill_source_mono(&mut cons, &mut source_mono, avail, source_channels);
        source_resampled.clear();
        resampler.process(&source_mono, &mut source_resampled);

        let mut bytes = vec![0u8; avail * block_align];
        let flags = write_render_bytes(
            &mut bytes,
            avail,
            block_align,
            device_channels,
            sample_type,
            &source_resampled,
            valid_bits,
        )?;

        render
            .write_to_device(avail, &bytes, Some(flags))
            .map_err(|error| {
                tracing::error!("[wasapi playout] write_to_device failed: {error:#}");
                error
            })
            .context("write WASAPI render buffer")?;
    }

    let _ = audio_client.stop_stream();
    Ok(())
}

fn fill_source_mono(
    cons: &mut HeapCons<i16>,
    out: &mut Vec<f32>,
    frames: usize,
    source_channels: usize,
) {
    out.clear();
    out.resize(frames, 0.0);

    for sample in out.iter_mut().take(frames) {
        *sample = cons
            .try_pop()
            .map(|s| s as f32 / i16::MAX as f32)
            .unwrap_or(0.0);
        for _ in 1..source_channels {
            let _ = cons.try_pop();
        }
    }
}

fn write_render_bytes(
    dst: &mut [u8],
    frames: usize,
    block_align: usize,
    channels: usize,
    sample_type: SampleType,
    mono: &[f32],
    valid_bits: u16,
) -> Result<BufferFlags> {
    if mono.is_empty() {
        return Ok(BufferFlags {
            silent: true,
            ..BufferFlags::none()
        });
    }

    if channels == 0 || block_align == 0 || block_align % channels != 0 {
        return Err(anyhow!(
            "invalid render frame layout: block_align={block_align} channels={channels}"
        ));
    }

    let bytes_per_sample = block_align / channels;
    let scale = int_scale(valid_bits);

    match sample_type {
        SampleType::Float => {
            if bytes_per_sample != 4 {
                return Err(anyhow!(
                    "unsupported WASAPI float bytes_per_sample={bytes_per_sample}"
                ));
            }

            for (frame_idx, frame) in dst.chunks_exact_mut(block_align).take(frames).enumerate() {
                let sample = mono.get(frame_idx).copied().unwrap_or(0.0).clamp(-1.0, 1.0);
                let encoded = sample.to_le_bytes();
                for ch in 0..channels {
                    let base = ch * bytes_per_sample;
                    frame[base..base + 4].copy_from_slice(&encoded);
                }
            }
        }
        SampleType::Int => match bytes_per_sample {
            2 => {
                for (frame_idx, frame) in dst.chunks_exact_mut(block_align).take(frames).enumerate()
                {
                    let sample =
                        scale_to_i32(mono.get(frame_idx).copied().unwrap_or(0.0), scale) as i16;
                    let encoded = sample.to_le_bytes();
                    for ch in 0..channels {
                        let base = ch * bytes_per_sample;
                        frame[base..base + 2].copy_from_slice(&encoded);
                    }
                }
            }
            3 => {
                for (frame_idx, frame) in dst.chunks_exact_mut(block_align).take(frames).enumerate()
                {
                    let sample = scale_to_i32(mono.get(frame_idx).copied().unwrap_or(0.0), scale);
                    let encoded = sample.to_le_bytes();
                    for ch in 0..channels {
                        let base = ch * bytes_per_sample;
                        frame[base] = encoded[0];
                        frame[base + 1] = encoded[1];
                        frame[base + 2] = encoded[2];
                    }
                }
            }
            4 => {
                for (frame_idx, frame) in dst.chunks_exact_mut(block_align).take(frames).enumerate()
                {
                    let mut sample =
                        scale_to_i32(mono.get(frame_idx).copied().unwrap_or(0.0), scale);
                    if valid_bits > 0 && valid_bits < 32 {
                        let shift = 32 - valid_bits;
                        sample = (sample << shift) >> shift;
                    }
                    let encoded = sample.to_le_bytes();
                    for ch in 0..channels {
                        let base = ch * bytes_per_sample;
                        frame[base..base + 4].copy_from_slice(&encoded);
                    }
                }
            }
            _ => {
                return Err(anyhow!(
                    "unsupported WASAPI integer bytes_per_sample={bytes_per_sample} (valid_bits={valid_bits})"
                ));
            }
        },
    }

    Ok(BufferFlags::none())
}

fn int_scale(valid_bits: u16) -> f32 {
    match valid_bits {
        0 => 1.0,
        1..=31 => ((1_i64 << (valid_bits - 1)) - 1) as f32,
        _ => i32::MAX as f32,
    }
}

fn scale_to_i32(sample: f32, scale: f32) -> i32 {
    (sample.clamp(-1.0, 1.0) * scale).round() as i32
}
