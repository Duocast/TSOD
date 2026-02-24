use anyhow::Result;
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapCons, HeapRb,
};
use std::sync::{Arc, Mutex};

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
        let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels as usize;
        let rb = HeapRb::<i16>::new(frame_samples * 50);
        let (prod, cons) = rb.split();

        #[cfg(target_os = "linux")]
        let backend = CaptureBackend::start(sample_rate, channels, prod)?;

        #[cfg(not(target_os = "linux"))]
        let backend = {
            let prod = Arc::new(Mutex::new(prod));
            CaptureBackend::start(sample_rate, channels, prod)?
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
}

pub fn enumerate_input_devices() -> Vec<String> {
    CaptureBackend::enumerate_input_devices()
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{anyhow, Context, Result};
    use pipewire as pw;
    use pw::prelude::*;
    use ringbuf::{traits::Producer, HeapProd};

    pub struct LinuxCapture {
        _thread: std::thread::JoinHandle<()>,
    }

    impl LinuxCapture {
        pub fn start(sample_rate: u32, channels: u16, mut prod: HeapProd<i16>) -> Result<Self> {
            let thread = std::thread::Builder::new()
                .name("tsod-pipewire-capture".to_string())
                .spawn(move || {
                    if let Err(e) = run_pipewire_capture(sample_rate, channels, &mut prod) {
                        eprintln!("pipewire capture thread failed: {e:#}");
                    }
                })
                .context("spawn PipeWire capture thread")?;

            Ok(Self { _thread: thread })
        }

        pub fn enumerate_input_devices() -> Vec<String> {
            vec!["PipeWire default input".to_string()]
        }
    }

    fn run_pipewire_capture(
        sample_rate: u32,
        channels: u16,
        prod: &mut HeapProd<i16>,
    ) -> Result<()> {
        pw::init();

        let mainloop = pw::main_loop::MainLoop::new(None).context("create PipeWire mainloop")?;
        let context = pw::context::Context::new(&mainloop).context("create PipeWire context")?;
        let core = context.connect(None).context("connect PipeWire core")?;

        let stream = pw::stream::Stream::new(
            &core,
            "tsod-capture",
            pw::properties! {
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
                move |stream, _| {
                    let Some(mut buf) = stream.dequeue_buffer() else {
                        return;
                    };

                    let datas = buf.datas_mut();
                    if datas.is_empty() {
                        stream.queue_buffer(buf);
                        return;
                    }

                    let Some(raw) = datas[0].data() else {
                        stream.queue_buffer(buf);
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

                    stream.queue_buffer(buf);
                }
            })
            .register();

        let mut info = pw::spa::param::audio::AudioInfoRaw::new();
        info.set_format(Some(pw::spa::param::audio::AudioFormat::S16LE));
        info.set_rate(sample_rate as i32);
        info.set_channels(channels as i32);

        let mut params = [
            pw::spa::pod::Pod::from(&pw::spa::param::audio::AudioInfo::Raw(info))
                .ok_or_else(|| anyhow!("failed to build PipeWire capture format"))?,
        ];

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

        let _keepalive = (context, core, stream, listener);
        let _ = mainloop.run();
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
    }

    impl CpalCapture {
        pub fn start(
            sample_rate: u32,
            channels: u16,
            prod: Arc<Mutex<HeapProd<i16>>>,
        ) -> Result<Self> {
            let host = cpal::default_host();
            let dev = host
                .default_input_device()
                .ok_or(anyhow!("no input device"))?;
            let (stream_cfg, actual_channels) =
                compatible_input_config(&dev, sample_rate, channels)?;

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
                move |err| eprintln!("capture err: {err}"),
                None,
            )?;
            stream.play()?;
            Ok(Self { _stream: stream })
        }

        pub fn enumerate_input_devices() -> Vec<String> {
            let host = cpal::default_host();
            host.input_devices()
                .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
                .unwrap_or_default()
        }
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
