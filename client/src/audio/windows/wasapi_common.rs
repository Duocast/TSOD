use anyhow::{anyhow, Context, Result};
use wasapi::{Device, DeviceEnumerator, Direction};

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
            Err(anyhow!(
                "initialize COM for WASAPI failed: {sta_result:?}"
            ))
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
    match preferred_id {
        Some(id) => enumerator
            .get_device(id)
            .with_context(|| format!("open WASAPI device by endpoint id: {id}")),
        None => enumerator
            .get_default_device(&direction)
            .context("open default WASAPI device"),
    }
}
