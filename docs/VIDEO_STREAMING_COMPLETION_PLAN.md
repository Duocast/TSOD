# Video Streaming: Remaining Work for Production Readiness

This document intentionally lists **only** the work still required to ship a production-grade video streaming feature.

---

## 1) Critical blockers (must be completed before production)

### 1.1 Replace placeholder codec payload path with real codecs
**Remaining work**
- Implement true realtime AV1 and VP9 encode/decode paths (hardware when available, software fallback otherwise).
- Remove the custom raw-frame `TSRV` payload dependency from normal streaming flows.
- Persist encoder/decoder instances per stream and expose explicit keyframe metadata.
- Add conformance tests for decode roundtrip and keyframe recovery behavior.

**Production acceptance**
- All video payloads on the wire are real codec bitstreams.
- Recovery flow is validated against true keyframe boundaries.

---

### 1.2 Implement NV12 encode ingestion
**Remaining work**
- Add NV12 input support in encode frontend (avoid forced BGRA conversion).
- Carry format/plane/stride metadata through frame abstractions.
- Add tests for odd dimensions and stride alignment edge cases.

**Production acceptance**
- NV12 sources encode successfully without format-conversion regressions.

---

### 1.3 Harden share lifecycle state handling
**Remaining work**
- Introduce explicit state machine transitions: `Idle -> Starting -> Active -> Stopping -> Error`.
- Ensure capture/encode/send task exits deterministically reset UI + session state.
- Surface user-visible reason codes for start/stop/failure outcomes.
- Add integration tests for start failure, network interruption, and forced unsubscribe while active.

**Production acceptance**
- No stuck sharing states after failures or abrupt stream teardown.

---

## 2) Reliability and observability gaps

### 2.1 Replace placeholder diagnostics with runtime metrics
**Remaining work**
- Populate real resolution/FPS/viewport/quality values from frame metadata.
- Add encode queue delay, decode delay, glass-to-glass latency, recovery counts.
- Emit metrics per stream (not aggregate-only) for multi-stream sessions.

**Production acceptance**
- UI and telemetry reflect actual runtime behavior and support incident debugging.

---

### 2.2 Recovery policy tuning and long-run stress validation
**Remaining work**
- Tune keyframe/recovery request thresholds and cooldown behavior under jitter/loss.
- Add soak and chaos scenarios (e.g., 30-minute run, multi-viewer, induced packet loss).
- Define and enforce SLOs for freeze detection and recovery time.

**Production acceptance**
- Streams recover within agreed SLOs in representative adverse network conditions.

---

## 3) Feature-completeness gaps

### 3.1 Simulcast/adaptive layering
**Remaining work**
- Increase simulcast capability beyond single-layer operation.
- Implement sender-side multi-layer encoding and receiver-side layer selection policy.
- Add hysteresis/cooldown to prevent rapid layer oscillation.

**Production acceptance**
- Viewers can adapt quality to viewport/network conditions without instability.

---

### 3.2 System audio sharing
**Remaining work**
- Implement platform loopback capture backends (Windows WASAPI loopback, Linux PipeWire monitor).
- Finalize transport strategy (prefer separate audio stream for initial rollout).
- Gate capability advertisement by validated platform/runtime support.

**Production acceptance**
- At least one major platform ships production-ready optional system-audio sharing.

---

## 4) Execution order (recommended)

1. Real codec implementation (AV1/VP9 bitstreams).
2. NV12 ingestion path.
3. Lifecycle state machine hardening + failure-path tests.
4. Runtime metrics and per-stream diagnostics.
5. Recovery tuning + soak/chaos validation.
6. Simulcast/adaptive layering.
7. System-audio sharing rollout.

---

## 5) Final production gate (all required)

- [ ] Real AV1/VP9 bitstream transport in production path.
- [ ] NV12 ingest support validated with edge-case tests.
- [ ] Lifecycle state machine prevents stuck sessions across failure modes.
- [ ] Runtime UI/telemetry metrics are accurate and per-stream.
- [ ] Recovery SLOs met under sustained packet loss/jitter tests.
- [ ] Simulcast/layer adaptation works with stable switching behavior.
- [ ] System-audio sharing available on at least one production platform.
