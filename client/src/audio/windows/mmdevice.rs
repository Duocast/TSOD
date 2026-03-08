#![allow(dead_code)]
use anyhow::{anyhow, Context, Result};
use windows::{
    core::PWSTR,
    Win32::{
        Devices::FunctionDiscovery::{
            PKEY_DeviceInterface_FriendlyName, PKEY_Device_DeviceDesc, PKEY_Device_FriendlyName,
        },
        Foundation::PROPERTYKEY,
        Media::Audio::{
            eCapture, eMultimedia, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
            DEVICE_STATE_ACTIVE,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_MULTITHREADED, STGM_READ,
        },
    },
};

pub fn enumerate_output_endpoints() -> Result<Vec<(String, String)>> {
    enumerate_endpoints(eRender)
}

pub fn enumerate_input_endpoints() -> Result<Vec<(String, String)>> {
    enumerate_endpoints(eCapture)
}

pub fn default_output_endpoint_id() -> Result<Option<String>> {
    default_endpoint_id(eRender)
}

pub fn default_input_endpoint_id() -> Result<Option<String>> {
    default_endpoint_id(eCapture)
}

fn enumerate_endpoints(
    data_flow: windows::Win32::Media::Audio::EDataFlow,
) -> Result<Vec<(String, String)>> {
    let _com = ComScope::new()?;
    let enumerator = create_enumerator()?;
    let collection = unsafe {
        enumerator
            .EnumAudioEndpoints(data_flow, DEVICE_STATE_ACTIVE)
            .context("enumerate audio endpoints")?
    };

    let count = unsafe { collection.GetCount().context("get endpoint count")? };
    let mut endpoints = Vec::with_capacity(count as usize);

    for index in 0..count {
        let device = unsafe { collection.Item(index).context("get endpoint item")? };
        let endpoint_id = endpoint_id(&device)?;
        let friendly_name = friendly_name(&device).unwrap_or_else(|_| "Unknown device".to_string());
        endpoints.push((endpoint_id, friendly_name));
    }

    Ok(endpoints)
}

fn default_endpoint_id(
    data_flow: windows::Win32::Media::Audio::EDataFlow,
) -> Result<Option<String>> {
    let _com = ComScope::new()?;
    let enumerator = create_enumerator()?;
    let maybe_device = unsafe { enumerator.GetDefaultAudioEndpoint(data_flow, eMultimedia) };
    match maybe_device {
        Ok(device) => Ok(Some(endpoint_id(&device)?)),
        Err(_) => Ok(None),
    }
}

fn create_enumerator() -> Result<IMMDeviceEnumerator> {
    let enumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER) }
        .context("create MMDevice enumerator")?;
    Ok(enumerator)
}

fn endpoint_id(device: &IMMDevice) -> Result<String> {
    let raw = unsafe { device.GetId().context("get endpoint id")? };
    pwstr_to_string_and_free(raw)
}

fn friendly_name(device: &IMMDevice) -> Result<String> {
    let property_store = unsafe {
        device
            .OpenPropertyStore(STGM_READ)
            .context("open endpoint property store")?
    };

    property_string(&property_store, &PKEY_Device_FriendlyName)
        .or_else(|_| property_string(&property_store, &PKEY_Device_DeviceDesc))
        .or_else(|_| property_string(&property_store, &PKEY_DeviceInterface_FriendlyName))
        .map(|name| {
            if name.trim().is_empty() {
                "Unknown device".to_string()
            } else {
                name
            }
        })
}

fn property_string(
    store: &windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore,
    key: &PROPERTYKEY,
) -> Result<String> {
    let value = unsafe {
        store
            .GetValue(key)
            .context("read endpoint property value")?
    };

    let s = value.to_string();
    if s.is_empty() {
        Err(anyhow!("property value is empty or not a string type"))
    } else {
        Ok(s)
    }
}

fn pwstr_to_string_and_free(pwstr: PWSTR) -> Result<String> {
    if pwstr.is_null() {
        return Err(anyhow!("null PWSTR"));
    }

    let result = pwstr_to_string_no_free(pwstr);
    unsafe {
        CoTaskMemFree(Some(pwstr.0 as *const _));
    }
    result
}

fn pwstr_to_string_no_free(pwstr: PWSTR) -> Result<String> {
    if pwstr.is_null() {
        return Err(anyhow!("null PWSTR"));
    }

    let mut len = 0usize;
    unsafe {
        while *pwstr.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(pwstr.0, len);
        Ok(String::from_utf16_lossy(slice))
    }
}

struct ComScope;

impl ComScope {
    fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .context("initialize COM for MMDevice")?;
        }
        Ok(Self)
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}
