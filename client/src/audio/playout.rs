use anyhow::Result;
use ringbuf::{
    traits::{Producer, Split},
    HeapProd, HeapRb,
};
use std::sync::{Arc, Mutex};

pub struct Playout {
    backend: PlayoutBackend,
    prod: Arc<Mutex<HeapProd<i16>>>,
}

#[cfg(target_os = "linux")]
type PlayoutBackend = linux::LinuxPlayout;

#[cfg(not(target_os = "linux"))]
type PlayoutBackend = non_linux::CpalPlayout;

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
        let rb = HeapRb::<i16>::new(sample_rate as usize * channels as usize);
        let (prod, cons) = rb.split();

        #[cfg(target_os = "linux")]
        let backend = PlayoutBackend::start(sample_rate, channels, cons, preferred_device)?;

        #[cfg(not(target_os = "linux"))]
        let backend = {
            let cons = Arc::new(Mutex::new(cons));
            PlayoutBackend::start(sample_rate, channels, cons, preferred_device)?
        };

        let prod = Arc::new(Mutex::new(prod));

        Ok(Self { backend, prod })
    }

    pub fn push_pcm(&self, pcm: &[i16]) {
        let _ = &self.backend;
        if let Ok(mut p) = self.prod.lock() {
            for &s in pcm {
                let _ = p.try_push(s);
            }
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.backend.is_healthy()
    }
}

pub fn enumerate_output_devices() -> Vec<String> {
    PlayoutBackend::enumerate_output_devices()
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{anyhow, Context, Result};
    use pipewire as pw;
    use pw::properties::properties;
    use ringbuf::{traits::Consumer, HeapCons};

    pub struct LinuxPlayout {
        _thread: std::thread::JoinHandle<()>,
    }

    impl LinuxPlayout {
        pub fn start(
            sample_rate: u32,
            channels: u16,
            cons: HeapCons<i16>,
            _preferred_device: Option<&str>,
        ) -> Result<Self> {
            let thread = std::thread::Builder::new()
                .name("tsod-pipewire-playout".to_string())
                .spawn(move || {
                    if let Err(e) = run_pipewire_playout(sample_rate, channels, cons) {
                        eprintln!("pipewire playout thread failed: {e:#}");
                    }
                })
                .context("spawn PipeWire playout thread")?;

            Ok(Self { _thread: thread })
        }

        pub fn enumerate_output_devices() -> Vec<String> {
            vec!["PipeWire default output".to_string()]
        }

        pub fn is_healthy(&self) -> bool {
            true
        }
    }

    fn run_pipewire_playout(
        sample_rate: u32,
        channels: u16,
        mut cons: HeapCons<i16>,
    ) -> Result<()> {
        pw::init();

        let mainloop = pw::main_loop::MainLoopBox::new(None).context("create PipeWire mainloop")?;
        let context = pw::context::ContextBox::new(mainloop.loop_(), None)
            .context("create PipeWire context")?;
        let core = context.connect(None).context("connect PipeWire core")?;

        let stream = pw::stream::StreamBox::new(
            &core,
            "tsod-playout",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Communication",
            },
        )
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
    use cpal::{
        traits::DeviceTrait, traits::HostTrait, traits::StreamTrait, FromSample, Sample,
        SizedSample,
    };
    use ringbuf::{traits::Consumer, HeapCons};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };

    struct LinearResampler {
        step: f64,
        phase: f64,
        history: Vec<f32>,
    }

    impl LinearResampler {
        fn new(input_rate: u32, output_rate: u32) -> Self {
            Self {
                step: input_rate as f64 / output_rate as f64,
                phase: 0.0,
                history: Vec::new(),
            }
        }

        fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
            if input.is_empty() {
                return;
            }

            self.history.extend_from_slice(input);
            while self.phase + 1.0 < self.history.len() as f64 {
                let i0 = self.phase.floor() as usize;
                let i1 = i0 + 1;
                let frac = (self.phase - i0 as f64) as f32;
                let s0 = self.history[i0];
                let s1 = self.history[i1];
                out.push(s0 + (s1 - s0) * frac);
                self.phase += self.step;
            }

            let consumed = self.phase.floor() as usize;
            if consumed > 0 {
                self.history.drain(..consumed);
                self.phase -= consumed as f64;
            }
        }
    }

    pub struct CpalPlayout {
        _stream: cpal::Stream,
        unhealthy: Arc<AtomicBool>,
    }

    impl CpalPlayout {
        pub fn start(
            sample_rate: u32,
            channels: u16,
            cons: Arc<Mutex<HeapCons<i16>>>,
            preferred_device: Option<&str>,
        ) -> Result<Self> {
            let host = cpal::default_host();
            let dev = if let Some(name) = preferred_device {
                find_output_device_by_name(&host, name)
                    .with_context(|| format!("output device '{name}' not found"))?
            } else {
                host.default_output_device()
                    .ok_or(anyhow!("no output device"))?
            };
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
                cpal::SampleFormat::I24 => build_output_stream::<cpal::I24>(
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

        pub fn enumerate_output_devices() -> Vec<String> {
            let host = cpal::default_host();
            host.output_devices()
                .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
                .unwrap_or_default()
        }

        pub fn is_healthy(&self) -> bool {
            !self.unhealthy.load(Ordering::Relaxed)
        }
    }

    fn find_output_device_by_name(host: &cpal::Host, name: &str) -> Result<cpal::Device> {
        let mut devices = host.output_devices().context("enumerate output devices")?;
        devices
            .find(|dev| dev.name().ok().as_deref() == Some(name))
            .ok_or_else(|| anyhow!("no matching output device"))
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
        cons: Arc<Mutex<HeapCons<i16>>>,
        unhealthy: Arc<AtomicBool>,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample + FromSample<f32>,
    {
        let target_rate = stream_cfg.sample_rate.0;
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

                if let Ok(mut c) = cons.lock() {
                    for _ in 0..needed_mono {
                        let mut sum = 0.0f32;
                        let mut count = 0usize;
                        for _ in 0..source_channels {
                            match c.try_pop() {
                                Some(sample) => {
                                    sum += sample as f32 / i16::MAX as f32;
                                    count += 1;
                                }
                                None => break,
                            }
                        }
                        source_mono.push(if count == 0 { 0.0 } else { sum / count as f32 });
                    }
                } else {
                    source_mono.resize(needed_mono, 0.0);
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
