# Video Streaming Feature: Current State and Remaining Work

## Scope of this analysis

This document focuses on **screen/video streaming over QUIC datagrams** (capture → encode → fragment/send → SFU forward → reassemble/decode → UI render), plus adjacent control-plane pieces needed to call it “complete.”

---

## What is already in place

## 1) Wire format, transport primitives, and protocol messages

- Protobuf messages for screen share, stream subscribe/unsubscribe, recovery, and video call are defined.
- Client and server both use datagram kind/version separation for video datagrams.
- Client-side packetization/reassembly exists with bounded queues and pacing.

**Implication:** the foundation is no longer “greenfield”; the core transport skeleton exists.

## 2) Server-side SFU forwarding path exists

- `StreamForwarder` is implemented with:
  - header validation,
  - sender authorization per registered stream,
  - subscriber fan-out,
  - queue pressure handling / frame-aware dropping,
  - recovery metrics hooks.
- Gateway datagram recv loop dispatches video datagrams to stream workers.
- Start/Stop screen share control requests register/unregister stream tags and push subscribe/unsubscribe events to viewers.

**Implication:** server forwarding is materially implemented, not just planned.

## 3) Client-side ingest and rendering path exists

- Datagram demux and a dedicated video receive loop are wired.
- `VideoReceiver` reassembles fragments into frames.
- A streaming panel renders decoded frames and shows live diagnostics.

**Implication:** an end-to-end viewer path exists.

## 4) Client-side sender path exists

- UI has start/stop screen share flow and source selection.
- Backend start-share flow requests stream allocation, then starts capture/encode/send tasks.
- Sender uses `VideoSender` with pacing and frame fragmentation.

**Implication:** share initiation and publishing pipeline is operational at a baseline level.

---

## What is still incomplete (and how to complete it)

## A. Replace placeholder codec path with real-time video codecs (highest priority)

### Current gap

- Current encoder is AVIF-image based (`Av1AvifEncoder`) and used as fallback even for “VP9” feature path.
- Decoder similarly uses AVIF image decode path; code contains TODO markers to replace VP9 fallback.

### Why this matters

- AVIF image encode/decode is not a production real-time stream codec path.
- Latency, CPU usage, interoperability, and quality under motion will not meet expected streaming performance.

### How to complete

1. Introduce real-time codec abstraction module (separate from `main.rs`) with pluggable backends:
   - VP9 software fallback (libvpx),
   - AV1 optional (real-time capable backend),
   - later hardware backends.
2. Keep `ScreenEncoder`/`VideoDecoder` traits, but move implementations into dedicated files.
3. Add runtime codec capability negotiation:
   - intersect local encode/decode and server-selected codec,
   - fail fast if no common codec.
4. Add benchmarking gates:
   - max encode ms/frame,
   - decode ms/frame,
   - sustained FPS under load.

**Definition of done**
- No AVIF-frame fallback in VP9 path.
- Sender/receiver both operate with true stream codec bitstreams.
- 1080p30 stable on target baseline hardware.

---

## B. Implement system-audio capture and mux strategy

### Current gap

- `platform_supports_system_audio()` always returns false.
- UI/start flow can request `include_audio`, but runtime logs that system audio is currently disabled.

### How to complete

1. Implement platform-specific loopback capture:
   - Windows: WASAPI loopback,
   - Linux: PipeWire monitor source,
   - macOS: platform equivalent.
2. Encode captured audio with existing Opus stack.
3. Define transport strategy clearly:
   - preferred: parallel media stream with shared timing reference,
   - optional: payload flag/mux extension if needed.
4. Add A/V sync clock mapping:
   - map video `ts_ms` and audio packet timestamps to common monotonic timeline.

**Definition of done**
- `include_audio=true` produces audible desktop audio on remote viewer with bounded A/V skew.

---

## C. Complete control-plane message handling for stream quality and keyframe controls

### Current gap

Gateway currently handles start/stop/recovery, but **not** full screenshare control surface from proto:
- `SelectScreenShareLayerRequest/Response` path not implemented.
- `RequestKeyframeRequest/Response` path not implemented.

### How to complete

1. Add request handlers in gateway for both messages.
2. Add stream-id ↔ stream-tag lookup in active stream registry.
3. For layer selection:
   - persist per-viewer preferred layer,
   - update `StreamForwarder` routing state.
4. For keyframe request:
   - rate-limit requests per viewer/stream,
   - forward to sender as a push event.
5. Add positive/negative ack responses with explicit error reasons.

**Definition of done**
- Viewer can switch quality layer dynamically.
- Loss-triggered keyframe requests complete round trip and recover stream quickly.

---

## D. Add real simulcast/multi-layer production path

### Current gap

- Start-share request accepts layers, but advertised/operational capability is effectively single layer in client caps (`max_simulcast_layers: 1`), and sender pipeline appears single-encoder/single-layer.

### How to complete

1. Expand sender pipeline to emit N layers:
   - either N encoders (simple path) or scalable coding if backend supports.
2. Register a stream tag per active layer and bind to shared stream-id session.
3. Update server subscription/routing to respect viewer-selected layer with fallback rules.
4. Update UI auto-layer logic based on viewport and measured throughput.

**Definition of done**
- Distinct bitrate/resolution layers are produced and selectable per viewer.

---

## E. Upgrade capture backends to true platform-native implementations

### Current gap

- Current capture path relies primarily on `scrap` fallback for display capture.
- Wayland path logs portal intent but still uses fallback capture implementation.
- Windows window capture path exists, but there is no fully integrated DXGI Desktop Duplication pipeline.

### How to complete

1. Add backend trait + implementations:
   - Linux Wayland: PipeWire portal stream,
   - Windows display: DXGI Desktop Duplication,
   - keep fallback capture for unsupported setups.
2. Add backend selection matrix and telemetry:
   - chosen backend,
   - capture FPS,
   - capture failures/timeouts.
3. Ensure pixel format compatibility with encoder backends (avoid repeated swizzles/copies).

**Definition of done**
- On supported platforms, default backend is native low-latency capture (not generic fallback).

---

## F. Implement stream receiver telemetry loop (video equivalent to voice RR)

### Current gap

- There is a periodic voice receiver report loop.
- Equivalent periodic stream/video receiver reports are not wired despite proto support.

### How to complete

1. Add periodic client stream report task (loss, jitter estimate, goodput, frame drop metrics).
2. Handle reports server-side and feed congestion hints back to sender.
3. Apply sender-side bitrate/fps adaptations from hints.

**Definition of done**
- Video quality adapts under congestion instead of only dropping frames.

---

## G. Complete video-call feature parity (if in MVP scope)

### Current gap

- Proto defines video-call request/response/events, but gateway request handling currently focuses on screenshare paths.

### How to complete

1. Decide if video-call is in current milestone.
2. If yes: implement call session lifecycle and participant/media mapping.
3. Reuse streaming transport path with call-scoped authorization and state transitions.

**Definition of done**
- End-to-end start/answer/end and participant state events function for call mode.

---

## Recommended execution plan

## Phase 1 (stabilize MVP stream quality)

1. Real-time codec replacement (A).
2. Keyframe + layer control handlers (C).
3. Video receiver telemetry + sender adaptation basics (F).

## Phase 2 (feature completeness)

4. System audio capture (B).
5. Native capture backends (E).
6. Simulcast layers (D).

## Phase 3 (adjacent feature)

7. Video-call lifecycle (G), if required for this release.

---

## Practical acceptance checklist

- [ ] Screen share 1080p30 for 10+ minutes without encoder stalls/crashes.
- [ ] Viewer recovers from induced packet loss via keyframe request path.
- [ ] Layer switch works live and updates forwarded bitrate/resolution.
- [ ] System audio works end-to-end on at least one platform.
- [ ] Stream telemetry visible and drives adaptive bitrate decisions.
- [ ] No placeholder AVIF fallback in production codec path.
