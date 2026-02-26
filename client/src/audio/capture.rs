use anyhow::Result;
use ringbuf::{
    traits::{Consumer, Split},
    HeapCons, HeapRb,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

pub struct Capture {
    backend: CaptureBackend,
    cons: Arc<Mutex<HeapCons<i16>>>,
    frame_samples: usize,
}

#[cfg(target_os = "linux")]
type CaptureBackend = linux::LinuxCapture;

#[cfg(not(target_os = "linux"))]
type CaptureBackend = non_linux::CpalCapture;

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
        let backend = {
            let prod = Arc::new(Mutex::new(prod));
            CaptureBackend::start(sample_rate, channels, prod, preferred_device)?
        };

        let cons = Arc::new(Mutex::new(cons));

        Ok(Self {
            backend,
            cons,
            frame_samples,
        })
    }

    pub fn read_frame(&self, out: &mut [i16]) -> bool {
        let _ = &self.backend;
        if out.len() != self.frame_samples {
            return false;
        }
        let mut got = 0usize;
        if let Ok(mut c) = self.cons.lock() {
            while got < out.len() {
                if let Some(v) = c.try_pop() {
                    out[got] = v;
                    got += 1;
                } else {
                    break;
                }
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
    use pipewire as pw;
    use pw::properties::properties;
    use ringbuf::{traits::Producer, HeapProd};

    pub struct LinuxCapture {
        _thread: std::thread::JoinHandle<()>,
    }

    impl LinuxCapture {
        pub fn start(
            sample_rate: u32,
            channels: u16,
            prod: HeapProd<i16>,
            _preferred_device: Option<&str>,
        ) -> Result<Self> {
            let thread = std::thread::Builder::new()
                .name("tsod-pipewire-capture".to_string())
                .spawn(move || {
                    if let Err(e) = run_pipewire_capture(sample_rate, channels, prod) {
                        eprintln!("pipewire capture thread failed: {e:#}");
                    }
                })
                .context("spawn PipeWire capture thread")?;

            Ok(Self { _thread: thread })
        }

        pub fn enumerate_input_devices() -> Vec<String> {
            vec!["PipeWire default input".to_string()]
        }

        pub fn is_healthy(&self) -> bool {
            true
        }
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

        // All PipeWire objects (stream, listener, core, context) stay alive
        // as local variables until mainloop.run() returns, then drop in
        // reverse declaration order.
        let _listener = listener;
        mainloop.run();
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod non_linux {
    use anyhow::{anyhow, Context, Result};
    use cpal::{traits::DeviceTrait, traits::HostTrait, traits::StreamTrait};
    use ringbuf::{traits::Producer, HeapProd};
    use std::sync::{Arc, Mutex};

    pub struct CpalCapture {
        _stream: cpal::Stream,
        unhealthy: Arc<AtomicBool>,
    }

    impl CpalCapture {
        pub fn start(
            sample_rate: u32,
            channels: u16,
            prod: Arc<Mutex<HeapProd<i16>>>,
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
            let (stream_cfg, actual_channels) =
                compatible_input_config(&dev, sample_rate, channels)?;
            let unhealthy = Arc::new(AtomicBool::new(false));
            let unhealthy_cb = unhealthy.clone();

            let target_ch = channels;
            let stream = dev.build_input_stream(
                &stream_cfg,
                move |data: &[i16], _| {
                    if let Ok(mut p) = prod.lock() {
                        if actual_channels == target_ch {
                            for &s in data {
                                let _ = p.try_push(s);
                            }
                        } else {
                            for chunk in data.chunks(actual_channels as usize) {
                                if let Some(&s) = chunk.first() {
                                    let _ = p.try_push(s);
                                }
                            }
                        }
                    }
                },
                move |err| {
                    unhealthy_cb.store(true, Ordering::Relaxed);
                    eprintln!("capture err: {err}")
                },
                None,
            )?;
            stream.play()?;
            Ok(Self {
                _stream: stream,
                unhealthy,
            })
        }

        pub fn enumerate_input_devices() -> Vec<String> {
            let host = cpal::default_host();
            host.input_devices()
                .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
                .unwrap_or_default()
        }

        pub fn is_healthy(&self) -> bool {
            !self.unhealthy.load(Ordering::Relaxed)
        }
    }

    fn find_input_device_by_name(host: &cpal::Host, name: &str) -> Result<cpal::Device> {
        let mut devices = host.input_devices().context("enumerate input devices")?;
        devices
            .find(|dev| dev.name().ok().as_deref() == Some(name))
            .ok_or_else(|| anyhow!("no matching input device"))
    }

    fn compatible_input_config(
        dev: &cpal::Device,
        target_rate: u32,
        target_channels: u16,
    ) -> Result<(cpal::StreamConfig, u16)> {
        if let Ok(ranges) = dev.supported_input_configs() {
            for range in ranges {
                if range.channels() == target_channels
                    && range.min_sample_rate().0 <= target_rate
                    && range.max_sample_rate().0 >= target_rate
                {
                    return Ok((
                        cpal::StreamConfig {
                            channels: target_channels,
                            sample_rate: cpal::SampleRate(target_rate),
                            buffer_size: cpal::BufferSize::Default,
                        },
                        target_channels,
                    ));
                }
            }
        }

        if let Ok(ranges) = dev.supported_input_configs() {
            for range in ranges {
                if range.min_sample_rate().0 <= target_rate
                    && range.max_sample_rate().0 >= target_rate
                {
                    let ch = range.channels();
                    return Ok((
                        cpal::StreamConfig {
                            channels: ch,
                            sample_rate: cpal::SampleRate(target_rate),
                            buffer_size: cpal::BufferSize::Default,
                        },
                        ch,
                    ));
                }
            }
        }

        let default = dev
            .default_input_config()
            .context("no supported input configuration")?;
        let ch = default.channels();
        Ok((default.config(), ch))
    }
}
