use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::f32::consts::{PI, TAU};
use std::time::Duration;

const URL_TONE_FREQ_HZ: f32 = 880.0;
const URL_TONE_DURATION_MS: u64 = 140;
const MEMBER_TONE_TOTAL_MS: u64 = 420;

#[derive(Clone, Copy)]
struct ToneSpec {
    freq_hz: f32,
    start_ms: f32,
    duration_ms: f32,
}

pub fn play_soft_url_tone(volume: f32) {
    std::thread::spawn(move || {
        if let Err(err) = play_soft_url_tone_impl(volume.clamp(0.0, 1.0)) {
            tracing::debug!("notification sound playback skipped: {err:#}");
        }
    });
}

pub fn play_member_join_tone(volume: f32) {
    play_member_tone(volume, true);
}

pub fn play_member_leave_tone(volume: f32) {
    play_member_tone(volume, false);
}

fn play_member_tone(volume: f32, joined: bool) {
    std::thread::spawn(move || {
        if let Err(err) = play_member_tone_impl(volume.clamp(0.0, 1.0), joined) {
            tracing::debug!("member notification sound playback skipped: {err:#}");
        }
    });
}

fn play_soft_url_tone_impl(volume: f32) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default output device")?;
    let config = device
        .default_output_config()
        .context("default output config")?;

    let sample_rate = config.sample_rate() as f32;
    let channels = config.channels() as usize;
    let total_samples = ((URL_TONE_DURATION_MS as f32 / 1000.0) * sample_rate) as usize;

    let mut sample_idx: usize = 0;
    let tone_gain = (volume * 0.10).max(0.01);

    let err_fn = |err| {
        tracing::debug!("notification audio stream error: {err}");
    };

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let cfg: cpal::StreamConfig = config.into();
            device.build_output_stream(
                &cfg,
                move |output: &mut [f32], _| {
                    write_sine_samples(
                        output,
                        channels,
                        &mut sample_idx,
                        total_samples,
                        sample_rate,
                        tone_gain,
                    )
                },
                err_fn,
                None,
            )?
        }
        cpal::SampleFormat::I16 => {
            let cfg: cpal::StreamConfig = config.into();
            device.build_output_stream(
                &cfg,
                move |output: &mut [i16], _| {
                    write_sine_samples_i16(
                        output,
                        channels,
                        &mut sample_idx,
                        total_samples,
                        sample_rate,
                        tone_gain,
                    )
                },
                err_fn,
                None,
            )?
        }
        cpal::SampleFormat::U16 => {
            let cfg: cpal::StreamConfig = config.into();
            device.build_output_stream(
                &cfg,
                move |output: &mut [u16], _| {
                    write_sine_samples_u16(
                        output,
                        channels,
                        &mut sample_idx,
                        total_samples,
                        sample_rate,
                        tone_gain,
                    )
                },
                err_fn,
                None,
            )?
        }
        other => {
            return Err(anyhow::anyhow!(
                "unsupported output sample format: {other:?}"
            ))
        }
    };

    stream.play().context("start notification stream")?;
    std::thread::sleep(Duration::from_millis(URL_TONE_DURATION_MS + 40));
    Ok(())
}

fn play_member_tone_impl(volume: f32, joined: bool) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default output device")?;
    let config = device
        .default_output_config()
        .context("default output config")?;

    let sample_rate = config.sample_rate() as f32;
    let channels = config.channels() as usize;
    let samples = build_member_tone_buffer(sample_rate, volume, joined);
    let mut sample_idx: usize = 0;

    let err_fn = |err| {
        tracing::debug!("notification audio stream error: {err}");
    };

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let cfg: cpal::StreamConfig = config.into();
            device.build_output_stream(
                &cfg,
                move |output: &mut [f32], _| {
                    write_buffer_samples(output, channels, &samples, &mut sample_idx)
                },
                err_fn,
                None,
            )?
        }
        cpal::SampleFormat::I16 => {
            let cfg: cpal::StreamConfig = config.into();
            device.build_output_stream(
                &cfg,
                move |output: &mut [i16], _| {
                    write_buffer_samples_i16(output, channels, &samples, &mut sample_idx)
                },
                err_fn,
                None,
            )?
        }
        cpal::SampleFormat::U16 => {
            let cfg: cpal::StreamConfig = config.into();
            device.build_output_stream(
                &cfg,
                move |output: &mut [u16], _| {
                    write_buffer_samples_u16(output, channels, &samples, &mut sample_idx)
                },
                err_fn,
                None,
            )?
        }
        other => {
            return Err(anyhow::anyhow!(
                "unsupported output sample format: {other:?}"
            ))
        }
    };

    stream.play().context("start member notification stream")?;
    std::thread::sleep(Duration::from_millis(MEMBER_TONE_TOTAL_MS + 90));
    Ok(())
}

fn build_member_tone_buffer(sample_rate: f32, volume: f32, joined: bool) -> Vec<f32> {
    let total_samples = ((MEMBER_TONE_TOTAL_MS as f32 / 1000.0) * sample_rate) as usize;
    let mut dry = vec![0.0_f32; total_samples];
    let tone_gain = (volume * 0.12).max(0.015);
    let tones = if joined {
        [
            ToneSpec {
                freq_hz: 560.0,
                start_ms: 0.0,
                duration_ms: 120.0,
            },
            ToneSpec {
                freq_hz: 820.0,
                start_ms: 170.0,
                duration_ms: 120.0,
            },
        ]
    } else {
        [
            ToneSpec {
                freq_hz: 820.0,
                start_ms: 0.0,
                duration_ms: 120.0,
            },
            ToneSpec {
                freq_hz: 560.0,
                start_ms: 170.0,
                duration_ms: 120.0,
            },
        ]
    };

    for (idx, sample) in dry.iter_mut().enumerate() {
        let t_ms = idx as f32 * 1000.0 / sample_rate;
        let mut mixed = 0.0_f32;
        for tone in tones {
            let rel_ms = t_ms - tone.start_ms;
            if !(0.0..tone.duration_ms).contains(&rel_ms) {
                continue;
            }
            let rel_sec = rel_ms / 1000.0;
            let phase = TAU * tone.freq_hz * rel_sec;
            let x = rel_ms / tone.duration_ms;
            let env = (PI * x).sin().powi(2);
            mixed += phase.sin() * env;
        }
        *sample = mixed * tone_gain;
    }

    let delay_a = ((0.045 * sample_rate) as usize).max(1);
    let delay_b = ((0.092 * sample_rate) as usize).max(1);
    let mut wet = vec![0.0_f32; total_samples];
    for i in 0..total_samples {
        let mut out = dry[i];
        if i >= delay_a {
            out += wet[i - delay_a] * 0.28;
        }
        if i >= delay_b {
            out += wet[i - delay_b] * 0.16;
        }
        wet[i] = out;
    }

    wet.into_iter()
        .map(|s| (s * 0.78).clamp(-1.0, 1.0))
        .collect()
}

fn write_buffer_samples(
    output: &mut [f32],
    channels: usize,
    source: &[f32],
    sample_idx: &mut usize,
) {
    for frame in output.chunks_mut(channels) {
        let sample = source.get(*sample_idx).copied().unwrap_or(0.0);
        *sample_idx = sample_idx.saturating_add(1);
        for chan in frame {
            *chan = sample;
        }
    }
}

fn write_buffer_samples_i16(
    output: &mut [i16],
    channels: usize,
    source: &[f32],
    sample_idx: &mut usize,
) {
    let mut tmp = vec![0.0_f32; output.len()];
    write_buffer_samples(&mut tmp, channels, source, sample_idx);
    for (dst, src) in output.iter_mut().zip(tmp.into_iter()) {
        *dst = (src.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
    }
}

fn write_buffer_samples_u16(
    output: &mut [u16],
    channels: usize,
    source: &[f32],
    sample_idx: &mut usize,
) {
    let mut tmp = vec![0.0_f32; output.len()];
    write_buffer_samples(&mut tmp, channels, source, sample_idx);
    for (dst, src) in output.iter_mut().zip(tmp.into_iter()) {
        let norm = ((src.clamp(-1.0, 1.0) * 0.5) + 0.5) * u16::MAX as f32;
        *dst = norm as u16;
    }
}

fn write_sine_samples(
    output: &mut [f32],
    channels: usize,
    sample_idx: &mut usize,
    total_samples: usize,
    sample_rate: f32,
    gain: f32,
) {
    for frame in output.chunks_mut(channels) {
        let sample = if *sample_idx < total_samples {
            let t = *sample_idx as f32 / sample_rate;
            let env = 1.0 - (*sample_idx as f32 / total_samples as f32);
            (TAU * URL_TONE_FREQ_HZ * t).sin() * gain * env
        } else {
            0.0
        };
        *sample_idx = sample_idx.saturating_add(1);
        for chan in frame {
            *chan = sample;
        }
    }
}

fn write_sine_samples_i16(
    output: &mut [i16],
    channels: usize,
    sample_idx: &mut usize,
    total_samples: usize,
    sample_rate: f32,
    gain: f32,
) {
    let mut tmp = vec![0.0_f32; output.len()];
    write_sine_samples(
        &mut tmp,
        channels,
        sample_idx,
        total_samples,
        sample_rate,
        gain,
    );
    for (dst, src) in output.iter_mut().zip(tmp.into_iter()) {
        *dst = (src.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
    }
}

fn write_sine_samples_u16(
    output: &mut [u16],
    channels: usize,
    sample_idx: &mut usize,
    total_samples: usize,
    sample_rate: f32,
    gain: f32,
) {
    let mut tmp = vec![0.0_f32; output.len()];
    write_sine_samples(
        &mut tmp,
        channels,
        sample_idx,
        total_samples,
        sample_rate,
        gain,
    );
    for (dst, src) in output.iter_mut().zip(tmp.into_iter()) {
        let norm = ((src.clamp(-1.0, 1.0) * 0.5) + 0.5) * u16::MAX as f32;
        *dst = norm as u16;
    }
}
