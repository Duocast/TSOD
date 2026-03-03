use anyhow::Result;
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use ringbuf::{
    traits::{Consumer, Split},
    HeapCons, HeapRb,
};

use crate::ui::{
    model::{disambiguate_display_labels, AudioBackend, AudioDeviceId, AudioDeviceInfo},
    UiEvent,
};

struct CaptureConsState {
    cons: HeapCons<i16>,
    stash: Vec<i16>,
    underflow_counter: usize,
}

pub struct Capture {
    backend: CaptureBackend,
    cons: Mutex<CaptureConsState>,
    frame_samples: usize,
}

pub const CAPTURE_MODE_AUTO: &str = "Automatically use best mode";
pub const CAPTURE_MODE_PIPEWIRE: &str = "PipeWire";
pub const CAPTURE_MODE_PULSEAUDIO: &str = "PulseAudio";
pub const CAPTURE_MODE_WASAPI: &str = "WASAPI";

#[cfg(target_os = "linux")]
type CaptureBackend = linux::LinuxCapture;

#[cfg(target_os = "windows")]
type CaptureBackend = crate::audio::windows::wasapi_capture::WasapiCapture;

#[cfg(target_os = "macos")]
type CaptureBackend = non_linux::CpalCapture;

#[cfg(all(
    not(target_os = "linux"),
    not(target_os = "windows"),
    not(target_os = "macos")
))]
type CaptureBackend = non_linux::CpalCapture;

impl Capture {
    pub fn start(sample_rate: u32, channels: u16, frame_ms: u32) -> Result<Self> {
        Self::start_with_device(sample_rate, channels, frame_ms, None, None)
    }

    pub fn start_with_device(
        sample_rate: u32,
        channels: u16,
        frame_ms: u32,
        preferred_device: Option<&str>,
        tx_event: Option<Sender<UiEvent>>,
    ) -> Result<Self> {
        Self::start_with_mode(
            sample_rate,
            channels,
            frame_ms,
            preferred_device,
            None,
            tx_event,
        )
    }

    pub fn start_with_mode(
        sample_rate: u32,
        channels: u16,
        frame_ms: u32,
        preferred_device: Option<&str>,
        preferred_mode: Option<&str>,
        tx_event: Option<Sender<UiEvent>>,
    ) -> Result<Self> {
        let frame_samples = (sample_rate as usize * frame_ms as usize / 1000) * channels as usize;
        let rb = HeapRb::<i16>::new(frame_samples * 50);
        let (prod, cons) = rb.split();

        #[cfg(target_os = "linux")]
        let backend = CaptureBackend::start(
            sample_rate,
            channels,
            prod,
            preferred_device,
            preferred_mode,
            tx_event,
        )?;

        #[cfg(not(target_os = "linux"))]
        let backend = CaptureBackend::start(
            sample_rate,
            channels,
            prod,
            preferred_device,
            preferred_mode,
            tx_event,
        )?;

        Ok(Self {
            backend,
            cons: Mutex::new(CaptureConsState {
                cons,
                stash: Vec::with_capacity(frame_samples * 2),
                underflow_counter: 0,
            }),
            frame_samples,
        })
    }

    pub fn read_frame(&self, out: &mut [i16]) -> bool {
        let _ = &self.backend;
        if out.len() != self.frame_samples {
            return false;
        }
        let mut state = self.cons.lock();

        let mut tmp = Vec::with_capacity(self.frame_samples);
        if !state.stash.is_empty() {
            let take = state.stash.len().min(self.frame_samples);
            tmp.extend_from_slice(&state.stash[..take]);
            state.stash.drain(..take);
        }

        while tmp.len() < self.frame_samples {
            if let Some(v) = state.cons.try_pop() {
                tmp.push(v);
            } else {
                break;
            }
        }

        if tmp.len() < self.frame_samples {
            let mut new_stash = tmp;
            new_stash.extend_from_slice(&state.stash);
            state.stash = new_stash;
            state.underflow_counter += 1;
            if state.underflow_counter == 200 {
                tracing::warn!(
                    "[audio] capture underflow: waiting for full frame (stash_len={} frame={})",
                    state.stash.len(),
                    self.frame_samples
                );
            }
            return false;
        }

        out.copy_from_slice(&tmp[..self.frame_samples]);
        if tmp.len() > self.frame_samples {
            state.stash.extend_from_slice(&tmp[self.frame_samples..]);
        }
        state.underflow_counter = 0;
        true
    }

    pub fn is_healthy(&self) -> bool {
        self.backend.is_healthy()
    }
}

pub fn enumerate_input_devices() -> Vec<AudioDeviceInfo> {
    let mut devices = vec![AudioDeviceInfo {
        key: AudioDeviceId::default_input(),
        label: "Default (system)".to_string(),
        display_label: "Default (system)".to_string(),
        is_default: true,
    }];
    devices.extend(CaptureBackend::enumerate_input_devices());
    disambiguate_display_labels(&mut devices);
    devices
}

pub fn enumerate_capture_modes() -> Vec<String> {
    CaptureBackend::enumerate_capture_modes()
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
    use cpal::{traits::DeviceTrait, traits::HostTrait, traits::StreamTrait, SizedSample};
    use crossbeam_channel::Sender;
    use pipewire as pw;
    use pw::properties::properties;
    use ringbuf::{traits::Producer, HeapProd};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};

    use tracing::info;

    use crate::audio::resample::{ResamplerImpl, ResamplerMode};
    use crate::ui::{
        model::{AudioBackend, AudioDeviceId, AudioDeviceInfo, AudioDirection},
        UiEvent,
    };

    struct PipeWireCaptureState {
        format: pw::spa::param::audio::AudioInfoRaw,
        target_rate: u32,
        target_channels: u16,
        resampler: Option<ResamplerImpl>,
        mono_in: Vec<f32>,
        mono_out: Vec<f32>,
        log_once: bool,
        resampler_mode: ResamplerMode,
        tx_event: Option<Sender<UiEvent>>,
        graph_logged: bool,
        last_process_at: Option<Instant>,
        timing_unstable_hits: u32,
    }

    enum LinuxCaptureBackend {
        PipeWire,
        Pulse(CpalCapture),
    }

    static PIPEWIRE_CAPTURE_FAILURES: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    pub struct LinuxCapture {
        thread: Option<std::thread::JoinHandle<()>>,
        stop: Arc<AtomicBool>,
        backend: LinuxCaptureBackend,
    }

    impl LinuxCapture {
        pub fn start(
            sample_rate: u32,
            channels: u16,
            prod: HeapProd<i16>,
            preferred_device: Option<&str>,
            preferred_mode: Option<&str>,
            tx_event: Option<Sender<UiEvent>>,
        ) -> Result<Self> {
            let prefer_pipewire = preferred_mode
                .map(|mode| mode == super::CAPTURE_MODE_PIPEWIRE)
                .unwrap_or(false);
            let prefer_pulse = preferred_mode
                .map(|mode| mode == super::CAPTURE_MODE_PULSEAUDIO)
                .unwrap_or(false);

            if !prefer_pulse && pipewire_is_available() {
                let preferred_device_owned = preferred_device.map(str::to_string);
                let tx_event_thread = tx_event.clone();
                let reported = Arc::new(AtomicBool::new(false));
                let reported_thread = reported.clone();
                let stop = Arc::new(AtomicBool::new(false));
                let stop_thread = stop.clone();
                let thread = std::thread::Builder::new()
                    .name("tsod-pipewire-capture".to_string())
                    .spawn(move || {
                        if let Err(e) = run_pipewire_capture(
                            sample_rate,
                            channels,
                            prod,
                            preferred_device_owned,
                            stop_thread,
                            tx_event_thread.clone(),
                        ) {
                            eprintln!("pipewire capture thread failed: {e:#}");
                            let failures = PIPEWIRE_CAPTURE_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
                            if reported_thread
                                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                                .is_ok()
                            {
                                if let Some(tx) = &tx_event_thread {
                                    let detail = format!("{e:#}");
                                    let detail_lower = detail.to_ascii_lowercase();
                                    let _ = tx.send(UiEvent::AppendLog(format!(
                                        "[audio] pipewire capture thread failed: {detail}"
                                    )));
                                    if detail_lower.contains("permission")
                                        || detail_lower.contains("portal")
                                        || detail_lower.contains("access denied")
                                    {
                                        let _ = tx.send(UiEvent::AppendLog(
                                            "[audio] PipeWire diagnostics: check xdg-desktop-portal and WirePlumber session permissions for microphone access.".to_string(),
                                        ));
                                    }
                                    if detail_lower.contains("target")
                                        || detail_lower.contains("node")
                                        || detail_lower.contains("object")
                                    {
                                        let _ = tx.send(UiEvent::AppendLog(
                                            "[audio] PipeWire diagnostics: selected node may be unavailable; verify default source in WirePlumber/pavucontrol to avoid wrong-default-mic selection.".to_string(),
                                        ));
                                    }
                                    if failures >= 3 {
                                        let _ = tx.send(UiEvent::SetPipeWirePulseFallbackSuggested(true));
                                        let _ = tx.send(UiEvent::AppendLog(
                                            "[audio] PipeWire capture setup has failed repeatedly; use the 'Use PulseAudio fallback now' button in Settings → Capture for a one-click fallback.".to_string(),
                                        ));
                                    }
                                }
                            }
                        }
                    })
                    .context("spawn PipeWire capture thread")?;

                if let Some(tx) = &tx_event {
                    let _ = tx.send(UiEvent::SetPipeWirePulseFallbackSuggested(false));
                    let _ = tx.send(UiEvent::AppendLog(
                        "[audio] capture backend active: PipeWire native".to_string(),
                    ));
                }
                return Ok(Self {
                    thread: Some(thread),
                    stop,
                    backend: LinuxCaptureBackend::PipeWire,
                });
            }

            if prefer_pipewire {
                return Err(anyhow!("PipeWire capture mode requested but unavailable"));
            }

            eprintln!("PipeWire unavailable, falling back to PulseAudio capture via CPAL");
            if let Some(tx) = &tx_event {
                let _ = tx.send(UiEvent::AppendLog(
                    "[audio] using PulseAudio fallback for capture".to_string(),
                ));
            }
            let pulse = CpalCapture::start(
                sample_rate,
                channels,
                prod,
                preferred_device,
                None,
                tx_event,
            )?;
            Ok(Self {
                thread: None,
                stop: Arc::new(AtomicBool::new(false)),
                backend: LinuxCaptureBackend::Pulse(pulse),
            })
        }

        pub fn enumerate_input_devices() -> Vec<AudioDeviceInfo> {
            if pipewire_is_available() {
                return enumerate_pipewire_inputs();
            }
            CpalCapture::enumerate_input_devices()
        }

        pub fn enumerate_capture_modes() -> Vec<String> {
            let mut modes = vec![super::CAPTURE_MODE_AUTO.to_string()];
            if pipewire_is_available() {
                modes.push(super::CAPTURE_MODE_PIPEWIRE.to_string());
            }
            if !CpalCapture::enumerate_input_devices().is_empty() {
                modes.push(super::CAPTURE_MODE_PULSEAUDIO.to_string());
            }
            modes
        }

        pub fn is_healthy(&self) -> bool {
            match &self.backend {
                LinuxCaptureBackend::PipeWire => true,
                LinuxCaptureBackend::Pulse(cpal) => cpal.is_healthy(),
            }
        }
    }

    impl Drop for LinuxCapture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
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
        preferred_device: Option<String>,
        stop: Arc<AtomicBool>,
        tx_event: Option<Sender<UiEvent>>,
    ) -> Result<()> {
        pw::init();

        let mainloop = pw::main_loop::MainLoopBox::new(None).context("create PipeWire mainloop")?;
        let context = pw::context::ContextBox::new(mainloop.loop_(), None)
            .context("create PipeWire context")?;
        let core = context.connect(None).context("connect PipeWire core")?;

        let props = if let Some(target) = preferred_device.as_deref() {
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::TARGET_OBJECT => target,
            }
        } else {
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Communication",
            }
        };

        let stream = pw::stream::StreamBox::new(&core, "tsod-capture", props)
            .context("create PipeWire capture stream")?;

        let requested_format = pw::spa::param::audio::AudioFormat::S16LE;
        let listener = stream
            .add_local_listener_with_user_data(PipeWireCaptureState {
                format: pw::spa::param::audio::AudioInfoRaw::new(),
                target_rate: sample_rate,
                target_channels: channels,
                resampler: None,
                mono_in: Vec::new(),
                mono_out: Vec::new(),
                log_once: false,
                resampler_mode: ResamplerMode::from_env(),
                tx_event: tx_event.clone(),
                graph_logged: false,
                last_process_at: None,
                timing_unstable_hits: 0,
            })
            .param_changed(move |_, state, id, param| {
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else {
                    return;
                };
                if state.format.parse(param).is_err() {
                    tracing::warn!("[audio] pipewire capture: failed to parse negotiated format");
                    return;
                }

                let negotiated_rate = state.format.rate();
                let negotiated_channels = state.format.channels();
                let negotiated_format = state.format.format();

                if !state.log_once {
                    info!(
                        "[audio] pipewire capture resampler={} in_rate={} out_rate={} channels={} format={:?}",
                        state.resampler_mode.as_str(),
                        negotiated_rate,
                        state.target_rate,
                        1,
                        negotiated_format
                    );
                    state.log_once = true;
                }

                if !state.graph_logged {
                    if negotiated_rate != 48_000 {
                        tracing::warn!(
                            "[audio] pipewire graph guidance: negotiated capture rate is {} Hz; 48 kHz is recommended for lower resampling pressure",
                            negotiated_rate
                        );
                        if let Some(tx) = &state.tx_event {
                            let _ = tx.send(UiEvent::AppendLog(format!(
                                "[audio] diagnostics: PipeWire negotiated {} Hz. Recommended profile: 48 kHz.",
                                negotiated_rate
                            )));
                        }
                    }
                    state.graph_logged = true;
                }

                if negotiated_format != requested_format {
                    tracing::error!(
                        "[audio] pipewire capture negotiated unsupported format {:?} (expected {:?}); capture will be muted",
                        negotiated_format,
                        requested_format
                    );
                }

                state.resampler = if negotiated_rate != state.target_rate {
                    Some(ResamplerImpl::new(negotiated_rate, state.target_rate, 1, state.resampler_mode))
                } else {
                    None
                };
            })
            .process({
                move |stream: &pw::stream::Stream, state: &mut PipeWireCaptureState| {
                    let Some(mut buf) = stream.dequeue_buffer() else {
                        return;
                    };

                    let datas = buf.datas_mut();
                    if datas.is_empty() {
                        return;
                    }

                    let negotiated_format = state.format.format();
                    if negotiated_format != requested_format {
                        return;
                    }

                    let negotiated_channels = state.format.channels().max(1) as usize;

                    let chunk = datas[0].chunk();
                    let chunk_offset = chunk.offset() as usize;
                    let chunk_size = chunk.size() as usize;
                    let chunk_stride = chunk.stride() as usize;

                    let Some(raw) = datas[0].data() else {
                        return;
                    };

                    if let Some(prev) = state.last_process_at.replace(Instant::now()) {
                        let delta = prev.elapsed();
                        let expected = Duration::from_secs_f64(256.0 / state.format.rate().max(1) as f64);
                        if delta > expected.mul_f32(1.75) {
                            state.timing_unstable_hits += 1;
                            if state.timing_unstable_hits == 6 {
                                tracing::warn!(
                                    "[audio] pipewire graph guidance: capture callback jitter looks high (latest {:?}); consider 48 kHz + smaller quantum where hardware allows",
                                    delta
                                );
                                if let Some(tx) = &state.tx_event {
                                    let _ = tx.send(UiEvent::AppendLog(format!(
                                        "[audio] diagnostics: PipeWire graph timing looks unstable (callback gap {:?}). Consider 48 kHz and smaller quantum where hardware allows.",
                                        delta
                                    )));
                                }
                            }
                        }
                    }

                    let offset = chunk_offset.min(raw.len());
                    let available_bytes = raw.len().saturating_sub(offset);
                    let size = chunk_size.min(available_bytes);
                    if size < 2 {
                        return;
                    }

                    let sample_bytes = &raw[offset..offset + size - (size % 2)];
                    let samples = unsafe {
                        std::slice::from_raw_parts(
                            sample_bytes.as_ptr() as *const i16,
                            sample_bytes.len() / 2,
                        )
                    };

                    state.mono_in.clear();
                    let frame_stride_samples = {
                        let stride = chunk_stride / 2;
                        if stride >= negotiated_channels { stride } else { negotiated_channels }
                    };

                    if negotiated_channels == 1 && frame_stride_samples == 1 {
                        state.mono_in.extend(
                            samples
                                .iter()
                                .map(|&s| (s as f32 / i16::MAX as f32).clamp(-1.0, 1.0)),
                        );
                    } else {
                        state.mono_in.reserve(samples.len() / frame_stride_samples);
                        for frame in samples.chunks_exact(frame_stride_samples) {
                            let sum: f32 = frame[..negotiated_channels]
                                .iter()
                                .map(|&s| (s as f32 / i16::MAX as f32).clamp(-1.0, 1.0))
                                .sum();
                            state.mono_in.push(sum / negotiated_channels as f32);
                        }
                    }

                    state.mono_out.clear();
                    if let Some(resampler) = state.resampler.as_mut() {
                        resampler.process_mono(&state.mono_in, &mut state.mono_out);
                    } else {
                        state.mono_out.extend_from_slice(&state.mono_in);
                    }

                    for &s in &state.mono_out {
                        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
                        for _ in 0..state.target_channels {
                            let _ = prod.try_push(v);
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
        while !stop.load(Ordering::Relaxed) {
            let _ = mainloop
                .loop_()
                .iterate(std::time::Duration::from_millis(100));
        }
        Ok(())
    }

    fn enumerate_pipewire_inputs() -> Vec<AudioDeviceInfo> {
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
            if media_class != "Audio/Source" {
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
                    direction: AudioDirection::Input,
                    id: node_name.to_string(),
                },
                label: label.clone(),
                display_label: label,
                is_default: false,
            });
        }
        devices
    }

    #[cfg(target_os = "windows")]
    use crate::audio::windows::mmdevice;

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
            _preferred_mode: Option<&str>,
            tx_event: Option<Sender<UiEvent>>,
        ) -> Result<Self> {
            let host = cpal::default_host();
            let dev = if let Some(name) = preferred_device {
                find_input_device_by_id(&host, name)
                    .with_context(|| format!("input device '{name}' not found"))?
            } else {
                host.default_input_device()
                    .ok_or(anyhow!("no input device"))?
            };
            let selected_id = dev
                .id()
                .ok()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<unknown-id>".to_string());
            let selected_name = device_label(&dev).unwrap_or_else(|| "Unknown device".to_string());
            info!(endpoint_id = %selected_id, friendly_name = %selected_name, "starting input stream");
            let stream_cfg = native_input_config(&dev)?;
            let tuned_stream_cfg = tune_pulse_input_config(&stream_cfg);
            let unhealthy = Arc::new(AtomicBool::new(false));
            let unhealthy_cb = unhealthy.clone();
            let reported = Arc::new(AtomicBool::new(false));
            let reported_cb = reported.clone();
            let stream = match stream_cfg.sample_format() {
                cpal::SampleFormat::I8 => build_input_stream::<i8>(
                    &dev,
                    &tuned_stream_cfg,
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::I16 => build_input_stream::<i16>(
                    &dev,
                    &tuned_stream_cfg,
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::I32 => build_input_stream::<i32>(
                    &dev,
                    &tuned_stream_cfg,
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::I64 => build_input_stream::<i64>(
                    &dev,
                    &tuned_stream_cfg,
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::U8 => build_input_stream::<u8>(
                    &dev,
                    &tuned_stream_cfg,
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::U16 => build_input_stream::<u16>(
                    &dev,
                    &tuned_stream_cfg,
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::U32 => build_input_stream::<u32>(
                    &dev,
                    &tuned_stream_cfg,
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::U64 => build_input_stream::<u64>(
                    &dev,
                    &tuned_stream_cfg,
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::F32 => build_input_stream::<f32>(
                    &dev,
                    &tuned_stream_cfg,
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::F64 => build_input_stream::<f64>(
                    &dev,
                    &tuned_stream_cfg,
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                other => return Err(anyhow!("unsupported input sample format: {other:?}")),
            };
            stream.play()?;
            Ok(Self {
                _stream: stream,
                unhealthy,
            })
        }

        fn enumerate_input_devices() -> Vec<AudioDeviceInfo> {
            let host = cpal::default_host();
            host.input_devices()
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
            .or_else(|| device.name().ok().filter(|name| !name.trim().is_empty()))
    }

    fn device_info(device: &cpal::Device) -> Option<AudioDeviceInfo> {
        let id_str = device.id().ok()?.to_string();
        let label = device_label(device).unwrap_or_else(|| id_str.clone());
        Some(AudioDeviceInfo {
            key: AudioDeviceId {
                backend: super::cpal_backend(),
                direction: AudioDirection::Input,
                id: id_str,
            },
            label: label.clone(),
            display_label: label,
            is_default: false,
        })
    }

    fn find_input_device_by_id(host: &cpal::Host, id: &str) -> Result<cpal::Device> {
        if let Ok(device_id) = id.parse::<cpal::DeviceId>() {
            if let Some(device) = host.device_by_id(&device_id) {
                return Ok(device);
            }
        }

        for device in host
            .input_devices()
            .context("list input devices for id fallback")?
        {
            let Ok(current_id) = device.id() else {
                continue;
            };
            if current_id.to_string() == id {
                return Ok(device);
            }
        }

        Err(anyhow!("no matching input device id: {id}"))
    }

    #[cfg(target_os = "windows")]
    fn enumerate_mmdevice_input_devices() -> Vec<AudioDeviceInfo> {
        let host = cpal::default_host();
        let default_id = mmdevice::default_input_endpoint_id().ok().flatten();
        let mut devices = Vec::new();

        let endpoints = match mmdevice::enumerate_input_endpoints() {
            Ok(values) => values,
            Err(error) => {
                warn!("failed MMDevice input enumeration: {error:#}");
                return host
                    .input_devices()
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
            debug!(endpoint_id = %endpoint_id, friendly_name = %friendly_name, openable_via_cpal = openable, "enumerated input endpoint");
            if !openable {
                warn!(endpoint_id = %endpoint_id, friendly_name = %friendly_name, "skipping input endpoint because CPAL cannot open it");
                continue;
            }
            devices.push(AudioDeviceInfo {
                key: AudioDeviceId {
                    backend: super::cpal_backend(),
                    direction: AudioDirection::Input,
                    id: endpoint_id.clone(),
                },
                label: friendly_name.clone(),
                display_label: friendly_name,
                is_default: default_id.as_deref() == Some(endpoint_id.as_str()),
            });
        }

        devices
    }

    fn native_input_config(dev: &cpal::Device) -> Result<cpal::SupportedStreamConfig> {
        dev.default_input_config()
            .context("no supported input configuration")
    }

    fn tune_pulse_input_config(cfg: &cpal::SupportedStreamConfig) -> cpal::StreamConfig {
        let mut tuned = cfg.config();
        let min_frames = (tuned.sample_rate.0 / 50).max(960);
        tuned.buffer_size = match cfg.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                cpal::BufferSize::Fixed(min_frames.clamp(*min, *max))
            }
            cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Fixed(min_frames),
        };
        tracing::info!(
            "[audio] pulse fallback capture tuning: buffer={} frames (~{} ms) for scheduler jitter headroom",
            min_frames,
            (min_frames as f32 * 1000.0) / tuned.sample_rate.0 as f32
        );
        tuned
    }

    fn build_input_stream<T>(
        dev: &cpal::Device,
        stream_cfg: &cpal::StreamConfig,
        target_rate: u32,
        target_channels: u16,
        mut prod: HeapProd<i16>,
        unhealthy: Arc<AtomicBool>,
        tx_event: Option<Sender<UiEvent>>,
        reported: Arc<AtomicBool>,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample,
        f32: cpal::FromSample<T>,
    {
        let source_rate = stream_cfg.sample_rate;
        let source_channels = stream_cfg.channels.max(1) as usize;
        let target_channels = target_channels.max(1) as usize;
        let resampler_mode = ResamplerMode::from_env();
        tracing::info!(
            "[audio] cpal capture resampler={} in_rate={} out_rate={} channels=1",
            resampler_mode.as_str(),
            source_rate,
            target_rate
        );
        let mut resampler = ResamplerImpl::new(source_rate, target_rate, 1, resampler_mode);
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
                resampler.process_mono(&mono, &mut resampled);

                for &s in &resampled {
                    let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
                    for _ in 0..target_channels {
                        let _ = prod.try_push(v);
                    }
                }
            },
            move |err| {
                unhealthy.store(true, Ordering::Relaxed);
                eprintln!("capture err: {err}");
                if reported
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    if let Some(tx) = &tx_event {
                        let _ = tx.send(UiEvent::AppendLog(format!(
                            "[audio] capture stream error: {err}"
                        )));
                    }
                }
            },
            None,
        )
        .context("build input stream")
    }
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
mod non_linux {
    use anyhow::{anyhow, Context, Result};
    use cpal::{traits::DeviceTrait, traits::HostTrait, traits::StreamTrait, SizedSample};
    use crossbeam_channel::Sender;
    use ringbuf::{traits::Producer, HeapProd};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tracing::{debug, info, warn};

    use crate::audio::resample::{ResamplerImpl, ResamplerMode};
    #[cfg(target_os = "windows")]
    use crate::audio::windows::mmdevice;
    use crate::ui::{
        model::{AudioDeviceId, AudioDeviceInfo, AudioDirection},
        UiEvent,
    };

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
            _preferred_mode: Option<&str>,
            tx_event: Option<Sender<UiEvent>>,
        ) -> Result<Self> {
            let host = cpal::default_host();
            let dev = if let Some(name) = preferred_device {
                find_input_device_by_id(&host, name)
                    .with_context(|| format!("input device '{name}' not found"))?
            } else {
                host.default_input_device()
                    .ok_or(anyhow!("no input device"))?
            };
            let selected_id = dev
                .id()
                .ok()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<unknown-id>".to_string());
            let selected_name = device_label(&dev).unwrap_or_else(|| "Unknown device".to_string());
            info!(endpoint_id = %selected_id, friendly_name = %selected_name, "starting input stream");
            let stream_cfg = native_input_config(&dev)?;
            let unhealthy = Arc::new(AtomicBool::new(false));
            let unhealthy_cb = unhealthy.clone();
            let reported = Arc::new(AtomicBool::new(false));
            let reported_cb = reported.clone();
            let stream = match stream_cfg.sample_format() {
                cpal::SampleFormat::I8 => build_input_stream::<i8>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::I16 => build_input_stream::<i16>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::I32 => build_input_stream::<i32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::I64 => build_input_stream::<i64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::U8 => build_input_stream::<u8>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::U16 => build_input_stream::<u16>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::U32 => build_input_stream::<u32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::U64 => build_input_stream::<u64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::F32 => build_input_stream::<f32>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                cpal::SampleFormat::F64 => build_input_stream::<f64>(
                    &dev,
                    &stream_cfg.config(),
                    sample_rate,
                    channels,
                    prod,
                    unhealthy_cb,
                    tx_event.clone(),
                    reported_cb.clone(),
                )?,
                other => return Err(anyhow!("unsupported input sample format: {other:?}")),
            };
            stream.play()?;
            Ok(Self {
                _stream: stream,
                unhealthy,
            })
        }

        pub fn enumerate_input_devices() -> Vec<AudioDeviceInfo> {
            #[cfg(target_os = "windows")]
            {
                return enumerate_mmdevice_input_devices();
            }

            #[cfg(not(target_os = "windows"))]
            {
                let host = cpal::default_host();
                return host
                    .input_devices()
                    .map(|devs| devs.filter_map(|d| device_info(&d)).collect())
                    .unwrap_or_default();
            }
        }

        pub fn enumerate_capture_modes() -> Vec<String> {
            vec![super::CAPTURE_MODE_AUTO.to_string()]
        }

        pub fn is_healthy(&self) -> bool {
            !self.unhealthy.load(Ordering::Relaxed)
        }
    }

    fn device_label(device: &cpal::Device) -> Option<String> {
        device
            .description()
            .ok()
            .map(|desc| desc.to_string())
            .filter(|name| !name.trim().is_empty())
            .or_else(|| device.name().ok().filter(|name| !name.trim().is_empty()))
    }

    fn device_info(device: &cpal::Device) -> Option<AudioDeviceInfo> {
        let id_str = device.id().ok()?.to_string();
        let label = device_label(device).unwrap_or_else(|| id_str.clone());
        Some(AudioDeviceInfo {
            key: AudioDeviceId {
                backend: super::cpal_backend(),
                direction: AudioDirection::Input,
                id: id_str,
            },
            label: label.clone(),
            display_label: label,
            is_default: false,
        })
    }

    fn find_input_device_by_id(host: &cpal::Host, id: &str) -> Result<cpal::Device> {
        if let Ok(device_id) = id.parse::<cpal::DeviceId>() {
            if let Some(device) = host.device_by_id(&device_id) {
                return Ok(device);
            }
        }

        for device in host
            .input_devices()
            .context("list input devices for id fallback")?
        {
            let Ok(current_id) = device.id() else {
                continue;
            };
            if current_id.to_string() == id {
                return Ok(device);
            }
        }

        Err(anyhow!("no matching input device id: {id}"))
    }

    #[cfg(target_os = "windows")]
    fn enumerate_mmdevice_input_devices() -> Vec<AudioDeviceInfo> {
        let host = cpal::default_host();
        let default_id = mmdevice::default_input_endpoint_id().ok().flatten();
        let mut devices = Vec::new();

        let endpoints = match mmdevice::enumerate_input_endpoints() {
            Ok(values) => values,
            Err(error) => {
                warn!("failed MMDevice input enumeration: {error:#}");
                return host
                    .input_devices()
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
            debug!(endpoint_id = %endpoint_id, friendly_name = %friendly_name, openable_via_cpal = openable, "enumerated input endpoint");
            if !openable {
                warn!(endpoint_id = %endpoint_id, friendly_name = %friendly_name, "skipping input endpoint because CPAL cannot open it");
                continue;
            }
            devices.push(AudioDeviceInfo {
                key: AudioDeviceId {
                    backend: super::cpal_backend(),
                    direction: AudioDirection::Input,
                    id: endpoint_id.clone(),
                },
                label: friendly_name.clone(),
                display_label: friendly_name,
                is_default: default_id.as_deref() == Some(endpoint_id.as_str()),
            });
        }

        devices
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
        tx_event: Option<Sender<UiEvent>>,
        reported: Arc<AtomicBool>,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample,
        f32: cpal::FromSample<T>,
    {
        let source_rate = stream_cfg.sample_rate;
        let source_channels = stream_cfg.channels.max(1) as usize;
        let target_channels = target_channels.max(1) as usize;
        let resampler_mode = ResamplerMode::from_env();
        tracing::info!(
            "[audio] cpal capture resampler={} in_rate={} out_rate={} channels=1",
            resampler_mode.as_str(),
            source_rate,
            target_rate
        );
        let mut resampler = ResamplerImpl::new(source_rate, target_rate, 1, resampler_mode);
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
                resampler.process_mono(&mono, &mut resampled);

                for &s in &resampled {
                    let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
                    for _ in 0..target_channels {
                        let _ = prod.try_push(v);
                    }
                }
            },
            move |err| {
                unhealthy.store(true, Ordering::Relaxed);
                eprintln!("capture err: {err}");
                if reported
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    if let Some(tx) = &tx_event {
                        let _ = tx.send(UiEvent::AppendLog(format!(
                            "[audio] capture stream error: {err}"
                        )));
                    }
                }
            },
            None,
        )
        .context("build input stream")
    }
}
