use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Sender;
use ringbuf::{traits::Consumer, HeapCons};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tracing::{debug, error, info};
use wasapi::{BufferFlags, Direction, SampleType, StreamMode};

use crate::{
    audio::resample::{ResamplerImpl, ResamplerMode},
    ui::{
        model::{AudioBackend, AudioDeviceId, AudioDeviceInfo, AudioDirection},
        UiEvent,
    },
};

use super::wasapi_common::{
    default_endpoint_id, enumerate_endpoints, negotiate_shared_voice_format, open_device, ComGuard,
};

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
                    if reported_thread
                        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        if let Some(tx) = &tx_event_thread {
                            let _ = tx.send(UiEvent::AppendLog(format!(
                                "[audio] wasapi playout thread failed: {error:#}"
                            )));
                        }
                    }
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

    let mut audio_client = device.get_iaudioclient().with_context(|| {
        format!(
            "get WASAPI audio client (id={} name={})",
            endpoint_id, friendly_name
        )
    })?;
    let mix = audio_client
        .get_mixformat()
        .context("get WASAPI mix format")?;
    let mix_rate = mix.get_samplespersec();
    let mix_channels = mix.get_nchannels().max(1) as usize;
    let mix_block_align = mix.get_blockalign() as usize;
    let mix_bits_per_sample = mix.get_bitspersample() as u16;
    let mix_valid_bits = mix.get_validbitspersample() as u16;
    let mix_sample_type = mix.get_subformat().context("get WASAPI mix sample type")?;

    let stream_format =
        negotiate_shared_voice_format(&audio_client, &mix, sample_rate, &[2, 1], "wasapi playout");

    let device_rate = stream_format.get_samplespersec();
    let device_channels = stream_format.get_nchannels().max(1) as usize;
    let block_align = stream_format.get_blockalign() as usize;
    let bits_per_sample = stream_format.get_bitspersample() as u16;
    let valid_bits = stream_format.get_validbitspersample() as u16;
    let effective_valid_bits = match valid_bits {
        0 => bits_per_sample,
        _ => valid_bits,
    }
    .clamp(1, 32);
    let sample_type = stream_format
        .get_subformat()
        .context("get WASAPI stream sample type")?;
    let bytes_per_sample = block_align / device_channels;

    info!(
        "[wasapi playout] open device id={} name={} mix: {}Hz {}ch block_align={} bits={} valid={} type={:?}; stream: {}Hz {}ch block_align={} bits={} valid={} effective_valid={} type={:?}",
        endpoint_id,
        friendly_name,
        mix_rate,
        mix_channels,
        mix_block_align,
        mix_bits_per_sample,
        mix_valid_bits,
        mix_sample_type,
        device_rate,
        device_channels,
        block_align,
        bits_per_sample,
        valid_bits,
        effective_valid_bits,
        sample_type
    );

    if bytes_per_sample == 3 || valid_bits == 24 {
        debug!(
            "[wasapi playout] 24-bit path active: bytes_per_sample={} valid_bits={} effective_valid_bits={} block_align={} channels={}",
            bytes_per_sample,
            valid_bits,
            effective_valid_bits,
            block_align,
            device_channels
        );
    }

    let mode = StreamMode::EventsShared {
        autoconvert: false,
        buffer_duration_hns: 200_000,
    };
    audio_client
        .initialize_client(&stream_format, &Direction::Render, &mode)
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

    let resampler_mode = ResamplerMode::from_env();
    tracing::info!(
        "[audio] wasapi playout resampler={} in_rate={} out_rate={} channels=1",
        resampler_mode.as_str(),
        sample_rate,
        device_rate
    );
    let mut resampler = ResamplerImpl::new(sample_rate, device_rate, 1, resampler_mode);
    let source_channels = channels.max(1) as usize;
    let mut consecutive_timeouts = 0u32;
    let mut source_mono = Vec::<f32>::new();
    let mut scratch_resampled = Vec::<f32>::new();
    let mut render_mono = Vec::<f32>::new();
    let mut bytes = Vec::<u8>::new();
    let mut out_fifo = Vec::<f32>::new();
    let mut out_off = 0usize;
    let mut padded_out_frames_total = 0usize;
    let mut last_write_instant = std::time::Instant::now();
    let mut last_stats_log = std::time::Instant::now();

    while !stop.load(Ordering::Relaxed) {
        match handle.wait_for_event(500) {
            Ok(()) => {}
            Err(wasapi::WasapiError::EventTimeout) => {
                consecutive_timeouts = consecutive_timeouts.saturating_add(1);
                let stalled_for = last_write_instant.elapsed();
                if stalled_for >= std::time::Duration::from_secs(5) {
                    return Err(anyhow!(
                        "WASAPI playout stalled: no successful writes for {}ms",
                        stalled_for.as_millis()
                    ));
                }
                if consecutive_timeouts == 121 {
                    tracing::warn!("[wasapi playout] wait_for_event timed out repeatedly; stream remains alive");
                }
                continue;
            }
            Err(error) => {
                tracing::error!("[wasapi playout] wait_for_event failed: {error:#}");
                return Err(error).context("wait for WASAPI render event");
            }
        }
        let avail = audio_client
            .get_available_space_in_frames()
            .context("query WASAPI render space")? as usize;
        if avail == 0 {
            continue;
        }

        consecutive_timeouts = 0;

        let mut refill_loops = 0usize;
        while output_fifo_len(&out_fifo, out_off) < avail {
            let missing_out_frames = avail - output_fifo_len(&out_fifo, out_off);
            let need_in =
                needed_input_frames_for_output(sample_rate, device_rate, missing_out_frames, 10);

            fill_source_mono(&mut cons, &mut source_mono, need_in, source_channels);
            scratch_resampled.clear();
            resampler.process_mono(&source_mono, &mut scratch_resampled);
            out_fifo.extend_from_slice(&scratch_resampled);

            refill_loops += 1;
            if refill_loops >= 16 {
                break;
            }
        }

        render_mono.clear();
        let available = output_fifo_len(&out_fifo, out_off);
        let to_copy = available.min(avail);
        render_mono.extend_from_slice(&out_fifo[out_off..out_off + to_copy]);
        out_off += to_copy;
        if to_copy < avail {
            let pad = avail - to_copy;
            render_mono.resize(avail, 0.0);
            padded_out_frames_total = padded_out_frames_total.saturating_add(pad);
        }

        if out_off > 8192 {
            out_fifo.drain(..out_off);
            out_off = 0;
        }

        bytes.resize(avail * block_align, 0);
        let flags = write_render_bytes(
            &mut bytes,
            avail,
            block_align,
            device_channels,
            sample_type,
            &render_mono,
            effective_valid_bits,
        )?;

        render
            .write_to_device(avail, &bytes, Some(flags))
            .map_err(|error| {
                tracing::error!("[wasapi playout] write_to_device failed: {error:#}");
                error
            })
            .context("write WASAPI render buffer")?;
        last_write_instant = std::time::Instant::now();

        if last_stats_log.elapsed() >= std::time::Duration::from_secs(5) {
            debug!(
                "WASAPI playout: device_rate={}, src_rate={}, padded_out_frames={}",
                device_rate, sample_rate, padded_out_frames_total
            );
            last_stats_log = std::time::Instant::now();
        }
    }

    let _ = audio_client.stop_stream();
    Ok(())
}

fn output_fifo_len(fifo: &[f32], out_off: usize) -> usize {
    fifo.len().saturating_sub(out_off)
}

fn needed_input_frames_for_output(
    src_rate: u32,
    dst_rate: u32,
    missing_out_frames: usize,
    slack: usize,
) -> usize {
    if missing_out_frames == 0 {
        return 0;
    }

    let ratio = src_rate as f64 / dst_rate as f64;
    ((missing_out_frames as f64) * ratio).ceil() as usize + slack
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
                        sample <<= shift;
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

#[cfg(test)]
mod wasapi_format_tests {
    use super::write_render_bytes;
    use wasapi::SampleType;

    #[test]
    fn packs_24bit_in_32bit_container_with_high_bit_alignment() {
        let mut dst = vec![0u8; 4];
        let mono = [1.0f32];

        let _ = write_render_bytes(&mut dst, 1, 4, 1, SampleType::Int, &mono, 24)
            .expect("write should succeed");

        let sample = i32::from_le_bytes([dst[0], dst[1], dst[2], dst[3]]);
        assert_eq!(sample, 0x7fffff00);
    }
}

fn scale_to_i32(sample: f32, scale: f32) -> i32 {
    (sample.clamp(-1.0, 1.0) * scale).round() as i32
}

#[cfg(test)]
mod tests {
    use super::needed_input_frames_for_output;

    #[test]
    fn needed_input_frames_covers_rate_ratio_with_slack() {
        let src = 48_000;
        let dst = 44_100;
        let missing_out = 441usize;
        let slack = 10usize;
        let need_in = needed_input_frames_for_output(src, dst, missing_out, slack);
        let min_needed = ((missing_out as f64) * (src as f64 / dst as f64)).ceil() as usize;

        assert!(need_in >= min_needed);
        assert!(need_in >= 480 + slack);
    }
}
