use anyhow::{anyhow, Context, Result};
use wasapi::{Device, DeviceEnumerator, DeviceState, Direction};

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
