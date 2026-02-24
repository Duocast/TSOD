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

#[cfg(all(target_os = "linux", feature = "pipewire-backend"))]
type PlayoutBackend = linux::LinuxPlayout;

#[cfg(any(not(target_os = "linux"), not(feature = "pipewire-backend")))]
type PlayoutBackend = non_linux::CpalPlayout;

unsafe impl Send for Playout {}
unsafe impl Sync for Playout {}

impl Playout {
    pub fn start(sample_rate: u32, channels: u16) -> Result<Self> {
        let rb = HeapRb::<i16>::new(sample_rate as usize * channels as usize);
        let (prod, cons) = rb.split();

        #[cfg(all(target_os = "linux", feature = "pipewire-backend"))]
        let backend = PlayoutBackend::start(sample_rate, channels, cons)?;

        #[cfg(any(not(target_os = "linux"), not(feature = "pipewire-backend")))]
        let backend = {
            let cons = Arc::new(Mutex::new(cons));
            PlayoutBackend::start(sample_rate, channels, cons)?
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
}

pub fn enumerate_output_devices() -> Vec<String> {
    PlayoutBackend::enumerate_output_devices()
}

#[cfg(all(target_os = "linux", feature = "pipewire-backend"))]
mod linux {
    use anyhow::{anyhow, Context, Result};
    use pipewire as pw;
    use pw::prelude::*;
    use ringbuf::{traits::Consumer, HeapCons};

    pub struct LinuxPlayout {
        _thread: std::thread::JoinHandle<()>,
    }

    impl LinuxPlayout {
        pub fn start(sample_rate: u32, channels: u16, mut cons: HeapCons<i16>) -> Result<Self> {
            let thread = std::thread::Builder::new()
                .name("tsod-pipewire-playout".to_string())
                .spawn(move || {
                    if let Err(e) = run_pipewire_playout(sample_rate, channels, &mut cons) {
                        eprintln!("pipewire playout thread failed: {e:#}");
                    }
                })
                .context("spawn PipeWire playout thread")?;

            Ok(Self { _thread: thread })
        }

        pub fn enumerate_output_devices() -> Vec<String> {
            vec!["PipeWire default output".to_string()]
        }
    }

    fn run_pipewire_playout(
        sample_rate: u32,
        channels: u16,
        cons: &mut HeapCons<i16>,
    ) -> Result<()> {
        pw::init();

        let mainloop = pw::main_loop::MainLoop::new(None).context("create PipeWire mainloop")?;
        let context = pw::context::Context::new(&mainloop).context("create PipeWire context")?;
        let core = context.connect(None).context("connect PipeWire core")?;

        let stream = pw::stream::Stream::new(
            &core,
            "tsod-playout",
            pw::properties! {
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
                move |stream, _| {
                    let Some(mut buf) = stream.dequeue_buffer() else {
                        return;
                    };

                    let datas = buf.datas_mut();
                    if datas.is_empty() {
                        stream.queue_buffer(buf);
                        return;
                    }

                    let Some(raw) = datas[0].data_mut() else {
                        stream.queue_buffer(buf);
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
                .ok_or_else(|| anyhow!("failed to build PipeWire playout format"))?,
        ];

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

        let _keepalive = (context, core, stream, listener);
        let _ = mainloop.run();
        Ok(())
    }
}

#[cfg(any(not(target_os = "linux"), not(feature = "pipewire-backend")))]
mod non_linux {
    use anyhow::{anyhow, Context, Result};
    use cpal::{traits::DeviceTrait, traits::HostTrait, traits::StreamTrait};
    use ringbuf::{traits::Consumer, HeapCons};
    use std::sync::{Arc, Mutex};

    pub struct CpalPlayout {
        _stream: cpal::Stream,
    }

    impl CpalPlayout {
        pub fn start(
            sample_rate: u32,
            channels: u16,
            cons: Arc<Mutex<HeapCons<i16>>>,
        ) -> Result<Self> {
            let host = cpal::default_host();
            let dev = host
                .default_output_device()
                .ok_or(anyhow!("no output device"))?;
            let (stream_cfg, actual_channels) =
                compatible_output_config(&dev, sample_rate, channels)?;

            let target_ch = channels;
            let stream = dev.build_output_stream(
                &stream_cfg,
                move |out: &mut [i16], _| {
                    if let Ok(mut c) = cons.lock() {
                        if actual_channels == target_ch {
                            for o in out.iter_mut() {
                                *o = c.try_pop().unwrap_or(0);
                            }
                        } else {
                            for frame in out.chunks_mut(actual_channels as usize) {
                                let sample = c.try_pop().unwrap_or(0);
                                for o in frame.iter_mut() {
                                    *o = sample;
                                }
                            }
                        }
                    }
                },
                move |err| eprintln!("playout err: {err}"),
                None,
            )?;
            stream.play()?;
            Ok(Self { _stream: stream })
        }

        pub fn enumerate_output_devices() -> Vec<String> {
            let host = cpal::default_host();
            host.output_devices()
                .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
                .unwrap_or_default()
        }
    }

    fn compatible_output_config(
        dev: &cpal::Device,
        target_rate: u32,
        target_channels: u16,
    ) -> Result<(cpal::StreamConfig, u16)> {
        if let Ok(ranges) = dev.supported_output_configs() {
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

        if let Ok(ranges) = dev.supported_output_configs() {
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
            .default_output_config()
            .context("no supported output configuration")?;
        let ch = default.channels();
        Ok((default.config(), ch))
    }
}
