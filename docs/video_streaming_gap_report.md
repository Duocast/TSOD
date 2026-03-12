# TSOD Production Video Streaming Audit (Current State + Completion Playbook)

## 0) Audit objective

This report audits the **current repository state** and provides a **production-completion guide** for video streaming.

Primary target:
- Reach reliable, production-grade streaming up to **2560x1440 @ 60fps (1440p60)** on qualified hardware.
- Avoid complicated licensing solutions (no patent-encumbered commercial codec stacks as a requirement for baseline operation).

Constraint used for recommendations:
- Baseline should be achievable with open tooling and practical OSS stacks (e.g., VP9/libvpx as baseline, optional AV1 paths where hardware exists).

---

## 1) Executive readiness status

### Current readiness: **not production-ready** for full video feature scope

The codebase already contains strong building blocks:
1. QUIC datagram transport + fragmentation/reassembly for video streams.
2. SFU-style forwarding and recovery routing on the server.
3. UI render path for received frames.
4. Screen-share control flow (start/stop) integrated through gateway + client.

But critical production blockers remain:
1. Codec layer is still placeholder raw-frame packaging in files labeled VP9/AV1.
2. Video-call control plane is defined in protobuf but not implemented end-to-end.
3. Runtime capability advertisement overstates real support.
4. Screen capture backends are partly wrapper/fallback behavior, not true platform-native paths.
5. Performance claims (including 1440p60) are based on synthetic copy benchmarks, not real encode/decode throughput.

---

## 2) Verified state by subsystem

## 2.1 Transport and SFU forwarding (good foundation)

### What exists
- Datagrams, fragmentation, frame reassembly, recovery flags, stream-tag-aware packet handling are implemented.
- Server media forwarding already handles fanout, queueing, and keyframe-recovery signaling.

### Why this matters
- This is the hardest architectural part to retrofit later; keeping this foundation avoids a WebRTC rewrite.

### Immediate conclusion
- Keep this transport architecture and build real media implementation on top.

---

## 2.2 Encode/decode path (critical blocker)

### What exists
- Encoder/decoder modules named for VP9/AV1 currently exchange custom raw payload framing rather than true compressed bitstreams.
- Decode path reconstructs raw BGRA-derived payloads and converts for rendering.

### Production impact
- Bandwidth use explodes compared to real inter-frame compression.
- Latency and CPU behavior measured now do not represent production reality.
- 1440p60 capability cannot be trusted from current path.

### Completion requirement
- Replace placeholder encode/decode with real codec backends and real access units (keyframes/interframes).

---

## 2.3 Screen share lifecycle (partially done, too centralized)

### What exists
- Start/stop and sender loop works through `main.rs` orchestration.
- Some session/fsm/policy modules exist but are incomplete scaffolding.

### Production impact
- Large lifecycle logic in `main.rs` is difficult to harden for device loss, restart, and permission revocation.

### Completion requirement
- Move to explicit session state machine with idempotent teardown + robust restart paths.

---

## 2.4 Camera video-calling (not implemented end-to-end)

### What exists
- Protobuf schemas for call control exist.
- Settings/UI knobs for video call exist.

### Missing
- Client and server handlers for start/answer/end/toggle flows.
- Camera media capture session and participant stream mapping.
- Real multi-participant camera rendering model.

### Completion requirement
- Implement control plane and media plane together; do not ship partial call UX.

---

## 2.5 Capability reporting (misaligned with runtime truth)

### What exists
- Capability flags exposed in client control/dispatcher paths.
- Synthetic "realtime encode" benchmark used to infer high-tier support.

### Production impact
- Server and UI can make decisions on unsupported capabilities.
- Creates field failures and hard-to-debug customer regressions.

### Completion requirement
- Capability advertisement must come from real backend probe (capture open + encoder init + small encode/decode test), not compile flags or memory-copy loops.

---

## 2.6 Capture backends (incomplete platform-native implementation)

### What exists
- Backend-specific module names for DXGI/PipeWire/X11, but practical behavior still heavily fallback-centric.
- Some window/source modes map to primary-display behavior instead of true source capture.

### Production impact
- UI promises source modes that may not reflect true backend behavior.
- Quality and CPU efficiency ceiling is reduced.

### Completion requirement
- Implement true platform paths for supported OS targets and hide unsupported source modes.

---

## 2.7 Receive/render path (prototype-grade, not hardened)

### What exists
- Reassembly + decode + render to latest-frame UI flow works.

### Missing hardening
- Decode performed eagerly on receive path.
- Some decode errors are dropped without robust observability.
- Texture/cache cleanup paths are incomplete for repeated subscribe/unsubscribe churn.

### Completion requirement
- Introduce decode worker scheduling, proper failure counters/logs, and deterministic resource cleanup.

---

## 3) 1440p60 target feasibility without complicated licensing

## 3.1 Recommended codec policy

### Baseline (must-have)
- **VP9** encode/decode as production baseline.
- Reason: mature OSS ecosystem, no complex commercial licensing dependency for baseline.

### Optional premium path
- **AV1** where hardware acceleration is available and proven stable.
- Keep AV1 software encode off by default for interactive low-latency workloads.

### Explicit non-goal for baseline
- Do not require H.264/H.265 commercial licensing pathways for minimum viable production launch.

---

## 3.2 Practical 1440p60 constraints

To achieve 1440p60 reliably:
1. Real hardware encode path available (or very strong CPU for software fallback).
2. NV12-first pipeline to reduce conversion overhead.
3. Tight queue bounds + freshness-biased drop strategy.
4. Real-time adaptive bitrate/fps reaction to queue and packet-loss pressure.
5. Accurate capability gating so weak devices auto-step down.

If these are absent, 1440p60 becomes a marketing label rather than an operational SLO.

---

## 4) Detailed completion plan (step-by-step, production order)

## Phase A — Make one real codec path production-valid (P0)

### Goal
Ship one true compressed media path end-to-end (sender -> transport -> receiver -> render) with operational stability.

### Steps
1. **Define canonical encoded unit type**
   - Replace placeholder raw-frame assumptions with "encoded access unit" contract.
   - Include codec id, keyframe flag, timestamp, layer id, bytes payload.
2. **Implement real VP9 encoder backend**
   - Session init with width/height/fps/target bitrate.
   - Keyframe request support.
   - Runtime bitrate update API.
3. **Implement real VP9 decoder backend**
   - Resolution change handling.
   - Reset/reinit path on stream restart and repeated decode faults.
4. **Wire payload into existing datagram framing**
   - Keep current transport header; only replace payload semantics.
5. **Backpressure-safe sender integration**
   - Enforce bounded queues (capture/encode/send).
   - Drop oldest on overflow; never allow unbounded growth.
6. **Receiver decode separation**
   - Move decode off ingress hot path to per-stream worker/task.
7. **Telemetry first**
   - Add metrics for encode ms, decode ms, dropped frames by reason, keyframe requests, queue depth.
8. **Soak verification**
   - 30+ minute continuous stream with repeated network impairment patterns.

### Exit criteria
- Real compressed stream is transported and displayed.
- No runaway memory growth.
- Recovery signaling still requests/receives keyframes correctly.

---

## Phase B — Honor settings + real adaptation (P0)

### Goal
Ensure user-configured fps/bitrate and dynamic adaptation actually shape behavior.

### Steps
1. **Remove hardcoded encoder defaults from runtime path**
   - Route UI settings (fps, bitrate) into session config at start.
2. **Frame pacing clock**
   - Capture cadence follows target fps, not "as-fast-as-possible" loops.
3. **Adaptive bitrate controller**
   - Use transport feedback (drops, queue pressure, RTT/loss signals) to drive `update_bitrate()`.
4. **Adaptive fps downgrade policy**
   - Step-down logic: 60 -> 45 -> 30 under sustained pressure.
5. **Hysteresis for upscaling**
   - Avoid oscillation by requiring stable-good windows before stepping up.
6. **Persist + expose effective runtime values**
   - UI/telemetry should display configured vs effective bitrate/fps.

### Exit criteria
- Knobs in settings visibly change runtime behavior.
- Stream remains stable under moderate loss without long stalls.

---

## Phase C — True platform capture backends (P1)

### Goal
Align UI source selection with real OS-native capture behavior.

### Steps
1. **Windows display capture**
   - Implement true DXGI duplication path.
   - Handle monitor hotplug/display-mode changes.
2. **Windows window capture**
   - Implement real window-target capture with occlusion/minimized-window semantics clearly defined.
3. **Wayland**
   - Implement xdg-desktop-portal session flow + PipeWire stream ingestion.
   - Persist and refresh portal permissions/token handling.
4. **X11 fallback**
   - Implement true X11 source capture or remove unsupported options.
5. **Damage region support**
   - Use dirty-region metadata where available to reduce encode cost.
6. **Capability-conditioned UI**
   - Hide source types that backend cannot actually provide.

### Exit criteria
- Selected source matches actual captured source on each platform.
- Backend failures are recoverable without app restart.

---

## Phase D — Session architecture refactor (P1)

### Goal
Move media lifecycle out of `main.rs` into maintainable production modules.

### Steps
1. **Define explicit state machine**
   - Idle -> Starting -> Active -> Recovering -> Stopping -> Failed.
2. **Split responsibilities**
   - Capture worker, encode worker, network sender, control-plane adapter.
3. **Idempotent stop path**
   - Multiple stop requests and race conditions should be safe.
4. **Restart/recovery protocol**
   - Explicit transitions for permission loss, device loss, stream restart.
5. **Structured tracing**
   - Correlation IDs per session/stream for debugability.

### Exit criteria
- Repeated start/stop/restart cycles run cleanly.
- Recovery paths no longer rely on fragile cross-task shared state.

---

## Phase E — Video-call feature completion (P0 for "full feature")

### Goal
Implement real camera calling using same transport core.

### Steps
1. **Server call control handlers**
   - Implement StartCall/AnswerCall/EndCall/SetVideoEnabled routing and validation.
2. **Client call signaling**
   - Request/response handling + event-driven UI state transitions.
3. **Camera capture pipeline**
   - Device enumeration, open, format negotiation, runtime switch.
4. **Participant stream ownership map**
   - Bind stream tags to participant identities and active media state.
5. **Rendering model**
   - 1:1 and group layouts, local preview, pin/focus logic.
6. **Call UX states**
   - Ringing, connecting, active, declined, busy, ended, reconnecting.
7. **Toggle camera/video semantics**
   - Fast mute/unmute without destroying full call session where possible.

### Exit criteria
- Direct calls and small group calls are functional end-to-end.
- Join/leave/toggle events correctly update render state.

---

## Phase F — Capability truthfulness + negotiation cleanup (P0)

### Goal
Never advertise unsupported media capabilities.

### Steps
1. **Replace synthetic benchmark with real probe**
   - Probe = backend init + short encode/decode trial at target profile.
2. **Separate compile-time vs runtime capability**
   - Only runtime-validated capability should be sent to server.
3. **Populate camera capability fields accurately**
   - Codec list, max resolution/fps/bitrate from proven probe results.
4. **Conservative failure behavior**
   - Probe failure => do not advertise tier.
5. **Periodic reprobe hooks**
   - Optional revalidation on driver change/device hotplug.

### Exit criteria
- Capability payload matches actual runtime success rates in telemetry.

---

## Phase G — Protocol/doc consistency and test gates (P1)

### Goal
Prevent drift between protobuf comments, wire layout, and implementation.

### Steps
1. **Pick canonical wire spec source**
   - Align comments and code around one datagram definition.
2. **Update stale protobuf comments**
   - Datagram kinds, header fields, and payload semantics must match reality.
3. **Add serialization contract tests**
   - Round-trip tests for header + fragment parser.
4. **Add compatibility tests**
   - Guard against accidental header-breaking changes.

### Exit criteria
- New developer can implement tooling from docs without reverse-engineering code.

---

## Phase H — Production hardening and SLO validation (P0 before GA)

### Goal
Operational confidence for real customer usage.

### Steps
1. **Long-run soak matrix**
   - 1h/4h sessions across representative platforms.
2. **Impairment matrix**
   - Loss, burst loss, reorder, jitter, RTT spikes.
3. **Memory/texture cleanup audits**
   - Subscribe/unsubscribe churn tests; no stale textures.
4. **Crash/edge resilience**
   - Device removed, permission revoked, GPU reset.
5. **SLO definitions**
   - Startup-to-first-frame, steady-state frame delivery, recovery time.
6. **Release gates**
   - Fail release if any P0 metric regresses beyond threshold.

### Exit criteria
- 1440p60 works on qualified hardware profile with controlled fallback on weaker devices.

---

## 5) Concrete missing-work checklist by priority

## P0 (must complete before claiming production-grade)
1. Real codec implementation (VP9 baseline, real decode).
2. Real runtime capability probe and truthful advertisement.
3. Settings-driven fps/bitrate + adaptive control loop.
4. Camera/video-call control plane and camera media pipeline.
5. Session lifecycle hardening (idempotent stop/recovery).
6. Observability for encode/decode/drop/recovery metrics.

## P1 (strongly recommended before broad rollout)
1. Platform-native capture backends (DXGI/portal+PipeWire/X11 clarity).
2. Protocol comment/spec consistency and contract tests.
3. Multi-participant rendering and full call UX states.
4. Receive-path workerization + decode failure policy.
5. Resource cleanup robustness for long-running sessions.

## P2 (optimization and future-proofing)
1. NV12-first zero/low-copy pipeline across capture->encode.
2. Simulcast layer expansion beyond single-layer if product requires.
3. Optional AV1 premium path maturation.

---

## 6) Suggested 1440p60 qualification profile (implementation-ready)

Use this as the **minimum engineering definition** of "supports 1440p60":

1. Probe success at startup:
   - capture backend open + encoder init + 10s encode/decode trial @ 2560x1440@60 target.
2. Sustained test success:
   - 20-minute run with <= 1% dropped frames due to local overload.
3. Network tolerance test:
   - 2% random loss + jitter profile with graceful adaptation (no persistent freeze).
4. Recovery test:
   - induced keyframe loss recovers within bounded time.
5. Thermal/CPU guard:
   - if encoder falls below threshold for N seconds, auto-step down to 1080p60 or 1440p30.

If any condition fails, device must advertise lower tier automatically.

---

## 7) Final recommendation

The fastest safe path to production is:
1. Finish **real VP9 path** and runtime truthfulness first.
2. Harden **screen share** lifecycle/capture/adaptation.
3. Then implement **camera video calls** on the same stable media core.

This preserves your current QUIC/SFU architecture, avoids heavy licensing dependencies for baseline launch, and gives a credible route to 1440p60 on qualified hardware.
