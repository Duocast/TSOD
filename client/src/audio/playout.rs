use anyhow::Result;
use ringbuf::{
    traits::{Producer, Split},
    HeapProd, HeapRb,
};
use std::cell::UnsafeCell;

use crate::ui::model::{disambiguate_display_labels, AudioBackend, AudioDeviceId, AudioDeviceInfo};

pub struct Playout {
    backend: PlayoutBackend,
    prod: UnsafeCell<HeapProd<i16>>,
}

pub const PLAYBACK_MODE_AUTO: &str = "Automatically use best mode";
pub const PLAYBACK_MODE_PIPEWIRE: &str = "PipeWire";
pub const PLAYBACK_MODE_PULSEAUDIO: &str = "PulseAudio";
pub const PLAYBACK_MODE_WASAPI: &str = "WASAPI";

#[cfg(target_os = "linux")]
type PlayoutBackend = linux::LinuxPlayout;

#[cfg(target_os = "windows")]
type PlayoutBackend = crate::audio::windows::wasapi_playout::WasapiPlayout;

#[cfg(target_os = "macos")]
type PlayoutBackend = non_linux::CpalPlayout;

#[cfg(all(
    not(target_os = "linux"),
    not(target_os = "windows"),
    not(target_os = "macos")
))]
type PlayoutBackend = non_linux::CpalPlayout;

// SAFETY: The `UnsafeCell<HeapProd<i16>>` is only ever accessed from a single
// writer thread via `push_pcm`. The consumer half lives on the audio-backend
// callback thread and never touches `prod`. Because exactly one thread holds a
// `&mut` reference at any time, the Send + Sync impls are sound.
unsafe impl Send for Playout {}
unsafe impl Sync for Playout {}

impl Playout {
    pub fn start(sample_rate: u32, channels: u16) -> Result<Self> {
        Self::start_with_device(sample_rate, channels, None)
    }

    pub fn start_with_device(
        sample_rate: u32,
        channels: u16,
        preferred_device: Option<&str>,
    ) -> Result<Self> {
        Self::start_with_mode(sample_rate, channels, preferred_device, None)
    }

    pub fn start_with_mode(
        sample_rate: u32,
        channels: u16,
        preferred_device: Option<&str>,
        preferred_mode: Option<&str>,
    ) -> Result<Self> {
        let rb = HeapRb::<i16>::new(sample_rate as usize * channels as usize);
        let (prod, cons) = rb.split();

        #[cfg(target_os = "linux")]
        let backend = PlayoutBackend::start(
            sample_rate,
            channels,
            cons,
            preferred_device,
            preferred_mode,
        )?;

        #[cfg(not(target_os = "linux"))]
        let backend = PlayoutBackend::start(
            sample_rate,
            channels,
            cons,
            preferred_device,
            preferred_mode,
        )?;

        Ok(Self {
            backend,
            prod: UnsafeCell::new(prod),
        })
    }

    pub fn push_pcm(&self, pcm: &[i16]) {
        let _ = &self.backend;
        let prod = unsafe { &mut *self.prod.get() };
        for &s in pcm {
            let _ = prod.try_push(s);
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.backend.is_healthy()
    }
}

pub fn enumerate_output_devices() -> Vec<AudioDeviceInfo> {
    let mut devices = vec![AudioDeviceInfo {
        key: AudioDeviceId::default_output(),
        label: "Default (system)".to_string(),
        display_label: "Default (system)".to_string(),
        is_default: true,
    }];
    devices.extend(PlayoutBackend::enumerate_output_devices());
    disambiguate_display_labels(&mut devices);
    devices
}

pub fn enumerate_playback_modes() -> Vec<String> {
    PlayoutBackend::enumerate_playback_modes()
}

#[cfg(target_os = "windows")]
fn cpal_backend() -> AudioBackend {
    AudioBackend::Wasapi
}
#[cfg(target_os = "macos")]
fn cpal_backend() -> AudioBackend {
    AudioBackend::CoreAudio
}
#[cfg(target_os = "linux")]
fn cpal_backend() -> AudioBackend {
    AudioBackend::Pulse
}
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn cpal_backend() -> AudioBackend {
    AudioBackend::Unknown
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{anyhow, Context, Result};
    use cpal::{
        traits::DeviceTrait, traits::HostTrait, traits::StreamTrait, FromSample, SizedSample,
    };
    use pipewire as pw;
    use pw::properties::properties;
    use ringbuf::{traits::Consumer, HeapCons};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tracing::info;

    use crate::ui::model::{AudioDeviceId, AudioDeviceInfo, AudioDirection};

    enum LinuxPlayoutBackend {
        PipeWire,
        Pulse(CpalPlayout),
    }

    pub struct LinuxPlayout {
        _thread: Option<std::thread::JoinHandle<()>>,
        backend: LinuxPlayoutBackend,
    }

    impl LinuxPlayout {
        pub fn start(
            sample_rate: u32,
            channels: u16,
            cons: HeapCons<i16>,
            preferred_device: Option<&str>,
            preferred_mode: Option<&str>,
        ) -> Result<Self> {
            let prefer_pipewire = preferred_mode
                .map(|mode| mode == super::PLAYBACK_MODE_PIPEWIRE)
                .unwrap_or(false);
            let prefer_pulse = preferred_mode
                .map(|mode| mode == super::PLAYBACK_MODE_PULSEAUDIO)
                .unwrap_or(false);

            if !prefer_pulse && pipewire_is_available() {
                let thread = std::thread::Builder::new()
                    .name("tsod-pipewire-playout".to_string())
                    .spawn(move || {
                        if let Err(e) = run_pipewire_playout(
                            sample_rate,
                            channels,
                            cons,
                            preferred_device.map(str::to_string),
                        ) {
                            eprintln!("pipewire playout thread failed: {e:#}");
                        }
                    })
                    .context("spawn PipeWire playout thread")?;

                return Ok(Self {
                    _thread: Some(thread),
                    backend: LinuxPlayoutBackend::PipeWire,
                });
            }

            if prefer_pipewire {
                return Err(anyhow!("PipeWire playback mode requested but unavailable"));
            }

            eprintln!("PipeWire unavailable, falling back to PulseAudio playback via CPAL");
            let pulse = CpalPlayout::start(sample_rate, channels, cons, preferred_device)?;
            Ok(Self {
                _thread: None,
                backend: LinuxPlayoutBackend::Pulse(pulse),
            })
        }

        pub fn enumerate_output_devices() -> Vec<AudioDeviceInfo> {
            if pipewire_is_available() {
                return enumerate_pipewire_outputs();
            }
            CpalPlayout::enumerate_output_devices()
        }

        pub fn enumerate_playback_modes() -> Vec<String> {
            let mut modes = vec![super::PLAYBACK_MODE_AUTO.to_string()];
            if pipewire_is_available() {
                modes.push(super::PLAYBACK_MODE_PIPEWIRE.to_string());
            }
            if !CpalPlayout::enumerate_output_devices().is_empty() {
                modes.push(super::PLAYBACK_MODE_PULSEAUDIO.to_string());
            }
            modes
        }

        pub fn is_healthy(&self) -> bool {
            match &self.backend {
                LinuxPlayoutBackend::PipeWire => true,
                LinuxPlayoutBackend::Pulse(cpal) => cpal.is_healthy(),
            }
        }
    }

    fn pipewire_is_available() -> bool {
        pw::init();
        let Ok(mainloop) = pw::main_loop::MainLoopBox::new(None) else {
            return false;
        };
        let Ok(context) = pw::context::ContextBox::new(mainloop.loop_(), None) else {
            return false;
        };
        let result = context.connect(None).is_ok();
        result
    }

    fn run_pipewire_playout(
        sample_rate: u32,
        channels: u16,
        mut cons: HeapCons<i16>,
        preferred_device: Option<String>,
    ) -> Result<()> {
        pw::init();

        let mainloop = pw::main_loop::MainLoopBox::new(None).context("create PipeWire mainloop")?;
        let context = pw::context::ContextBox::new(mainloop.loop_(), None)
            .context("create PipeWire context")?;
        let core = context.connect(None).context("connect PipeWire core")?;

        let props = if let Some(target) = preferred_device.as_deref() {
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::TARGET_OBJECT => target,
            }
        } else {
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Communication",
            }
        };

        let stream = pw::stream::StreamBox::new(&core, "tsod-playout", props)
            .context("create PipeWire playout stream")?;

        let ch = channels;
        let listener = stream
            .add_local_listener_with_user_data(())
            .process({
                move |stream: &pw::stream::Stream, _: &mut ()| {
                    let Some(mut buf) = stream.dequeue_buffer() else {
                        return;
                    };

                    let datas = buf.datas_mut();
                    if datas.is_empty() {
                        return;
                    }

                    let Some(raw) = datas[0].data() else {
                        return;
                    };

                    let out = unsafe {
                        std::slice::from_raw_parts_mut(raw.as_mut_ptr() as *mut i16, raw.len() / 2)
                    };

                    if ch == 1 {
                        for o in out.iter_mut() {
                            *o = cons.try_pop().unwrap_or(0);
                        }
                    } else {
                        for frame in out.chunks_mut(ch as usize) {
                            let sample = cons.try_pop().unwrap_or(0);
                            for o in frame.iter_mut() {
                                *o = sample;
                            }
                        }
                    }
                }
            })
            .register()
            .context("register PipeWire playout listener")?;

        let mut info = pw::spa::param::audio::AudioInfoRaw::new();
        info.set_format(pw::spa::param::audio::AudioFormat::S16LE);
        info.set_rate(sample_rate);
        info.set_channels(channels as u32);

        let format_props: Vec<pw::spa::pod::Property> = info.into();
        let obj = pw::spa::pod::Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: pw::spa::param::ParamType::EnumFormat.as_raw(),
            properties: format_props,
        };
        let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(obj),
        )
        .map_err(|_| anyhow!("failed to serialize PipeWire playout format"))?
        .0
        .into_inner();

        let mut params = [pw::spa::pod::Pod::from_bytes(&values)
            .ok_or_else(|| anyhow!("failed to build PipeWire playout format"))?];

        stream
            .connect(
                pw::spa::utils::Direction::Output,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .context("connect PipeWire playout stream")?;

        let _listener = listener;
        mainloop.run();
        Ok(())
    }

    fn enumerate_pipewire_outputs() -> Vec<AudioDeviceInfo> {
        let Ok(output) = std::process::Command::new("pw-dump").output() else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
            return Vec::new();
        };
        let Some(entries) = json.as_array() else {
            return Vec::new();
        };

        let mut devices = Vec::new();
        for entry in entries {
            let Some(info) = entry.get("info") else {
                continue;
            };
            let Some(props) = info.get("props") else {
                continue;
            };
            let media_class = props
                .get("media.class")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if media_class != "Audio/Sink" {
                continue;
            }
            let node_name = props
                .get("node.name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if node_name.is_empty() {
                continue;
            }
            let label = props
                .get("node.description")
                .or_else(|| props.get("node.nick"))
                .or_else(|| props.get("node.name"))
                .and_then(|v| v.as_str())
                .unwrap_or(node_name)
                .to_string();
            devices.push(AudioDeviceInfo {
                key: AudioDeviceId {
                    backend: AudioBackend::PipeWire,
                    direction: AudioDirection::Output,
                    id: node_name.to_string(),
                },
                label: label.clone(),
                display_label: label,
                is_default: false,
            });
        }
        devices
    }

    use crate::audio::resample::LinearResampler;
    #[cfg(target_os = "windows")]
    use crate::audio::windows::mmdevice;

    struct CpalPlayout {
        _stream: cpal::Stream,
        unhealthy: Arc<AtomicBool>,
    }

    impl CpalPlayout {
        fn start(
            sample_rate: u32,
            channels: u16,
            cons: HeapCons<i16>,
            preferred_device: Option<&str>,
        ) -> Result<Self> {
            let host = cpal::default_host();
            let dev = if let Some(name) = preferred_device {
                find_output_device_by_id(&host, name)
                    .with_context(|| format!("output device '{name}' not found"))?
            } else {
                host.default_output_device()
                    .ok_or(anyhow!("no output device"))?
            };
            let selected_id = dev
                .id()
                .ok()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<unknown-id>".to_string());
            let selected_name = device_label(&dev).unwrap_or_else(|| "Unknown device".to_string());
            info!(endpoint_id = %selected_id, friendly_name = %selected_name, "starting output stream");
            let stream_cfg = native_output_config(&dev)?;
            let unhealthy = Arc::new(AtomicBool::new(false));
            let unhealthy_cb = unhealthy.clone();
            let stream = match stream_cfg.sample_format() {
                cpal::SampleFormat::I8 => build_output_stream::<i8>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I16 => build_output_stream::<i16>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I32 => build_output_stream::<i32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I64 => build_output_stream::<i64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U8 => build_output_stream::<u8>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U16 => build_output_stream::<u16>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U32 => build_output_stream::<u32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U64 => build_output_stream::<u64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::F32 => build_output_stream::<f32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::F64 => build_output_stream::<f64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                other => return Err(anyhow!("unsupported output sample format: {other:?}")),
            };
            stream.play()?;
            Ok(Self {
                _stream: stream,
                unhealthy,
            })
        }

        fn enumerate_output_devices() -> Vec<AudioDeviceInfo> {
            let host = cpal::default_host();
            host.output_devices()
                .map(|devs| devs.filter_map(|d| device_info(&d)).collect())
                .unwrap_or_default()
        }

        fn is_healthy(&self) -> bool {
            !self.unhealthy.load(Ordering::Relaxed)
        }
    }

    fn device_label(device: &cpal::Device) -> Option<String> {
        device
            .description()
            .ok()
            .map(|desc| desc.to_string())
            .filter(|name| !name.trim().is_empty())
    }

    fn device_info(device: &cpal::Device) -> Option<AudioDeviceInfo> {
        let id_str = device.id().ok()?.to_string();
        let label = device_label(device)?;
        Some(AudioDeviceInfo {
            key: AudioDeviceId {
                backend: super::cpal_backend(),
                direction: AudioDirection::Output,
                id: id_str,
            },
            label: label.clone(),
            display_label: label,
            is_default: false,
        })
    }

    fn find_output_device_by_id(host: &cpal::Host, id: &str) -> Result<cpal::Device> {
        if let Ok(device_id) = id.parse::<cpal::DeviceId>() {
            if let Some(device) = host.device_by_id(&device_id) {
                return Ok(device);
            }
        }

        for device in host
            .output_devices()
            .context("list output devices for id fallback")?
        {
            let Ok(current_id) = device.id() else {
                continue;
            };
            if current_id.to_string() == id {
                return Ok(device);
            }
        }

        Err(anyhow!("no matching output device id: {id}"))
    }

    #[cfg(target_os = "windows")]
    fn enumerate_mmdevice_output_devices() -> Vec<AudioDeviceInfo> {
        let host = cpal::default_host();
        let default_id = mmdevice::default_output_endpoint_id().ok().flatten();
        let mut devices = Vec::new();

        let endpoints = match mmdevice::enumerate_output_endpoints() {
            Ok(values) => values,
            Err(error) => {
                warn!("failed MMDevice output enumeration: {error:#}");
                return host
                    .output_devices()
                    .map(|devs| devs.filter_map(|d| device_info(&d)).collect())
                    .unwrap_or_default();
            }
        };

        for (endpoint_id, friendly_name) in endpoints {
            let openable = endpoint_id
                .parse::<cpal::DeviceId>()
                .ok()
                .and_then(|parsed| host.device_by_id(&parsed))
                .is_some();
            debug!(endpoint_id = %endpoint_id, friendly_name = %friendly_name, openable_via_cpal = openable, "enumerated output endpoint");
            if !openable {
                warn!(endpoint_id = %endpoint_id, friendly_name = %friendly_name, "skipping output endpoint because CPAL cannot open it");
                continue;
            }
            devices.push(AudioDeviceInfo {
                key: AudioDeviceId {
                    backend: super::cpal_backend(),
                    direction: AudioDirection::Output,
                    id: endpoint_id.clone(),
                },
                label: friendly_name.clone(),
                display_label: friendly_name,
                is_default: default_id.as_deref() == Some(endpoint_id.as_str()),
            });
        }

        devices
    }

    fn native_output_config(dev: &cpal::Device) -> Result<cpal::SupportedStreamConfig> {
        dev.default_output_config()
            .context("no supported output configuration")
    }

    fn build_output_stream<T>(
        dev: &cpal::Device,
        stream_cfg: &cpal::StreamConfig,
        source_rate: u32,
        source_channels: u16,
        mut cons: HeapCons<i16>,
        unhealthy: Arc<AtomicBool>,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample + FromSample<f32>,
    {
        let target_rate = stream_cfg.sample_rate;
        let target_channels = stream_cfg.channels.max(1) as usize;
        let source_channels = source_channels.max(1) as usize;
        let mut resampler = LinearResampler::new(source_rate, target_rate);
        let mut source_mono = Vec::<f32>::new();
        let mut source_resampled = Vec::<f32>::new();

        dev.build_output_stream(
            stream_cfg,
            move |data: &mut [T], _| {
                let frames_needed = data.len() / target_channels;
                if source_mono.len() < frames_needed {
                    source_mono.resize(frames_needed, 0.0);
                }
                for sample in source_mono.iter_mut().take(frames_needed) {
                    *sample = cons
                        .try_pop()
                        .map(|s| s as f32 / i16::MAX as f32)
                        .unwrap_or(0.0);
                    for _ in 1..source_channels {
                        let _ = cons.try_pop();
                    }
                }

                source_resampled.clear();
                resampler.process(&source_mono[..frames_needed], &mut source_resampled);

                let mut idx = 0usize;
                for frame in data.chunks_mut(target_channels) {
                    let s = source_resampled.get(idx).copied().unwrap_or(0.0);
                    let out = T::from_sample(s.clamp(-1.0, 1.0));
                    for ch in frame {
                        *ch = out;
                    }
                    idx += 1;
                }
            },
            move |err| {
                unhealthy.store(true, Ordering::Relaxed);
                eprintln!("playout err: {err}")
            },
            None,
        )
        .context("build output stream")
    }
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
mod non_linux {
    use anyhow::{anyhow, Context, Result};
    use cpal::{
        traits::DeviceTrait, traits::HostTrait, traits::StreamTrait, FromSample, SizedSample,
    };
    use ringbuf::{traits::Consumer, HeapCons};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tracing::{debug, info, warn};

    use crate::audio::resample::LinearResampler;
    #[cfg(target_os = "windows")]
    use crate::audio::windows::mmdevice;
    use crate::ui::model::{AudioDeviceId, AudioDeviceInfo, AudioDirection};

    pub struct CpalPlayout {
        _stream: cpal::Stream,
        unhealthy: Arc<AtomicBool>,
    }

    impl CpalPlayout {
        pub fn start(
            sample_rate: u32,
            channels: u16,
            cons: HeapCons<i16>,
            preferred_device: Option<&str>,
            _preferred_mode: Option<&str>,
        ) -> Result<Self> {
            let host = cpal::default_host();
            let dev = if let Some(name) = preferred_device {
                find_output_device_by_id(&host, name)
                    .with_context(|| format!("output device '{name}' not found"))?
            } else {
                host.default_output_device()
                    .ok_or(anyhow!("no output device"))?
            };
            let selected_id = dev
                .id()
                .ok()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<unknown-id>".to_string());
            let selected_name = device_label(&dev).unwrap_or_else(|| "Unknown device".to_string());
            info!(endpoint_id = %selected_id, friendly_name = %selected_name, "starting output stream");
            let stream_cfg = native_output_config(&dev)?;
            let unhealthy = Arc::new(AtomicBool::new(false));
            let unhealthy_cb = unhealthy.clone();

            let stream = match stream_cfg.sample_format() {
                cpal::SampleFormat::I8 => build_output_stream::<i8>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I16 => build_output_stream::<i16>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I32 => build_output_stream::<i32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I64 => build_output_stream::<i64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U8 => build_output_stream::<u8>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U16 => build_output_stream::<u16>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U32 => build_output_stream::<u32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U64 => build_output_stream::<u64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::F32 => build_output_stream::<f32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::F64 => build_output_stream::<f64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    cons,
                    unhealthy_cb,
                )?,
                other => return Err(anyhow!("unsupported output sample format: {other:?}")),
            };
            stream.play()?;
            Ok(Self {
                _stream: stream,
                unhealthy,
            })
        }

        pub fn enumerate_output_devices() -> Vec<AudioDeviceInfo> {
            #[cfg(target_os = "windows")]
            {
                return enumerate_mmdevice_output_devices();
            }

            #[cfg(not(target_os = "windows"))]
            {
                let host = cpal::default_host();
                return host
                    .output_devices()
                    .map(|devs| devs.filter_map(|d| device_info(&d)).collect())
                    .unwrap_or_default();
            }
        }

        pub fn is_healthy(&self) -> bool {
            !self.unhealthy.load(Ordering::Relaxed)
        }

        pub fn enumerate_playback_modes() -> Vec<String> {
            vec![
                super::PLAYBACK_MODE_AUTO.to_string(),
                super::PLAYBACK_MODE_WASAPI.to_string(),
            ]
        }
    }

    fn device_label(device: &cpal::Device) -> Option<String> {
        device
            .description()
            .ok()
            .map(|desc| desc.to_string())
            .filter(|name| !name.trim().is_empty())
    }

    fn device_info(device: &cpal::Device) -> Option<AudioDeviceInfo> {
        let id_str = device.id().ok()?.to_string();
        let label = device_label(device)?;
        Some(AudioDeviceInfo {
            key: AudioDeviceId {
                backend: super::cpal_backend(),
                direction: AudioDirection::Output,
                id: id_str,
            },
            label: label.clone(),
            display_label: label,
            is_default: false,
        })
    }

    fn find_output_device_by_id(host: &cpal::Host, id: &str) -> Result<cpal::Device> {
        if let Ok(device_id) = id.parse::<cpal::DeviceId>() {
            if let Some(device) = host.device_by_id(&device_id) {
                return Ok(device);
            }
        }

        for device in host
            .output_devices()
            .context("list output devices for id fallback")?
        {
            let Ok(current_id) = device.id() else {
                continue;
            };
            if current_id.to_string() == id {
                return Ok(device);
            }
        }

        Err(anyhow!("no matching output device id: {id}"))
    }

    #[cfg(target_os = "windows")]
    fn enumerate_mmdevice_output_devices() -> Vec<AudioDeviceInfo> {
        let host = cpal::default_host();
        let default_id = mmdevice::default_output_endpoint_id().ok().flatten();
        let mut devices = Vec::new();

        let endpoints = match mmdevice::enumerate_output_endpoints() {
            Ok(values) => values,
            Err(error) => {
                warn!("failed MMDevice output enumeration: {error:#}");
                return host
                    .output_devices()
                    .map(|devs| devs.filter_map(|d| device_info(&d)).collect())
                    .unwrap_or_default();
            }
        };

        for (endpoint_id, friendly_name) in endpoints {
            let openable = endpoint_id
                .parse::<cpal::DeviceId>()
                .ok()
                .and_then(|parsed| host.device_by_id(&parsed))
                .is_some();
            debug!(endpoint_id = %endpoint_id, friendly_name = %friendly_name, openable_via_cpal = openable, "enumerated output endpoint");
            if !openable {
                warn!(endpoint_id = %endpoint_id, friendly_name = %friendly_name, "skipping output endpoint because CPAL cannot open it");
                continue;
            }
            devices.push(AudioDeviceInfo {
                key: AudioDeviceId {
                    backend: super::cpal_backend(),
                    direction: AudioDirection::Output,
                    id: endpoint_id.clone(),
                },
                label: friendly_name.clone(),
                display_label: friendly_name,
                is_default: default_id.as_deref() == Some(endpoint_id.as_str()),
            });
        }

        devices
    }

    fn native_output_config(dev: &cpal::Device) -> Result<cpal::SupportedStreamConfig> {
        dev.default_output_config()
            .context("no supported output configuration")
    }

    fn build_output_stream<T>(
        dev: &cpal::Device,
        stream_cfg: &cpal::StreamConfig,
        source_rate: u32,
        source_channels: u16,
        mut cons: HeapCons<i16>,
        unhealthy: Arc<AtomicBool>,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample + FromSample<f32>,
    {
        let target_rate = stream_cfg.sample_rate;
        let target_channels = stream_cfg.channels.max(1) as usize;
        let source_channels = source_channels.max(1) as usize;

        let mut resampler = LinearResampler::new(source_rate, target_rate);
        let mut source_mono = Vec::<f32>::new();
        let mut resampled = Vec::<f32>::new();

        dev.build_output_stream(
            stream_cfg,
            move |out: &mut [T], _| {
                source_mono.clear();
                let needed_mono = out.len().div_ceil(target_channels);

                for _ in 0..needed_mono {
                    let mut sum = 0.0f32;
                    let mut count = 0usize;
                    for _ in 0..source_channels {
                        match cons.try_pop() {
                            Some(sample) => {
                                sum += sample as f32 / i16::MAX as f32;
                                count += 1;
                            }
                            None => break,
                        }
                    }
                    source_mono.push(if count == 0 { 0.0 } else { sum / count as f32 });
                }

                resampled.clear();
                resampler.process(&source_mono, &mut resampled);

                let mut idx = 0usize;
                for frame in out.chunks_mut(target_channels) {
                    let sample = resampled.get(idx).copied().unwrap_or(0.0);
                    idx += 1;
                    let converted = T::from_sample(sample.clamp(-1.0, 1.0));
                    for o in frame.iter_mut() {
                        *o = converted;
                    }
                }
            },
            move |err| {
                unhealthy.store(true, Ordering::Relaxed);
                eprintln!("playout err: {err}")
            },
            None,
        )
        .context("build output stream")
    }
}
