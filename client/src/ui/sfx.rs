use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::f32::consts::TAU;
use std::time::Duration;

const URL_TONE_FREQ_HZ: f32 = 880.0;
const URL_TONE_DURATION_MS: u64 = 140;

pub fn play_soft_url_tone(volume: f32) {
    std::thread::spawn(move || {
        if let Err(err) = play_soft_url_tone_impl(volume.clamp(0.0, 1.0)) {
            tracing::debug!("notification sound playback skipped: {err:#}");
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

    let sample_rate = config.sample_rate().0 as f32;
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
