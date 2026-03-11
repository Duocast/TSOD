# Video Streaming Completion Plan (Current State + Remaining Work)

This document summarizes what the repository already implements for video streaming and what remains to make the feature production-ready.

---

## 1) What is already in place

### A. Control-plane lifecycle exists end-to-end
- Client sends `StartScreenShareRequest` with selected codec/layer and handles `StartScreenShareResponse`.
- Gateway handles `StartScreenShareRequest` / `StopScreenShareRequest`, registers stream tags, and sends `SubscribeStream`/`UnsubscribeStream` pushes.
- Client consumes `SubscribeStream`/`UnsubscribeStream` push events and keeps active stream/codec maps in sync.

### B. Datagram transport and reassembly are implemented
- Client has a `VideoSender` with MTU-aware fragmentation, pacing controls, max frame guards, and recovery marking.
- Client has a `VideoReceiver` path and freeze/recovery loop integration, including periodic `RequestKeyframeRequest` and `RequestRecovery` emission when viewers detect persistent freezes.
- Gateway forwards `RequestRecovery` intents from viewers back to the sender, with forwarding cooldown behavior via stream registry.

### C. UI streaming surface exists
- Streaming panel renders the latest received frame texture and has an overlay for throughput/drop diagnostics.
- A debug snapshot is periodically emitted from runtime counters into `StreamDebugView` and then rendered in the panel.

### D. Capability negotiation scaffolding exists
- Proto/capability schema includes screenshare/video codec capability surfaces and system-audio flags.
- Gateway codec negotiation already supports primary/fallback stream assignment for mixed viewer decode capability.

---

## 2) What is NOT complete yet (with concrete evidence)

### 1. Codec implementation is still placeholder (critical)
**Current behavior**
- `Av1RealtimeEncoder`/`Vp9RealtimeEncoder` currently package raw BGRA payloads with a custom `TSRV` header instead of true realtime AV1/VP9 bitstreams.
- `decode_realtime_payload` similarly expects this raw payload format and converts BGRA→RGBA.

**Why this blocks completion**
- Bandwidth, latency, and interoperability targets cannot be met with raw-frame payloads.

**How to complete**
1. Introduce real codec backends:
   - AV1: hardware path + software fallback (e.g., SVT-AV1/rav1e baseline).
   - VP9: libvpx or equivalent maintained backend.
2. Keep encoder/decoder instances persistent per stream (already architected via traits/cache).
3. Replace `TSRV` payload format with real encoded access units and explicit keyframe metadata.
4. Add feature-gated conformance tests for decode roundtrip and keyframe recovery.

---

### 2. NV12 encode path is unimplemented
**Current behavior**
- `PixelFormat::Nv12` returns `"NV12 screen encoding is not implemented"` in encoder path.

**How to complete**
1. Add NV12 ingestion in the encoder front-end (avoid BGRA conversion when source is already NV12).
2. Normalize frame abstraction to carry format + strides/planes for zero-copy candidates.
3. Add tests for NV12 dimensions/stride edge-cases.

---

### 3. System audio screenshare is explicitly disabled
**Current behavior**
- `platform_supports_system_audio()` returns `false`.
- Client capability advertisement uses `supports_system_audio: false`.

**How to complete**
1. Implement platform loopback capture:
   - Windows: WASAPI loopback.
   - Linux: PipeWire monitor stream (portal-compatible).
2. Choose mux strategy (separate audio stream recommended for first milestone).
3. Enable capability flag only on validated platforms and add runtime fallback messaging.

---

### 4. Simulcast/adaptive layering is only partial
**Current behavior**
- Client sends a single requested layer in `StartScreenShareRequest`.
- Capability advertisement sets `max_simulcast_layers: 1`.

**How to complete**
1. Increase advertised/negotiated simulcast layer count (e.g., 2–3 layers).
2. Encode/send per-layer streams (or SVC mapping) with explicit `layer_id` management.
3. Implement viewer-driven `SelectScreenShareLayerRequest` policy based on viewport + loss/RTT.
4. Add hysteresis and cooldown to avoid quality oscillation.

---

### 5. Diagnostics still include placeholder values
**Current behavior**
- `StreamDebugView` snapshot currently sets placeholder values for `current_resolution`, `optimal_resolution`, and `viewport` (`"0x0@0"`, `"0x0*1.00"`) before UI-level derivation.

**How to complete**
1. Source real encode/decode dimensions/fps from frame metadata.
2. Add encode queue delay, decode delay, glass-to-glass latency, and recovery count metrics.
3. Emit per-stream metrics (not just aggregate) so multi-stream debugging is possible.

---

### 6. Share lifecycle state robustness can be improved
**Current behavior**
- There is a local `ShareState` and start/stop plumbing, but failure paths still depend on task-level warnings and implicit teardown.

**How to complete**
1. Introduce explicit state machine transitions with reason codes (`Idle/Starting/Active/Stopping/Error`).
2. Wire task exits (capture/encode/send) to deterministic UI reset + user-visible error.
3. Add integration tests for start failure, network drop, and forced unsubscribe while active.

---

## 3) Recommended execution order

### Phase 1 (must-have for real shipping)
1. Replace placeholder codec payload path with real AV1/VP9 encode/decode.
2. Implement NV12 path.
3. Harden lifecycle failure handling.

### Phase 2 (quality/reliability)
4. Expand metrics and real per-stream diagnostics.
5. Improve recovery policy tuning + automated stress tests under loss/jitter.

### Phase 3 (feature completeness)
6. Add simulcast + dynamic layer selection.
7. Add system-audio share support and capability rollout.

---

## 4) Suggested implementation checklist

- [ ] Add `client/src/net/video_encode.rs` (or equivalent) to isolate codec backends from `main.rs`.
- [ ] Replace raw `TSRV` frame packing/unpacking with codec bitstream packets.
- [ ] Add golden tests for AV1/VP9 decode acceptance and keyframe seek/recovery.
- [ ] Implement NV12 encode ingestion and test odd resolutions/stride alignment.
- [ ] Implement system-audio capture backend abstraction and platform adapters.
- [ ] Add simulcast layer planner + receiver-side layer selector.
- [ ] Extend `StreamDebugView` with real latency/quality metrics.
- [ ] Add soak test scenario: 30-minute share, 2 viewers, induced 2–5% packet loss.

---

## 5) Definition of done

Video streaming is complete when all of the following are true:
1. Screen share works for 30+ minutes without stalls across 2+ viewers in realistic packet loss.
2. AV1/VP9 payloads are true codec bitstreams (no raw-frame `TSRV` fallback).
3. Recovery/keyframe flow visibly restores frozen streams within target SLO.
4. Resolution/FPS/latency metrics shown in UI reflect actual runtime measurements.
5. At least one major platform supports optional system-audio sharing in production mode.
