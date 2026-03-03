use anyhow::{anyhow, Context, Result};
use wasapi::{
    AudioClient, Device, DeviceEnumerator, DeviceState, Direction, ShareMode, WaveFormat,
};

pub struct ComGuard {
    initialized: bool,
}

impl ComGuard {
    pub fn new() -> Result<Self> {
        if wasapi::initialize_mta().is_ok() {
            return Ok(Self { initialized: true });
        }

        let sta_result = wasapi::initialize_sta();
        if sta_result.is_ok() {
            Ok(Self { initialized: true })
        } else {
            Err(anyhow!("initialize COM for WASAPI failed: {sta_result:?}"))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized {
            let _ = wasapi::deinitialize();
        }
    }
}

pub fn enumerate_endpoints(direction: Direction) -> Result<Vec<(String, String)>> {
    let _com = ComGuard::new()?;
    let enumerator = DeviceEnumerator::new().context("create WASAPI device enumerator")?;
    let collection = enumerator
        .get_device_collection(&direction)
        .context("enumerate WASAPI endpoint collection")?;

    let mut out = Vec::new();
    for device in &collection {
        let device = device.context("read endpoint device")?;
        let id = device.get_id().context("read endpoint id")?;
        let friendly = device
            .get_friendlyname()
            .or_else(|_| device.get_description())
            .unwrap_or_else(|_| "Unknown device".to_string());
        out.push((id, friendly));
    }
    Ok(out)
}

pub fn default_endpoint_id(direction: Direction) -> Option<String> {
    let _com = ComGuard::new().ok()?;
    let enumerator = DeviceEnumerator::new().ok()?;
    let device = enumerator.get_default_device(&direction).ok()?;
    device.get_id().ok()
}

pub fn open_device(direction: Direction, preferred_id: Option<&str>) -> Result<Device> {
    let enumerator = DeviceEnumerator::new().context("create WASAPI device enumerator")?;

    if let Some(id) = preferred_id {
        match enumerator.get_device(id) {
            Ok(device) => {
                let state = device.get_state().with_context(|| {
                    format!("read WASAPI device state for selected endpoint id: {id}")
                })?;
                if state == DeviceState::Active {
                    return Ok(device);
                }

                tracing::warn!(
                    "[wasapi] selected endpoint is not Active (id={} state={}); falling back to default",
                    id,
                    state
                );
            }
            Err(error) => {
                tracing::warn!(
                    "[wasapi] open by id failed (id={}): {}; falling back to default",
                    id,
                    error
                );
            }
        }
    }

    let default_device = enumerator
        .get_default_device(&direction)
        .context("open default WASAPI device")?;

    let default_state = default_device
        .get_state()
        .context("read default WASAPI device state")?;
    if default_state != DeviceState::Active {
        return Err(anyhow!(
            "default WASAPI device is not Active: {}",
            default_state
        ));
    }

    Ok(default_device)
}

pub fn negotiate_shared_voice_format(
    audio_client: &AudioClient,
    mix: &WaveFormat,
    preferred_rate: u32,
    preferred_channels: &[usize],
    log_prefix: &str,
) -> WaveFormat {
    let mix_rate = mix.get_samplespersec();
    let mix_channels = mix.get_nchannels().max(1) as usize;
    let bits_per_sample = mix.get_bitspersample() as usize;
    let valid_bits = mix.get_validbitspersample().max(1) as usize;
    let sample_type = match mix.get_subformat() {
        Ok(sample_type) => sample_type,
        Err(error) => {
            tracing::debug!(
                "[{log_prefix}] could not read mix subformat ({error:#}); using mix format"
            );
            return mix.clone();
        }
    };

    let mut candidate_channels = Vec::with_capacity(preferred_channels.len() + 1);
    for &channels in preferred_channels {
        if channels > 0 && !candidate_channels.contains(&channels) {
            candidate_channels.push(channels);
        }
    }
    if !candidate_channels.contains(&mix_channels) {
        candidate_channels.push(mix_channels);
    }

    let mut closest_fallback: Option<WaveFormat> = None;
    for channels in candidate_channels {
        let requested = WaveFormat::new(
            bits_per_sample,
            valid_bits,
            &sample_type,
            preferred_rate as usize,
            channels,
            None,
        );

        match audio_client.is_supported(&requested, &ShareMode::Shared) {
            Ok(None) => {
                tracing::info!(
                    "[{log_prefix}] using negotiated shared voice format {}Hz {}ch",
                    preferred_rate,
                    channels
                );
                return requested;
            }
            Ok(Some(closest)) => {
                tracing::debug!(
                    "[{log_prefix}] requested {}Hz {}ch not directly supported; closest is {}Hz {}ch",
                    preferred_rate,
                    channels,
                    closest.get_samplespersec(),
                    closest.get_nchannels().max(1)
                );
                if closest_fallback.is_none() {
                    closest_fallback = Some(closest);
                }
            }
            Err(error) => {
                tracing::debug!(
                    "[{log_prefix}] shared support query for {}Hz {}ch failed ({error:#})",
                    preferred_rate,
                    channels
                );
            }
        }
    }

    if let Some(closest) = closest_fallback {
        tracing::info!(
            "[{log_prefix}] falling back to closest shared voice format {}Hz {}ch (mix is {}Hz {}ch)",
            closest.get_samplespersec(),
            closest.get_nchannels().max(1),
            mix_rate,
            mix_channels
        );
        closest
    } else {
        tracing::info!(
            "[{log_prefix}] keeping mix format {}Hz {}ch (no preferred 48k mono/stereo format available)",
            mix_rate,
            mix_channels
        );
        mix.clone()
    }
}
