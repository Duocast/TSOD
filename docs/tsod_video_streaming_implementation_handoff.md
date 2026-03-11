# TSOD Video Streaming Implementation Handoff

## Goal

Implement production-grade screen sharing / video streaming for TSOD without WebRTC, using the current custom QUIC + datagram + SFU-style forwarding architecture. The result must be low-latency, lightweight, modern, and capable of scaling up to **1440p60 on qualified hardware**.

This guide uses:

- **Version 3** as the architectural base.
- **Version 2** for module boundaries, access-unit model, and test / perf gates.
- **Version 1** for frame model, queueing, and rollout discipline.
- **Version 4** for capture abstraction and instrumentation.
- **Version 5** only for final sender-policy / UI / gateway plumbing.

---

## What the current repo actually looks like

These are the important realities in the current source tree:

1. `client/src/net/video_encode/mod.rs` still emits a custom raw payload with `RAW_VIDEO_MAGIC = b"TSRV"`.
2. `client/src/net/video_encode/vp9.rs` is not a real VP9 encoder. It wraps `Av1RealtimeEncoder`.
3. `client/src/net/video_decode/av1.rs` and `client/src/net/video_decode/vp9.rs` decode the same raw `TSRV` payload, not real codec bitstreams.
4. `client/src/net/video_frame.rs` already has `PixelFormat::{Bgra,Nv12}`, but `NV12` is unimplemented in the encoder path.
5. `client/src/main.rs` owns most of the screen-share session orchestration, capture backend construction, and placeholder capture implementations.
6. `client/src/main.rs` still uses `scrap` as the practical display capture path and logs a Wayland / portal intent without a real first-class portal + PipeWire video path.
7. `client/src/main.rs` hardcodes `platform_supports_system_audio() -> false`.
8. `client/src/net/dispatcher.rs` has a fake 1440p60 capability benchmark that measures raw BGRA copy + `TSRV` packing, not real encode performance.
9. `client/src/net/control.rs` and `client/src/net/dispatcher.rs` still advertise `max_simulcast_layers: 1` and `supports_system_audio: false`.
10. `server/gateway/src/gateway.rs` handles `StartScreenShareRequest`, `StopScreenShareRequest`, and `RequestRecovery`, but there is no complete viewer-driven `SelectScreenShareLayerRequest` path.
11. `server/media/stream_forwarder.rs` is already a solid fanout foundation and should be preserved, but it needs layer-aware filtering.
12. The repo contains both `client/src/net/video_decode.rs` and `client/src/net/video_decode/mod.rs`. Keep the directory-backed module and remove the duplicate top-level file during cleanup.

---

## Final technical decisions

These decisions are non-negotiable for the implementation.

### Codec policy

Use **VP9 as the production baseline sender codec** and **AV1 as the premium path**.

Sender policy presets:

- `auto_low_latency` (default): `VP9(hw) -> VP9(sw/libvpx)`
- `auto_premium_av1`: `AV1(hw) -> VP9(hw) -> VP9(sw/libvpx)`

Receiver policy:

- AV1 decode: `hw -> dav1d`
- VP9 decode: `hw -> libvpx`

Do **not** auto-fallback to AV1 software for interactive screen share. Keep AV1 software encode behind an explicit feature flag.

### Capture policy

- **Windows display capture:** DXGI Desktop Duplication first.
- **Windows window capture:** keep current GDI / `PrintWindow` path only as a temporary fallback.
- **Linux Wayland:** portal + PipeWire first-class path.
- **Linux X11:** dedicated X11 capture fallback.
- Keep `scrap` only as a temporary compatibility fallback during migration.

### Pixel format policy

- Make **NV12 the primary encoder input format**.
- BGRA remains allowed for compatibility only.
- Convert BGRA -> NV12 off the UI thread.
- Keep the pipeline bounded and latency-biased.

### Wire format policy

- Keep `client/src/net/video_datagram.rs` and the existing datagram header format.
- Replace only the frame payload content: use **real encoded access units**, not raw `TSRV` frames.

### Simulcast policy

For the first production milestone, use **independent encoders per layer**.

Do not implement SVC first.

Use this 3-layer ladder for the 1440p60 profile:

- Layer 0: `640x360 @ 30`
- Layer 1: `1280x720 @ 60`
- Layer 2: `2560x1440 @ 60`

### Queueing policy

- Every capture / encode / send queue must be bounded.
- Under pressure, **drop oldest**, never newest.
- Keep queue depths small: capture queue 2-3, encode queue 2, pending AU queue 2.
- Prioritize freshness over completeness.

---

## High-level implementation order

Implement in this order:

1. Cargo feature / dependency cleanup
2. Canonical frame + access-unit model
3. Real VP9 encode / decode baseline
4. Runtime probe and backend selection
5. Capture abstraction and platform capture backends
6. Session orchestration refactor out of `main.rs`
7. Real capability reporting
8. Sender policy / settings / UI plumbing
9. Simulcast + layer selection
10. System audio
11. Soak / impairment / perf gates

Do **not** start with AV1 software encode, SVC, or UI polish.

---

## File-by-file implementation plan

## 1. Cargo and feature cleanup

### Modify `client/Cargo.toml`

Replace the current screen-share feature wiring:

- Current:
  - `screen-share = ["video-av1"]`
  - `video-call = ["video-av1"]`

With this model:

```toml
[features]
default = ["dsp", "aec"]

dsp = []
aec = ["dsp", "dep:sonora-aec3"]

screen-share = ["video-vp9", "video-av1-decode"]
video-call = ["video-vp9", "video-av1-decode"]

video-vp9 = []
video-av1-decode = []
video-av1-software = []

screen-share-hw-windows = []
screen-share-hw-linux = []

capture-dxgi = []
capture-pipewire = []
capture-x11 = []

system-audio-windows = []
system-audio-pipewire = []

e2ee = []
dev-synthetic-stream = []
```

Add dependencies or internal wrappers for:

- `libvpx` for VP9 encode / decode baseline
- `dav1d` for AV1 software decode baseline
- `SVT-AV1` for optional AV1 software encode
- platform bindings for:
  - Windows Media Foundation + D3D11
  - Linux VA-API

Do not make AV1 software encode part of the default interactive path.

---

## 2. Canonical media types

### Modify `client/src/net/video_frame.rs`

This file should become the canonical frame / encoded-unit model for the whole client.

Replace the current `VideoPlane` + `Vec<VideoPlane>` model with an explicit plane enum.

Target shape:

```rust
use bytes::Bytes;
use crate::proto::voiceplatform::v1 as pb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Nv12,
}

#[derive(Debug, Clone)]
pub enum FramePlanes {
    Bgra {
        bytes: Bytes,
        stride: u32,
    },
    Nv12 {
        y: Bytes,
        uv: Bytes,
        y_stride: u32,
        uv_stride: u32,
    },
}

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub ts_ms: u32,
    pub format: PixelFormat,
    pub planes: FramePlanes,
}

#[derive(Debug, Clone)]
pub struct EncodedAccessUnit {
    pub codec: pb::VideoCodec,
    pub layer_id: u8,
    pub ts_ms: u32,
    pub is_keyframe: bool,
    pub data: Bytes,
}
```

Notes:

- Keep the type name `VideoFrame` to minimize churn.
- Replace `EncodedFrame` with `EncodedAccessUnit`.
- All encode / decode code should use `EncodedAccessUnit`.
- Do not keep `Vec<VideoPlane>` for the primary path.

### Modify `client/src/media_codec.rs`

Keep this file as the stable trait façade, but expand it so the traits are useful for real backends.

Target shape:

```rust
pub trait VideoEncoder: Send {
    fn configure_session(&mut self, config: VideoSessionConfig) -> anyhow::Result<()>;
    fn request_keyframe(&mut self) -> anyhow::Result<()>;
    fn update_bitrate(&mut self, bitrate_bps: u32) -> anyhow::Result<()>;
    fn encode(&mut self, frame: VideoFrame) -> anyhow::Result<EncodedAccessUnit>;
    fn backend_name(&self) -> &'static str;
}

pub trait VideoDecoder: Send {
    fn configure_session(&mut self, config: VideoSessionConfig) -> anyhow::Result<()>;
    fn decode(
        &mut self,
        encoded: &EncodedAccessUnit,
        metadata: DecodeMetadata,
    ) -> anyhow::Result<DecodedVideoFrame>;
    fn reset(&mut self) -> anyhow::Result<()>;
    fn backend_name(&self) -> &'static str;
}
```

Keep `VideoSessionConfig`, but extend it with:

- `fps: u32`
- `target_bitrate_bps: u32`
- `low_latency: bool`
- `allow_frame_drop: bool`

---

## 3. Runtime probe and sender policy

### Create `client/src/screen_share/mod.rs`

Export:

- `config`
- `runtime_probe`
- `capture`
- `session`
- `fsm`
- `policy`
- `audio`

### Create `client/src/screen_share/config.rs`

This file owns sender policy and environment overrides.

Define:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SenderPolicy {
    AutoLowLatency,
    AutoPremiumAv1,
}
```

Add methods:

- `as_str()`
- `from_settings_or_env(...)`
- `preferred_codec_order()`

Environment overrides:

- `TSOD_SCREEN_CAPTURE=auto|dxgi|pipewire|x11|scrap`
- `TSOD_VIDEO_CODEC_POLICY=auto|vp9|av1`
- `TSOD_VIDEO_ENCODER=auto|vp9-libvpx|vp9-mf|vp9-vaapi|av1-svt|av1-mf|av1-vaapi`
- `TSOD_VIDEO_DECODER=auto|vp9-libvpx|vp9-mf|vp9-vaapi|av1-dav1d|av1-mf|av1-vaapi`
- `TSOD_SYSTEM_AUDIO=auto|off|wasapi|pipewire`
- `TSOD_DISABLE_HW=0|1`

### Optional compatibility shim: create `client/src/net/video_policy.rs`

If you want to keep imports shallow for networking / UI code, add a thin re-export:

```rust
pub use crate::screen_share::config::SenderPolicy;
```

Then add `pub mod video_policy;` to `client/src/net/mod.rs`.

### Create `client/src/screen_share/runtime_probe.rs`

Define:

```rust
pub struct MediaRuntimeCaps {
    pub capture_backends: Vec<CaptureBackendKind>,
    pub encode_backends: std::collections::HashMap<pb::VideoCodec, Vec<EncodeBackendKind>>,
    pub decode_backends: std::collections::HashMap<pb::VideoCodec, Vec<DecodeBackendKind>>,
    pub audio_backends: Vec<SystemAudioBackendKind>,
    pub supports_system_audio: bool,
    pub max_simulcast_layers: u8,
    pub preferred_codec: pb::VideoCodec,
    pub supports_1440p60: bool,
}
```

Implement one entry point:

```rust
pub fn probe_media_caps(source: &crate::ShareSource) -> MediaRuntimeCaps
```

Fallback order must be:

### Windows

- capture: `dxgi -> scrap`
- encode VP9: `mf_hw_vp9 -> libvpx`
- encode AV1: `mf_hw_av1 -> svt_av1` (only if `video-av1-software` enabled)
- decode VP9: `mf_hw_vp9 -> libvpx`
- decode AV1: `mf_hw_av1 -> dav1d`
- system audio: `wasapi_loopback -> off`

### Linux / Wayland

- capture: `pipewire_portal -> scrap` (temporary fallback only)
- encode VP9: `vaapi_vp9 -> libvpx`
- encode AV1: `vaapi_av1 -> svt_av1` (software only if feature enabled)
- decode VP9: `vaapi_vp9 -> libvpx`
- decode AV1: `vaapi_av1 -> dav1d`
- system audio: `pipewire_monitor -> off`

### Linux / X11

- capture: `x11 -> scrap`
- encode / decode: same as Linux / Wayland
- system audio: `pipewire_monitor -> off`

Do not advertise:

- `supports_system_audio = true` unless init really succeeds
- `max_simulcast_layers > 1` until multi-encoder path is live
- `supports_1440p60 = true` until real encode timing passes the perf gate

---

## 4. Capture abstraction

### Modify `client/src/media_capture.rs`

Keep this as the shared trait façade.

Replace the current one-method trait with:

```rust
pub trait CaptureBackend: Send {
    fn next_frame(&mut self) -> anyhow::Result<VideoFrame>;
    fn backend_name(&self) -> &'static str;
    fn native_format(&self) -> PixelFormat;
}
```

### Create `client/src/screen_share/capture/mod.rs`

This file owns capture backend construction.

Provide:

```rust
pub fn build_capture_backend(
    source: &crate::ShareSource,
    caps: &MediaRuntimeCaps,
) -> anyhow::Result<Box<dyn CaptureBackend>>
```

### Create `client/src/screen_share/capture/dxgi.rs`

Implement real desktop display capture for Windows.

Requirements:

- Use DXGI Desktop Duplication for monitor capture.
- Keep the data path GPU-friendly as long as possible.
- Expose damage / dirty-rect information internally if available, even if v1 ignores it.
- Return `VideoFrame` in either NV12 or BGRA depending on what the backend can practically supply.

Do not route Windows display capture through `scrap` once this backend is stable.

### Create `client/src/screen_share/capture/pipewire.rs`

Implement first-class Wayland portal + PipeWire capture.

Requirements:

- Real XDG ScreenCast / portal session.
- `DMA-BUF -> SHM -> fail closed` preference order.
- Capture timestamps from the PipeWire stream and map them to `ts_ms`.
- Surface actionable errors when the portal or PipeWire setup fails.

### Create `client/src/screen_share/capture/x11.rs`

Implement dedicated X11 capture.

Requirements:

- Dedicated X11 display/window capture path.
- No portal assumptions.
- BGRA fallback is acceptable initially.

### Create `client/src/screen_share/capture/scrap_fallback.rs`

Move the existing `ScrapCapture` implementation out of `main.rs` into this file.

Use this only as a migration fallback.

### Modify `client/src/main.rs`

Delete or move out of `main.rs`:

- `ScrapCapture`
- `WindowsWindowCapture`
- `build_screen_capture()`
- platform-specific capture construction logic

Keep only the `ShareSource` enum in `main.rs` for now, unless you want to move it into `screen_share/config.rs` in a later cleanup PR.

---

## 5. Codec backends

## Rewrite `client/src/net/video_encode/mod.rs`

This file becomes the encoder registry and construction layer.

Remove:

- `RAW_VIDEO_MAGIC`
- `encode_raw_payload()`
- any `TSRV` payload generation

Replace `build_screen_encoder(codec: &str, _profile: &str)` with something like:

```rust
pub fn build_screen_encoder(
    codec: pb::VideoCodec,
    policy: SenderPolicy,
    caps: &MediaRuntimeCaps,
) -> anyhow::Result<Box<dyn VideoEncoder>>
```

This function must:

- honor sender policy
- honor environment overrides
- select hardware first if allowed and available
- explicitly log the chosen backend
- refuse AV1 software encode in interactive mode unless `video-av1-software` is enabled

## Rewrite `client/src/net/video_encode/vp9.rs`

This file must become a real VP9 implementation.

Required behavior:

- no AV1 wrapper reuse
- persistent encoder instance per stream / layer
- real keyframe requests
- real bitrate updates
- real codec output as `EncodedAccessUnit`
- `backend_name()` returns a stable value like `vp9-libvpx` or `vp9-mf`

Recommended internal split:

- software path using `libvpx`
- hardware path delegated to platform backends

## Rewrite `client/src/net/video_encode/av1.rs`

This file must become a real AV1 implementation.

Required behavior:

- hardware AV1 path when supported
- optional software AV1 encode path behind `video-av1-software`
- no fake `Hardware / SvtAv1` enum that still emits raw BGRA bytes
- hard-fail interactive AV1 if hardware is unavailable and software is not enabled

### Add platform helper files if needed

Create these if the encoder files get too large:

- `client/src/net/video_encode/hw_windows.rs`
- `client/src/net/video_encode/hw_linux.rs`

Use them for Media Foundation / VA-API integration.

## Rewrite `client/src/net/video_decode/mod.rs`

Keep the directory-backed module and delete `client/src/net/video_decode.rs`.

Change the decoder cache to be keyed by codec and layer, not codec only.

Target:

```rust
HashMap<(pb::VideoCodec, u8), Box<dyn VideoDecoder>>
```

Or keep per-stream caches in `main.rs` / `session.rs` and make the internal map keyed by `(codec, layer_id)`.

Change `decode(...)` to accept `EncodedAccessUnit` or enough metadata to construct one.

## Rewrite `client/src/net/video_decode/vp9.rs`

Implement:

- hardware decode path if available
- `libvpx` software fallback
- real VP9 bitstream decode
- `backend_name()` reporting

## Rewrite `client/src/net/video_decode/av1.rs`

Implement:

- hardware decode path if available
- `dav1d` software fallback
- real AV1 bitstream decode
- remove `decode_realtime_payload()` and all `TSRV` parsing

---

## 6. Session orchestration and queueing

### Create `client/src/screen_share/fsm.rs`

Replace the current local share state machine (`Idle / Starting / Active`) with:

```rust
pub enum ShareState {
    Idle,
    Starting { request_id: uuid::Uuid },
    Active { stream_id: String },
    Stopping { reason: StopReason },
    Error { reason: ShareError, retriable: bool },
}
```

### Create `client/src/screen_share/session.rs`

This file becomes the owner of the local share sender pipeline.

It should own:

- capture task
- capture queue
- encode workers
- per-stream / per-layer `VideoSender`
- optional system-audio sender
- metrics update loop
- shutdown / join logic

Provide a single entry point such as:

```rust
pub async fn start_local_share(...)
```

### Queueing rules inside `session.rs`

Do not keep the current `mpsc::channel::<VideoFrame>(4)` + `try_send` behavior for capture frames.

Use a bounded overwrite queue or a very small drain-latest queue with drop-oldest semantics.

Recommended queue depths:

- capture queue: 3
- encode queue per encoder: 2
- packetization queue per stream: 2

If you reuse `client/src/net/overwrite_queue.rs`, keep these rules:

- oldest frame drops first
- queue length and overflow counts must be recorded in metrics
- the sender should always encode the freshest available frame

### Modify `client/src/main.rs`

Remove or move out of `main.rs`:

- `platform_supports_system_audio()`
- `build_screen_capture()`
- direct capture thread creation
- direct encoder construction in the start-share path
- manual capture queue drain logic
- local share state transitions

Replace with calls into:

- `screen_share::runtime_probe::probe_media_caps(...)`
- `screen_share::session::start_local_share(...)`
- `screen_share::fsm::*`

Keep in `main.rs` only:

- UI intents
- control-plane requests / responses
- event fanout
- wiring into the new subsystem

### Modify `client/src/net/overwrite_queue.rs`

Keep the drop-oldest semantics.

If needed, add a generic `pop_latest_or_wait(...)` helper for video capture / encode queues so the new session layer does not have to use raw `mpsc` + `try_recv()` loops.

---

## 7. Transport and recovery

### Modify `client/src/net/video_transport.rs`

Preserve the header format and fragmentation logic.

Add or tighten:

- explicit incomplete-frame timeout metrics
- a public way to observe incomplete-frame evictions
- a public way to expose `layer_id` and recovery flags to the caller

Do **not** change the wire header.

This file is already a strong foundation.

### Modify `client/src/main.rs` or move logic into `screen_share/policy/recovery.rs`

Current viewer-side freeze / recovery logic should be moved out of the main event loop into a dedicated policy module.

Create `client/src/screen_share/policy/recovery.rs` and move the logic that currently drives:

- `RequestKeyframeRequest`
- `RequestRecovery`
- freeze counters
- cooldown behavior

Required policy:

- keyframe request first
- `RequestRecovery` only after repeated unresolved gaps
- per-stream cooldowns
- recovery requests should also trigger a sender-side forced keyframe

---

## 8. Metrics and instrumentation

### Modify `client/src/net/video_metrics.rs`

Expand beyond the current counters.

Add at least:

- `capture_frames`
- `capture_queue_overflows`
- `encode_frames`
- `encode_errors`
- `decode_frames`
- `decode_errors`
- `tx_bitrate_bps`
- `rx_bitrate_bps`
- `encode_p50_ms`
- `encode_p95_ms`
- `decode_p50_ms`
- `decode_p95_ms`
- `freeze_count`
- `freeze_ms_p95`
- `active_layer`
- `backend_label`

### Modify `client/src/ui/model.rs`

Expand `StreamDebugView`.

Add:

- `capture_fps`
- `encoded_fps`
- `decoded_fps`
- `rendered_fps`
- `encode_p95_ms`
- `decode_p95_ms`
- `tx_bitrate_bps`
- `rx_bitrate_bps`
- `freeze_count`
- `freeze_ms_p95`
- `active_layer`
- `backend_label`
- `queue_depth_capture`
- `queue_depth_encode`
- `queue_depth_packetize`

### Modify `client/src/ui/panels/streaming.rs`

Update the diagnostics overlay so it displays:

- selected codec
- selected backend
- active layer
- encode/decode FPS
- queue depth
- tx/rx bitrate
- freeze / recovery counts
- current rendered resolution
- advertised target profile

### Modify `client/src/main.rs`

When publishing `UiEvent::StreamDebugUpdate`, stop emitting placeholder values like synthetic resolution / buffer health. Populate the snapshot from real runtime metrics.

---

## 9. Capability reporting

### Rewrite `client/src/net/dispatcher.rs`

This file currently contains the fake realtime benchmark and should stop being the source of truth for media capability guesses.

Delete or replace:

- `benchmark_realtime_encode_fps()`
- the `TSRV`-based startup encode benchmark
- fake `hw_av1` inference

Replace with runtime-probe-derived capability construction.

Add a helper:

```rust
pub fn build_screenshare_caps(caps: &MediaRuntimeCaps) -> pb::ScreenShareCaps
```

`available_screen_share_codecs()` must become policy-aware and runtime-capability-aware.

Preferred advertised ordering:

- VP9 first
- AV1 second

`can_offer_1440p60()` must be driven by:

- runtime probe result
- real measured encode throughput from the live backend
- not synthetic BGRA copy performance

### Modify `client/src/net/control.rs`

Replace hardcoded capability fields with values from the runtime probe helper.

Specifically stop hardcoding:

- `max_simulcast_layers: 1`
- `supports_system_audio: false`

Only advertise what the runtime can actually do.

---

## 10. Sender policy and settings plumbing

These changes are the only place Version 5 should drive implementation.

### Modify `client/src/ui/model.rs`

Add a new persisted setting:

```rust
pub screen_share_sender_policy: String
```

Default value:

- `"auto_low_latency"`

Keep `screen_share_codec` for compatibility during migration, but the sender policy becomes the new authoritative choice for automatic behavior.

### Modify `client/src/ui/panels/settings.rs`

Replace the current profile-to-codec assumptions with a sender-policy selector.

Expose:

- `auto_low_latency`
- `auto_premium_av1`

Suggested profile mappings:

- Presentation -> `auto_low_latency`
- Balanced -> `auto_low_latency`
- Motion -> `auto_premium_av1`

Show inline fallback hints:

- `auto_low_latency: VP9(hw) -> VP9(sw)`
- `auto_premium_av1: AV1(hw) -> VP9(hw) -> VP9(sw)`

### Modify `client/src/main.rs`

In the start-share flow:

- derive sender policy from settings
- resolve preferred codec order from sender policy
- send the first requested codec accordingly
- if the response negotiates fallback, log it clearly

Do not keep direct raw codec preference as the primary control.

---

## 11. Simulcast and layer selection

### Modify `client/src/main.rs` and/or `client/src/screen_share/policy/layer_selection.rs`

Move the current layer-selection helper out of `main.rs` into a dedicated policy file.

Create:

- `client/src/screen_share/policy/mod.rs`
- `client/src/screen_share/policy/layer_selection.rs`
- `client/src/screen_share/policy/bitrate.rs`

Required behavior:

- viewers request preferred layer based on viewport + recent loss + decode health
- downshift fast
- upshift slowly
- request keyframe on layer change

### Modify `server/gateway/src/gateway.rs`

Add a real `SelectScreenShareLayerRequest` handler.

Do not leave the proto path unused.

The gateway should:

- validate viewer access to the target stream
- persist per-viewer preferred layer for the stream
- return `SelectScreenShareLayerResponse { active_layer_id }`

### Create `server/gateway/src/screenshare.rs`

Move screen-share-specific control-plane logic out of the giant `gateway.rs` match.

Suggested responsibilities:

- start-share negotiation helpers
- stream registration helpers
- layer selection helpers
- teardown helpers

### Create `server/gateway/src/screenshare_policy.rs`

Own server-side safety rules:

- layer switch cooldowns
- invalid request rejection
- conservative fallback if a viewer requests an unavailable layer

### Create `server/media/layer_filter.rs`

This file should decide, for each viewer, whether a datagram at `layer_id` should be forwarded.

### Modify `server/media/src/lib.rs`

Export the new module:

```rust
#[path = "../layer_filter.rs"]
pub mod layer_filter;
```

### Modify `server/media/stream_forwarder.rs`

Inject layer-aware filtering during fanout.

Do not change the queueing or fragment fanout architecture more than necessary.

Required behavior:

- per-viewer preferred layer
- only forward datagrams for layers the viewer should currently receive
- keep recovery / keyframe datagrams priority-aware

---

## 12. System audio

This is not the first blocker, but it must be wired into the final plan.

### Modify `client/src/media_audio_loopback.rs`

Expand the trait to support start / stop and backend naming.

### Create `client/src/screen_share/audio/mod.rs`

Provide:

```rust
pub fn build_system_audio_backend(...) -> anyhow::Result<Option<Box<dyn AudioLoopbackBackend>>>
```

### Create `client/src/screen_share/audio/wasapi_loopback.rs`

Implement Windows system audio capture.

### Create `client/src/screen_share/audio/pipewire_monitor.rs`

Implement Linux system audio capture using PipeWire monitor streams.

### Modify `client/src/main.rs` or `session.rs`

Treat system audio as a separate Opus stream, not as part of the video payload.

If system-audio init fails:

- continue with video-only mode
- log the reason clearly
- reflect the failure in the UI

---

## 13. Cleanup work

### Delete `client/src/net/video_decode.rs`

Keep `client/src/net/video_decode/mod.rs` and the submodules.

### Remove all `TSRV` references

Delete:

- `RAW_VIDEO_MAGIC`
- raw BGRA payload packaging
- raw BGRA payload decoding
- fake startup encode benchmark logic

Search for and remove all references to:

- `TSRV`
- `RAW_VIDEO_MAGIC`

---

## 14. Tests and rollout gates

## Add unit tests in the client media modules

### In `client/src/net/video_encode/*`

Add tests for:

- keyframe request propagation
- bitrate update propagation
- AU metadata correctness
- backend selection fallback order

### In `client/src/net/video_decode/*`

Add tests for:

- AV1 decode acceptance
- VP9 decode acceptance
- decoder reset behavior
- invalid / truncated AU rejection

### In `client/src/net/video_transport.rs`

Keep the existing tests and add:

- incomplete-frame timeout eviction
- recovery-trigger hook behavior
- layer-specific frame routing tests

### Add a client integration test file

Create one integration test file, for example:

- `client/tests/screenshare_roundtrip.rs`

Cover:

- encode -> fragment -> reassemble -> decode roundtrip for VP9 baseline
- keyframe request / recovery loop
- layer switch response path

## Extend soak / impairment testing

### Modify `tools/soak/src/main.rs`

Add a screenshare mode with configurable profile:

- `1080p60`
- `1440p60`

### Use `tools/netem`

Validate these cases before calling the feature production-ready:

1. 1080p60 stable for 30 minutes with 2 viewers
2. 1440p60 stable on qualified hardware
3. 2-5% packet loss with bounded recovery
4. repeated start / stop / restart is idempotent
5. no unbounded queue growth
6. no stale active-share UI state after failure

### Performance gates

Do not advertise a profile until it passes:

- `1080p60`: sustained >= 60 encoded FPS on the chosen backend with bounded p95 encode time
- `1440p60`: sustained >= 55 encoded FPS minimum for runtime offer, >= 60 FPS target for qualified hardware label

---

## 15. Recommended PR sequence

### PR 1 — media type cleanup

Modify:

- `client/Cargo.toml`
- `client/src/net/video_frame.rs`
- `client/src/media_codec.rs`
- delete `client/src/net/video_decode.rs`

### PR 2 — real VP9 baseline

Modify:

- `client/src/net/video_encode/mod.rs`
- `client/src/net/video_encode/vp9.rs`
- `client/src/net/video_decode/mod.rs`
- `client/src/net/video_decode/vp9.rs`

### PR 3 — runtime probe + caps

Create / modify:

- `client/src/screen_share/config.rs`
- `client/src/screen_share/runtime_probe.rs`
- `client/src/net/dispatcher.rs`
- `client/src/net/control.rs`
- optional `client/src/net/video_policy.rs`

### PR 4 — capture refactor

Create / modify:

- `client/src/screen_share/capture/*`
- `client/src/media_capture.rs`
- `client/src/main.rs`

### PR 5 — session orchestration

Create / modify:

- `client/src/screen_share/session.rs`
- `client/src/screen_share/fsm.rs`
- `client/src/net/overwrite_queue.rs`
- `client/src/main.rs`

### PR 6 — metrics / instrumentation

Modify:

- `client/src/net/video_metrics.rs`
- `client/src/ui/model.rs`
- `client/src/ui/panels/streaming.rs`
- `client/src/main.rs`

### PR 7 — AV1 premium path

Modify:

- `client/src/net/video_encode/av1.rs`
- `client/src/net/video_decode/av1.rs`
- any hardware helper modules

### PR 8 — sender policy + settings plumbing

Modify:

- `client/src/ui/model.rs`
- `client/src/ui/panels/settings.rs`
- `client/src/main.rs`
- `client/src/net/dispatcher.rs`

### PR 9 — simulcast + layer control

Create / modify:

- `client/src/screen_share/policy/*`
- `server/gateway/src/screenshare.rs`
- `server/gateway/src/screenshare_policy.rs`
- `server/gateway/src/gateway.rs`
- `server/media/layer_filter.rs`
- `server/media/src/lib.rs`
- `server/media/stream_forwarder.rs`

### PR 10 — system audio

Create / modify:

- `client/src/media_audio_loopback.rs`
- `client/src/screen_share/audio/*`
- `client/src/screen_share/session.rs`

### PR 11 — soak / perf / docs

Modify:

- `tools/soak/src/main.rs`
- `docs/VIDEO_STREAMING_COMPLETION_PLAN.md`
- `docs/SCREENSHARE_PLAN.md`

---

## Do not change these things unless necessary

Keep these stable:

- `client/src/net/video_datagram.rs` header format
- `client/src/net/video_transport.rs` fragmentation / reassembly architecture
- `server/media/stream_forwarder.rs` queue-first fanout design
- current QUIC transport model
- current control-plane request / response pattern

The correct change is to replace the **media payload and backend plumbing**, not to redesign the whole transport stack.

---

## Short version for the developer

If you only remember five things, remember these:

1. Replace `TSRV` with real codec access units.
2. Ship real VP9 first; AV1 is premium mode, not the baseline.
3. Move capture / session / state logic out of `main.rs`.
4. Make NV12 and real platform capture first-class.
5. Do policy / UI plumbing only after the media core is real.

