# Video Streaming Feature: Current State and Remaining Work

## What is already in place

### 1) Transport and protocol plumbing is largely implemented
- A dedicated video/screenshare datagram format exists with parsing/serialization and tests (`VideoHeader`, `make_video_datagram`, payload bounds checks).  
- Fragmentation/reassembly pipeline exists with bounded memory behavior:
  - `VideoSender` handles MTU-based fragmentation, pacing hooks, recovery flagging, and oversized-frame rejection.
  - `VideoReceiver` handles out-of-order fragments, duplicate suppression, bounded in-flight slots, and frame reassembly.
- Runtime demux separates voice and video datagrams and routes video into a dedicated receiver loop.

### 2) Client control-plane wiring exists end-to-end
- UI emits `StartScreenShare`/`StopScreenShare` intents.
- Client sends `StartScreenShareRequest` and processes `StartScreenShareResponse`.
- On success, client starts capture + encode + send tasks, with stop signaling via watch channel.
- Client processes server push `SubscribeStream`/`UnsubscribeStream` to maintain active receivers and codec mapping.

### 3) Server-side control flow exists
- Gateway handles `StartScreenShareRequest` and `StopScreenShareRequest`.
- Gateway negotiates codecs and can assign primary/fallback stream tags.
- Gateway pushes subscribe/unsubscribe events to viewers and handles recovery forwarding (`RequestRecovery`).

### 4) UI has a streaming panel and diagnostics
- Streaming panel renders latest received frame texture and displays stream diagnostics.
- Debug counters are periodically pushed into UI (`StreamDebugUpdate`) and include datagrams/s, tx drops, last frame metadata, etc.

---

## What is still incomplete (and why streaming is not “done”)

## A) Encoding/decoding is still placeholder-grade for realtime streaming

### Gap
- Current encoder path uses AVIF image encoding (`image::codecs::avif::AvifEncoder`) per frame, which is not a realtime video encoder pipeline.
- VP9 path currently reuses the same AVIF fallback (TODO in code).
- Decoder similarly uses `image::load_from_memory` AVIF fallback, and VP9 path is also TODO/fallback.
- `decode_video_frame` constructs a fresh decoder per frame, so decoder state is not reused.

### How to finish
1. Introduce persistent realtime encoder/decoder instances per active stream/codec.
2. Implement true VP9 and/or AV1 video encode/decode backends (software baseline first, hardware optional).
3. Replace AVIF fallback paths and remove TODOs.
4. Keep a decoder cache keyed by `(stream_tag, codec)` and decode in a worker thread/task pool to avoid UI-thread stalls.

---

## B) Capture pipeline is incomplete for production quality/perf

### Gap
- `PixelFormat::Nv12` encode path returns "not implemented".
- Current capture path uses BGRA CPU copies, with no zero-copy GPU path.
- Linux/Wayland is acknowledged in logs, but there is no robust portal+PipeWire native frame path yet.

### How to finish
1. Implement NV12 input support in encoder path.
2. Add platform-specialized capture backends with a normalized frame abstraction:
   - Windows: DXGI Desktop Duplication.
   - Linux: PipeWire portal path first (Wayland-safe), X11 fallback.
   - macOS (if supported target): ScreenCaptureKit.
3. Add backpressure-aware capture pacing (drop old frames intentionally when encode/send lags).

---

## C) System-audio share is explicitly disabled

### Gap
- `platform_supports_system_audio()` hardcodes `false`.
- Start-share flow logs warning that include-audio is requested but disabled.

### How to finish
1. Implement platform-specific loopback/system-audio capture.
2. Mux/send according to protocol decision (parallel stream vs frame-associated signaling).
3. Advertise capability correctly in caps (`supports_system_audio`) once verified.

---

## D) Simulcast/layer adaptation is not really implemented

### Gap
- Client currently requests only a single simulcast layer in start-share request.
- Sender uses `layer_id = 0` in `VideoSender::new`, not dynamic multi-layer encoding.
- No client logic for `SelectScreenShareLayerRequest` based on viewport/network.

### How to finish
1. Define and request 2–3 layers in `StartScreenShareRequest` (e.g., 180p/540p/1080p).
2. Run per-layer encoder/sender or SVC mapping and propagate correct `layer_id` in datagram headers.
3. Add receiver-side layer selection policy based on panel size + measured loss/jitter.
4. Send layer-select requests when viewport changes (fullscreen, thumbnail, etc.).

---

## E) Loss recovery loop is only partially wired

### Gap
- Server can forward `RequestRecovery`, and sender can mark recovery frames.
- Receiver path does not yet detect reassembly timeouts/gaps in a way that actively requests recovery from sender.
- Incomplete frame expiry/timers are minimal (bounded slots exist, but not robust age-based eviction + recovery trigger).

### How to finish
1. Add frame assembly timeout (e.g., 40–80ms) with drop reason metrics.
2. On timeout/gap bursts, emit `RequestRecovery` with stream tag (rate-limited).
3. Treat recovery/keyframes as priority in send queue where possible.

---

## F) UI/state synchronization around share lifecycle needs hardening

### Gap
- UI sets `sharing_active = true` optimistically on button click before start-share success.
- Error and teardown paths can leave UI state stale if start fails or sender exits unexpectedly.
- No explicit "remote stream roster" UX (who is sharing, selected stream focus, stream ended notice).

### How to finish
1. Move `sharing_active` transitions to confirmed state changes (response + push events).
2. Add explicit local share state machine: `Idle -> Starting -> Active -> Stopping -> Idle`.
3. Surface sender-task failure to UI and auto-reset controls.
4. Add multi-stream UI handling (if multiple users share in same channel).

---

## G) Streaming stats are partly synthetic/placeholders

### Gap
- Debug snapshot uses placeholder values for resolution/viewport (`0x0@0`, `0x0*1.00`) unless derived from rendered texture.
- No explicit encode/decode latency metrics, end-to-end glass latency, or per-stream jitter/loss panels.

### How to finish
1. Capture real encode/decode/frame queue timings and publish in `StreamDebugView`.
2. Track per-stream dimensions/fps from actual encoded metadata.
3. Add dropped-frame reason counters (queue full, decode error, timeout, not subscribed, etc.).

---

## H) Feature-gating and codec capabilities are conservative/incomplete

### Gap
- Available codecs are constrained to feature flags + decodable set; VP8 exists in proto/server negotiation paths but is not a first-class client encode/decode option in the current path.

### How to finish
1. Decide supported codec matrix for MVP and enforce consistently across client/server negotiation.
2. Either implement VP8 support end-to-end or remove it from negotiation candidates to avoid dead paths.

---

## Recommended implementation order (practical)

1. **Realtime codec replacement (BASIC software path first)**
   - Persistent encode/decode instances, remove AVIF fallback usage.
2. **Receiver reliability pass**
   - Timeout-based incomplete-frame eviction + `RequestRecovery` trigger loop.
3. **Lifecycle/state hardening**
   - Local share state machine and failure cleanup.
4. **Capture improvements**
   - NV12 + platform capture backend improvements.
5. **Simulcast and adaptation**
   - Multi-layer encode/send + layer selection control.
6. **System audio share**
   - Platform loopback support and caps updates.
7. **Observability and tuning**
   - Real latency and quality metrics, better debug panel.

---

## Definition of Done (suggested)

The feature can be considered complete when all of the following are true:
- Two or more users in one channel can start/view/stop streams reliably for 30+ minutes.
- Realtime video encode/decode is used (no AVIF-per-frame fallback), with stable fps at target profile.
- Packet loss causes graceful degradation + recovery request behavior, not permanent freeze.
- Share lifecycle state is consistent between backend and UI.
- System supports at least one production-grade capture backend per primary OS target.
- Debug metrics report real values (resolution/fps/latency/loss/drop reasons).
