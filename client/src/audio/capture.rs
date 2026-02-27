use anyhow::Result;
use ringbuf::{
    traits::{Consumer, Split},
    HeapCons, HeapRb,
};
use std::cell::UnsafeCell;

pub struct Capture {
    backend: CaptureBackend,
    cons: UnsafeCell<HeapCons<i16>>,
    frame_samples: usize,
}

#[cfg(target_os = "linux")]
type CaptureBackend = linux::LinuxCapture;

#[cfg(not(target_os = "linux"))]
type CaptureBackend = non_linux::CpalCapture;

// SAFETY: The `UnsafeCell<HeapCons<i16>>` is only ever accessed from a single
// reader thread via `read_frame`. The producer half lives on the audio-backend
// callback thread and never touches `cons`. Because exactly one thread holds a
// `&mut` reference at any time, the Send + Sync impls are sound.
unsafe impl Send for Capture {}
unsafe impl Sync for Capture {}

impl Capture {
    pub fn start(sample_rate: u32, channels: u16, frame_ms: u32) -> Result<Self> {
        Self::start_with_device(sample_rate, channels, frame_ms, None)
    }

    pub fn start_with_device(
        sample_rate: u32,
        channels: u16,
        frame_ms: u32,
        preferred_device: Option<&str>,
    ) -> Result<Self> {
        let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels as usize;
        let rb = HeapRb::<i16>::new(frame_samples * 50);
        let (prod, cons) = rb.split();

        #[cfg(target_os = "linux")]
        let backend = CaptureBackend::start(sample_rate, channels, prod, preferred_device)?;

        #[cfg(not(target_os = "linux"))]
        let backend = CaptureBackend::start(sample_rate, channels, prod, preferred_device)?;

        Ok(Self {
            backend,
            cons: UnsafeCell::new(cons),
            frame_samples,
        })
    }

    pub fn read_frame(&self, out: &mut [i16]) -> bool {
        let _ = &self.backend;
        if out.len() != self.frame_samples {
            return false;
        }
        let mut got = 0usize;
        let cons = unsafe { &mut *self.cons.get() };
        while got < out.len() {
            if let Some(v) = cons.try_pop() {
                out[got] = v;
                got += 1;
            } else {
                break;
            }
        }
        got == out.len()
    }

    pub fn is_healthy(&self) -> bool {
        self.backend.is_healthy()
    }
}

pub fn enumerate_input_devices() -> Vec<String> {
    CaptureBackend::enumerate_input_devices()
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{anyhow, Context, Result};
    use cpal::{traits::DeviceTrait, traits::HostTrait, traits::StreamTrait, SizedSample};
    use pipewire as pw;
    use pw::properties::properties;
    use ringbuf::{traits::Producer, HeapProd};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    enum LinuxCaptureBackend {
        PipeWire,
        Pulse(CpalCapture),
    }

    pub struct LinuxCapture {
        _thread: Option<std::thread::JoinHandle<()>>,
        backend: LinuxCaptureBackend,
    }

    impl LinuxCapture {
        pub fn start(
            sample_rate: u32,
            channels: u16,
            prod: HeapProd<i16>,
            preferred_device: Option<&str>,
        ) -> Result<Self> {
            if pipewire_is_available() {
                let thread = std::thread::Builder::new()
                    .name("tsod-pipewire-capture".to_string())
                    .spawn(move || {
                        if let Err(e) = run_pipewire_capture(sample_rate, channels, prod) {
                            eprintln!("pipewire capture thread failed: {e:#}");
                        }
                    })
                    .context("spawn PipeWire capture thread")?;

                return Ok(Self {
                    _thread: Some(thread),
                    backend: LinuxCaptureBackend::PipeWire,
                });
            }

            eprintln!("PipeWire unavailable, falling back to PulseAudio capture via CPAL");
            let pulse = CpalCapture::start(sample_rate, channels, prod, preferred_device)?;
            Ok(Self {
                _thread: None,
                backend: LinuxCaptureBackend::Pulse(pulse),
            })
        }

        pub fn enumerate_input_devices() -> Vec<String> {
            if pipewire_is_available() {
                return vec!["PipeWire default input".to_string()];
            }
            CpalCapture::enumerate_input_devices()
        }

        pub fn is_healthy(&self) -> bool {
            match &self.backend {
                LinuxCaptureBackend::PipeWire => true,
                LinuxCaptureBackend::Pulse(cpal) => cpal.is_healthy(),
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

    fn run_pipewire_capture(
        sample_rate: u32,
        channels: u16,
        mut prod: HeapProd<i16>,
    ) -> Result<()> {
        pw::init();

        let mainloop = pw::main_loop::MainLoopBox::new(None).context("create PipeWire mainloop")?;
        let context = pw::context::ContextBox::new(mainloop.loop_(), None)
            .context("create PipeWire context")?;
        let core = context.connect(None).context("connect PipeWire core")?;

        let stream = pw::stream::StreamBox::new(
            &core,
            "tsod-capture",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Communication",
            },
        )
        .context("create PipeWire capture stream")?;

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

                    let samples = unsafe {
                        std::slice::from_raw_parts(raw.as_ptr() as *const i16, raw.len() / 2)
                    };

                    if ch == 1 {
                        for &s in samples {
                            let _ = prod.try_push(s);
                        }
                    } else {
                        for frame in samples.chunks(ch as usize) {
                            if let Some(&s) = frame.first() {
                                let _ = prod.try_push(s);
                            }
                        }
                    }
                }
            })
            .register()
            .context("register PipeWire capture listener")?;

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
        .map_err(|_| anyhow!("failed to serialize PipeWire capture format"))?
        .0
        .into_inner();

        let mut params = [pw::spa::pod::Pod::from_bytes(&values)
            .ok_or_else(|| anyhow!("failed to build PipeWire capture format"))?];

        stream
            .connect(
                pw::spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .context("connect PipeWire capture stream")?;

        let _listener = listener;
        mainloop.run();
        Ok(())
    }

    use crate::audio::resample::LinearResampler;

    struct CpalCapture {
        _stream: cpal::Stream,
        unhealthy: Arc<AtomicBool>,
    }

    impl CpalCapture {
        fn start(
            sample_rate: u32,
            channels: u16,
            prod: HeapProd<i16>,
            preferred_device: Option<&str>,
        ) -> Result<Self> {
            let host = cpal::default_host();
            let dev = if let Some(name) = preferred_device {
                find_input_device_by_name(&host, name)
                    .with_context(|| format!("input device '{name}' not found"))?
            } else {
                host.default_input_device()
                    .ok_or(anyhow!("no input device"))?
            };
            let stream_cfg = native_input_config(&dev)?;
            let unhealthy = Arc::new(AtomicBool::new(false));
            let unhealthy_cb = unhealthy.clone();
            let stream = match stream_cfg.sample_format() {
                cpal::SampleFormat::I8 => build_input_stream::<i8>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I16 => build_input_stream::<i16>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I32 => build_input_stream::<i32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I64 => build_input_stream::<i64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U8 => build_input_stream::<u8>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U16 => build_input_stream::<u16>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U32 => build_input_stream::<u32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U64 => build_input_stream::<u64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::F32 => build_input_stream::<f32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::F64 => build_input_stream::<f64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                other => return Err(anyhow!("unsupported input sample format: {other:?}")),
            };
            stream.play()?;
            Ok(Self {
                _stream: stream,
                unhealthy,
            })
        }

        fn enumerate_input_devices() -> Vec<String> {
            let host = cpal::default_host();
            host.input_devices()
                .map(|devs| devs.filter_map(|d| device_name(&d)).collect())
                .unwrap_or_default()
        }

        fn is_healthy(&self) -> bool {
            !self.unhealthy.load(Ordering::Relaxed)
        }
    }

    fn device_name(device: &cpal::Device) -> Option<String> {
        let description = device.description().ok();
        if let Some(desc) = description {
            let base = desc.name().trim();
            if base.is_empty() {
                return None;
            }

            let extended = desc
                .extended()
                .iter()
                .map(|line| line.trim())
                .find(|line| !line.is_empty() && *line != base);
            if let Some(name) = extended {
                return Some(name.to_string());
            }

            if let Some(driver) = desc.driver().map(str::trim) {
                if !driver.is_empty() && driver != base {
                    return Some(driver.to_string());
                }
            }

            if let Some(manufacturer) = desc.manufacturer().map(str::trim) {
                if !manufacturer.is_empty()
                    && !base
                        .to_ascii_lowercase()
                        .contains(&manufacturer.to_ascii_lowercase())
                {
                    return Some(format!("{base} ({manufacturer})"));
                }
            }

            return Some(base.to_string());
        }

        device.name().ok().filter(|name| !name.trim().is_empty())
    }

    fn find_input_device_by_name(host: &cpal::Host, name: &str) -> Result<cpal::Device> {
        let mut devices = host.input_devices().context("enumerate input devices")?;
        devices
            .find(|dev| device_name(dev).as_deref() == Some(name))
            .ok_or_else(|| anyhow!("no matching input device"))
    }

    fn native_input_config(dev: &cpal::Device) -> Result<cpal::SupportedStreamConfig> {
        dev.default_input_config()
            .context("no supported input configuration")
    }

    fn build_input_stream<T>(
        dev: &cpal::Device,
        stream_cfg: &cpal::StreamConfig,
        target_rate: u32,
        target_channels: u16,
        mut prod: HeapProd<i16>,
        unhealthy: Arc<AtomicBool>,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample,
        f32: cpal::FromSample<T>,
    {
        let source_rate = stream_cfg.sample_rate;
        let source_channels = stream_cfg.channels.max(1) as usize;
        let target_channels = target_channels.max(1) as usize;
        let mut resampler = LinearResampler::new(source_rate, target_rate);
        let mut mono = Vec::<f32>::new();
        let mut resampled = Vec::<f32>::new();

        dev.build_input_stream(
            stream_cfg,
            move |data: &[T], _| {
                mono.clear();
                mono.reserve(data.len() / source_channels + 1);
                for frame in data.chunks(source_channels) {
                    if frame.is_empty() {
                        continue;
                    }
                    let mut sum = 0.0f32;
                    for &sample in frame {
                        sum += sample.to_sample::<f32>();
                    }
                    mono.push(sum / frame.len() as f32);
                }

                resampled.clear();
                resampler.process(&mono, &mut resampled);

                for &s in &resampled {
                    let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
                    for _ in 0..target_channels {
                        let _ = prod.try_push(v);
                    }
                }
            },
            move |err| {
                unhealthy.store(true, Ordering::Relaxed);
                eprintln!("capture err: {err}")
            },
            None,
        )
        .context("build input stream")
    }
}

#[cfg(not(target_os = "linux"))]
mod non_linux {
    use anyhow::{anyhow, Context, Result};
    use cpal::{traits::DeviceTrait, traits::HostTrait, traits::StreamTrait, SizedSample};
    use ringbuf::{traits::Producer, HeapProd};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    use crate::audio::resample::LinearResampler;

    pub struct CpalCapture {
        _stream: cpal::Stream,
        unhealthy: Arc<AtomicBool>,
    }

    impl CpalCapture {
        pub fn start(
            sample_rate: u32,
            channels: u16,
            prod: HeapProd<i16>,
            preferred_device: Option<&str>,
        ) -> Result<Self> {
            let host = cpal::default_host();
            let dev = if let Some(name) = preferred_device {
                find_input_device_by_name(&host, name)
                    .with_context(|| format!("input device '{name}' not found"))?
            } else {
                host.default_input_device()
                    .ok_or(anyhow!("no input device"))?
            };
            let stream_cfg = native_input_config(&dev)?;
            let unhealthy = Arc::new(AtomicBool::new(false));
            let unhealthy_cb = unhealthy.clone();
            let stream = match stream_cfg.sample_format() {
                cpal::SampleFormat::I8 => build_input_stream::<i8>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I16 => build_input_stream::<i16>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I32 => build_input_stream::<i32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::I64 => build_input_stream::<i64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U8 => build_input_stream::<u8>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U16 => build_input_stream::<u16>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U32 => build_input_stream::<u32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::U64 => build_input_stream::<u64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::F32 => build_input_stream::<f32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                cpal::SampleFormat::F64 => build_input_stream::<f64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                )?,
                other => return Err(anyhow!("unsupported input sample format: {other:?}")),
            };
            stream.play()?;
            Ok(Self {
                _stream: stream,
                unhealthy,
            })
        }

        pub fn enumerate_input_devices() -> Vec<String> {
            let host = cpal::default_host();
            host.input_devices()
                .map(|devs| devs.filter_map(|d| device_name(&d)).collect())
                .unwrap_or_default()
        }

        pub fn is_healthy(&self) -> bool {
            !self.unhealthy.load(Ordering::Relaxed)
        }
    }

    fn device_name(device: &cpal::Device) -> Option<String> {
        let description = device.description().ok();
        if let Some(desc) = description {
            let base = desc.name().trim();
            if base.is_empty() {
                return None;
            }

            let extended = desc
                .extended()
                .iter()
                .map(|line| line.trim())
                .find(|line| !line.is_empty() && *line != base);
            if let Some(name) = extended {
                return Some(name.to_string());
            }

            if let Some(driver) = desc.driver().map(str::trim) {
                if !driver.is_empty() && driver != base {
                    return Some(driver.to_string());
                }
            }

            if let Some(manufacturer) = desc.manufacturer().map(str::trim) {
                if !manufacturer.is_empty()
                    && !base
                        .to_ascii_lowercase()
                        .contains(&manufacturer.to_ascii_lowercase())
                {
                    return Some(format!("{base} ({manufacturer})"));
                }
            }

            return Some(base.to_string());
        }

        device.name().ok().filter(|name| !name.trim().is_empty())
    }

    fn find_input_device_by_name(host: &cpal::Host, name: &str) -> Result<cpal::Device> {
        let mut devices = host.input_devices().context("enumerate input devices")?;
        devices
            .find(|dev| device_name(dev).as_deref() == Some(name))
            .ok_or_else(|| anyhow!("no matching input device"))
    }

    fn native_input_config(dev: &cpal::Device) -> Result<cpal::SupportedStreamConfig> {
        dev.default_input_config()
            .context("no supported input configuration")
    }

    fn build_input_stream<T>(
        dev: &cpal::Device,
        stream_cfg: &cpal::StreamConfig,
        target_rate: u32,
        target_channels: u16,
        mut prod: HeapProd<i16>,
        unhealthy: Arc<AtomicBool>,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample,
        f32: cpal::FromSample<T>,
    {
        let source_rate = stream_cfg.sample_rate;
        let source_channels = stream_cfg.channels.max(1) as usize;
        let target_channels = target_channels.max(1) as usize;
        let mut resampler = LinearResampler::new(source_rate, target_rate);
        let mut mono = Vec::<f32>::new();
        let mut resampled = Vec::<f32>::new();

        dev.build_input_stream(
            stream_cfg,
            move |data: &[T], _| {
                mono.clear();
                mono.reserve(data.len() / source_channels + 1);
                for frame in data.chunks(source_channels) {
                    if frame.is_empty() {
                        continue;
                    }
                    let mut sum = 0.0f32;
                    for &sample in frame {
                        sum += sample.to_sample::<f32>();
                    }
                    mono.push(sum / frame.len() as f32);
                }

                resampled.clear();
                resampler.process(&mono, &mut resampled);

                for &s in &resampled {
                    let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
                    for _ in 0..target_channels {
                        let _ = prod.try_push(v);
                    }
                }
            },
            move |err| {
                unhealthy.store(true, Ordering::Relaxed);
                eprintln!("capture err: {err}")
            },
            None,
        )
        .context("build input stream")
    }
}
