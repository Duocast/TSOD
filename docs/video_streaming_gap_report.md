# TSOD video streaming review: what is left to do

## Scope of this review
This is a source review of the attached `tsod.zip` main branch. I did **not** execute `cargo check` or run tests because the container does not have the Rust toolchain installed. Findings below are based on the code paths present in the repo.

## Bottom line
The repository already has a **usable low-level video transport foundation**:
- QUIC datagram demux exists.
- Video fragmentation / reassembly exists.
- Server-side forwarding / queueing / recovery signaling exists.
- A screen-share send path exists end-to-end.
- The UI can render a decoded stream.

What is **not** finished is the part that makes this a real, production-ready video feature:
1. the “codec” layer is still placeholder raw-frame packing/unpacking,
2. the implementation is screen-share-centric, not camera-video-centric,
3. the video-call control plane is defined in protobuf but not implemented in client or server,
4. many settings and capabilities are advertised but not actually enforced,
5. several platform backends are wrappers/stubs rather than real capture implementations.

The practical conclusion is:
- **screen-share transport plumbing is partly there**,
- **camera video streaming / video calls are not fully implemented**,
- **the current branch should not be considered production ready for video**.

---

## What already exists and looks directionally correct
These are the parts I would keep and build on.

### 1) Datagram format, fragmentation, and reassembly are in place
- `shared/voice/src/lib.rs:17-57`
- `client/src/net/video_datagram.rs:1-139`
- `client/src/net/video_transport.rs`

The code already has:
- a dedicated video datagram kind,
- a fixed video header,
- fragment indexing,
- per-frame reassembly,
- keyframe / recovery flags,
- bounded in-flight frame caches,
- packet pacing and buffer pooling on the sender.

That is real implementation work, not scaffolding.

### 2) Server-side forwarding / SFU-style stream handling exists
- `server/media/stream_forwarder.rs`
- `server/gateway/src/gateway.rs:1269-1504`

The server already does:
- stream registration,
- subscriber management,
- forwarding of QUIC datagrams,
- frame-aware queue eviction,
- recovery forwarding / keyframe request routing.

This is the strongest part of the feature.

### 3) Client receive path and UI rendering exist
- `client/src/main.rs:486-584`
- `client/src/ui/panels/streaming.rs:55-130`

The client can:
- receive subscribed stream datagrams,
- reassemble frames,
- decode them through a decoder cache,
- push RGBA frames into the UI,
- render the latest frame.

This is enough to prove the transport path conceptually works.

### 4) Screen-share control request path exists
- `client/src/main.rs:3839-4231`
- `proto/screenshare.proto`
- `server/gateway/src/gateway.rs:1269-1384`

There is a clear control-plane flow for starting a screen share and obtaining stream tags.

---

## Major blockers before video streaming can be called “fully implemented”

## P0 — real codec implementation is still missing
This is the single biggest blocker.

### Evidence
- `client/src/net/video_encode/vp9.rs:47-65`
- `client/src/net/video_encode/av1.rs:56-72`
- `client/src/net/video_decode/vp9.rs:55-79`
- `client/src/net/video_decode/av1.rs:55-79`

Both “encoders” currently serialize:
- width,
- height,
- raw BGRA pixel bytes.

Both “decoders” currently:
- read width/height back out,
- reinterpret the payload as raw pixel data,
- convert BGRA to RGBA.

That means the current branch is **not actually encoding VP9 or AV1**. It is transporting raw video frames in a custom envelope while labeling them as VP9/AV1.

### Why this blocks production
Without a real codec implementation:
- bitrate will be far too high,
- CPU/network behavior will not reflect production conditions,
- interop assumptions are invalid,
- any “performance” or “quality” numbers are misleading,
- real call-scale networking will collapse under raw frame sizes.

### What must be done
1. Replace placeholder encode/decode with real codec backends.
2. Keep one codec path as the first-class baseline and make the rest optional.
3. Emit actual access units / keyframes / recovery frames.
4. Convert captured frames into the pixel format required by the chosen codec backend.
5. Make decoder reset on stream restart / recovery / resolution changes.
6. Add robust error typing around encoder init, encode failure, decode failure, format mismatch, and device loss.

### Recommendation
For implementation sequencing, do **one codec well first**.
A good finish plan is:
- ship **VP9 first** for screen share / camera video,
- add AV1 only after the baseline path is stable,
- avoid advertising any codec that does not have a working end-to-end encoder + decoder.

---

## P0 — camera/video-call pipeline is not implemented
The repo has settings and protobuf definitions for video calls, but the actual camera video feature is not wired through.

### Evidence
- `proto/videocall.proto:11-123` defines the protocol.
- `proto/control.proto:124-128, 277-281` defines request/response slots.
- `client/src/net/dispatcher.rs:1370-1410` advertises `supports_video_call` but sets `camera_video: None`.
- `client/src/net/control.rs:149-150, 189` does the same in default caps.
- `client/src/ui/panels/settings.rs:1874-1971` exposes video-call settings.
- `client/src/ui/model.rs:720-723` stores video-call settings.
- There are **no** client or server handlers for:
  - `StartCallRequest`
  - `AnswerCallRequest`
  - `EndCallRequest`
  - `SetVideoEnabledRequest`
  - `VideoCallEvent`

### Why this blocks production
Right now the product can show a “Video Call” settings page, but there is no complete camera call feature behind it.

### What must be done
1. Implement camera capture backends, separate from screen capture.
2. Add a dedicated local camera session lifecycle.
3. Implement the video-call control plane on the server.
4. Implement client request/response handling for start / answer / end / toggle video.
5. Add call state in the UI: incoming, connecting, connected, ended, declined, busy.
6. Add participant-to-stream mapping for direct and group calls.
7. Add local preview and remote participant rendering.
8. Wire the saved settings (`video_device`, `video_resolution`, `video_fps`, `video_max_bitrate_kbps`) into the real camera path.

---

## P0 — advertised capabilities are misleading or incomplete
The capability surface says more than the implementation really delivers.

### Evidence
- `client/src/net/dispatcher.rs:1363-1370` advertises `supports_streaming` and `supports_video_call` based on feature flags.
- `client/src/net/dispatcher.rs:1410` sets `camera_video: None`.
- `client/src/net/control.rs:189` also sets `camera_video: None`.
- `client/src/screen_share/runtime_probe.rs:54-99` chooses backends mostly from platform/env policy, not from a real end-to-end capability probe.

### Why this matters
The server may make routing / negotiation decisions using capabilities that do not reflect a genuinely working runtime. That becomes a production support problem immediately.

### What must be done
1. Do not advertise `supports_video_call` until camera video is actually runnable.
2. Populate `camera_video` with real codec / resolution / fps / bitrate limits when camera video exists.
3. Split “compiled in” from “runtime validated”.
4. Probe device access, encoder init, and decoder init before advertising a capability.
5. Fail closed: if the capability cannot be validated, do not announce it.

---

## P0 — protocol documentation is inconsistent with implementation
There are protocol comments that no longer match the real wire format.

### Evidence
- `proto/videocall.proto:13` says video calls use datagram type `0x03`.
- `shared/voice/src/lib.rs:19-27` defines only video/screenshare kind `0x02`.
- `proto/screenshare.proto:148-155` documents an old header layout with `hdr_len`, `stream_id_hash`, and `payload_offset`.
- `shared/voice/src/lib.rs:38-57` and `client/src/net/video_datagram.rs:1-139` implement a different layout using `u64 stream_tag` and no `hdr_len` / `payload_offset` fields.

### Why this matters
This will create implementation bugs later, especially once another developer starts finishing camera video or writing tooling around the protocol.

### What must be done
1. Make one source of truth for the datagram spec.
2. Update protobuf comments to match the real wire format.
3. Decide whether video calls truly share the same datagram kind as screen share, or whether they need their own kind.
4. Add protocol tests that serialize and parse from the spec-level layout.

---

## P1 — screen capture backends are not truly implemented yet
A lot of the “backend diversity” is wrapper naming around the same fallback path.

### Evidence
- `client/src/screen_share/capture/dxgi.rs` wraps `ScrapCapture` and reports the whole frame as dirty.
- `client/src/screen_share/capture/pipewire.rs` also wraps `ScrapCapture`.
- `client/src/screen_share/capture/x11.rs` also wraps `ScrapCapture`.
- `client/src/screen_share/capture/scrap_fallback.rs:21-52` maps `WindowsWindow` and `X11Window` to the **primary display**, not true window capture.
- `client/src/ui/model.rs:1704-1711` offers a Wayland portal picker source, but `client/src/screen_share/capture/scrap_fallback.rs:37-47` interprets Linux portal sources like display selection, not a real portal token / PipeWire stream.

### Why this matters
The UI suggests platform-specific and per-window capture behavior that is not actually delivered. That is acceptable for a prototype, but not for production.

### What must be done
1. Implement real platform capture backends:
   - Windows: actual DXGI duplication and real window capture path.
   - Linux/Wayland: actual xdg-desktop-portal + PipeWire session handling.
   - Linux/X11: actual X11 window capture or clear removal of unsupported window modes.
2. Make source selection identifiers map to real backend handles.
3. Remove or hide UI options that are not actually implemented.
4. Add device-loss / display-change / revoked-permission handling.
5. Add frame pacing and damage-region support where available.

---

## P1 — current send path ignores configured FPS/bitrate and lacks pacing at capture stage
### Evidence
- `client/src/main.rs:4048-4052` capture loop runs as fast as frames arrive and blindly `try_send`s.
- `client/src/main.rs:4111-4118` encoder sessions are configured with hardcoded `fps: 30` and `target_bitrate_bps: 2_000_000`.
- `client/src/ui/model.rs:713-717` stores screen-share settings.
- The actual start path uses the chosen codec/profile but not the user FPS/bitrate knobs.

### Why this matters
The UI exposes settings that the pipeline does not honor. Also, an unpaced capture loop tends to create bursty CPU usage, pointless dropped frames, and unstable latency.

### What must be done
1. Respect `screen_share_fps` and `screen_share_max_bitrate_kbps` in encoder/session config.
2. Add a frame clock so capture and encode are cadence-controlled.
3. Decide explicit drop policy:
   - latest-frame wins for screen share,
   - maybe bounded jitter for camera.
4. Feed bitrate changes into `update_bitrate()` rather than only at startup.
5. Connect transport feedback / queue pressure to adaptive bitrate and framerate reduction.

---

## P1 — current “capability measurement” is not measuring a real encoder
### Evidence
- `client/src/net/dispatcher.rs:1268-1299`
- `client/src/net/dispatcher.rs:1302-1328`

`benchmark_realtime_encode_fps()` just copies BGRA rows into a vector and appends a small custom header. It is **not** benchmarking VP9 or AV1 encode performance.

### Why this matters
The code uses this synthetic benchmark to infer:
- `hw_av1`,
- 1440p60 support,
- startup encode FPS.

Those values are not meaningful for real production media behavior.

### What must be done
1. Replace the synthetic benchmark with actual encoder initialization + short encode sample.
2. Measure per codec/backend, not one generic copy loop.
3. Cache the result, but only after real backend validation.
4. Remove 1440p60 claims until a real encoder path proves them.

---

## P1 — screen-share lifecycle architecture is still unfinished / centralized in `main.rs`
### Evidence
- `client/src/screen_share/session.rs` is only scaffolding.
- `client/src/screen_share/fsm.rs` is only scaffolding.
- `client/src/screen_share/policy.rs` is only scaffolding.
- The actual lifecycle lives inline in `client/src/main.rs:3839-4250`.

### Why this matters
For a production media feature, lifecycle state in a large `main.rs` block becomes hard to reason about and hard to recover from when devices fail, streams renegotiate, or permissions change.

### What must be done
1. Move session lifecycle into a dedicated module.
2. Model states explicitly: idle, starting, active, stopping, failed, recovery.
3. Make stop / teardown idempotent.
4. Make reconnect / stream restart paths explicit.
5. Separate capture, encode, transport, and control-plane concerns.

---

## P1 — receive path is too eager and too expensive for production
### Evidence
- `client/src/main.rs:555-571` decodes immediately on the receive path and copies `frame.payload` into a new `Bytes`.
- `client/src/main.rs:571` swallows decode errors silently (`Err(_err) => continue`).
- `client/src/ui/panels/streaming.rs:65-76` and `78-124` only present the latest frame, with no playout scheduling.

### Why this matters
Current behavior is acceptable for a prototype viewer, but it is not production-grade:
- decode happens on the hot receive path,
- payload copies increase pressure,
- decode failures are invisible,
- there is no frame timing / reorder / jitter strategy beyond “latest wins”.

### What must be done
1. Move decode off the datagram receive path into a dedicated decode worker pool or per-stream task.
2. Avoid payload copy where possible.
3. Add metrics and logging for decode failures.
4. Reset / recreate decoders on repeated failures.
5. Add a minimal frame playout scheduler or render cadence model.
6. Decide late-frame policy explicitly.

---

## P1 — multi-participant rendering is not there yet
### Evidence
- `client/src/ui/panels/streaming.rs:65-76` selects a single active stream to render.
- The panel renders one texture and one “waiting for stream” area.
- There is no client-side participant video layout logic for group calls.

### Why this matters
The protobuf surface talks about direct and group video calls, but the current viewer is essentially a single-stream panel.

### What must be done
1. Add participant-to-stream ownership mapping.
2. Render a grid layout for group calls.
3. Distinguish screen share view from camera call view.
4. Support pin / dominant-speaker / fullscreen behavior.
5. Handle subscribe/unsubscribe transitions per participant.

---

## P1 — UI cleanup / texture lifecycle is incomplete
### Evidence
- `client/src/main.rs:2690-2714` removes stream receiver/decoder state on unsubscribe.
- `client/src/ui/model.rs:1897-1899` keeps cached frame data and textures.
- `client/src/ui/model.rs:2439-2447` only inserts/replaces frames; there is no removal path.

### Why this matters
Stale textures and old frame state can persist after unsubscribe, which is the kind of leak / stale-preview behavior that shows up in long-running sessions.

### What must be done
1. Add a UI event for stream removal.
2. Remove frame cache, texture handle, and last-presented sequence on unsubscribe.
3. Add cleanup for stream restart / codec switch / resolution change.

---

## P1 — system audio support is exposed but not implemented
### Evidence
- `client/src/screen_share/audio.rs` is scaffolding.
- `client/src/screen_share/runtime_probe.rs:88-99` sets `supports_system_audio = false`.
- `client/src/main.rs:4205-4207` logs that `include_audio` is requested but disabled.

### Why this matters
If screen share with system audio is in product scope, it is not done.

### What must be done
1. Implement actual system-audio capture backends.
2. Decide whether system audio is muxed into the existing audio path or a dedicated media stream.
3. Expose support only on platforms where it works.

---

## P2 — simulcast / multi-layer support is mostly protocol-level, not implementation-level
### Evidence
- `proto/screenshare.proto` models repeated `SimulcastLayer`.
- `client/src/main.rs:3869-3898` only sends one layer in practice.
- `client/src/main.rs:561-566` decodes received frames with `layer_id: 0`.
- Current encoder implementations emit `layer_id: 0` only.

### Why this matters
The protocol and server negotiation suggest future adaptive layering, but the client side is effectively single-layer.

### What must be done
1. Decide whether v1 ships single-layer or true simulcast.
2. If single-layer, simplify the API and stop implying simulcast readiness.
3. If true simulcast is required, implement parallel encoder ladders, layer-specific stream IDs/tags, and viewer selection logic.

---

## P2 — NV12 path exists in types but is unused end-to-end
### Evidence
- `client/src/net/video_frame.rs` defines `PixelFormat::Nv12` and `FramePlanes::Nv12`.
- I did not find any capture, encode, or decode path actually using NV12.

### Why this matters
Most hardware media pipelines prefer NV12 or similar planar formats. Today the implementation is centered on BGRA copies, which is expensive.

### What must be done
1. Decide the canonical internal pixel formats.
2. Prefer zero-copy / low-copy paths into hardware encoders when possible.
3. Add explicit color conversion paths where needed.

---

## P2 — video call UX is missing
Even after the transport is working, the feature is not product-finish without UX work.

### Missing product behaviors
- incoming call UI,
- ringing / timeout / declined / busy states,
- toggle camera on/off,
- local preview,
- camera permission errors,
- device disappeared / device busy handling,
- reconnect / media restart UI,
- clear separation of screen share vs camera calls.

---

## Concrete “what’s left to do” plan for the developer

## Phase 1 — make one real end-to-end media path work
Goal: one codec, one capture source, one viewer path, stable enough to test.

1. Implement a real codec path.
   - Replace placeholder VP9 or AV1 encode/decode.
   - Add backend init + session config + reset + keyframe handling.
2. Keep transport code as-is unless real codec sizes expose bugs.
3. Remove misleading capability claims until the real codec path works.
4. Ensure the receive path can decode and render real compressed access units.
5. Add detailed logging + metrics around encoder init / decode failure / keyframe requests.

**Exit criteria:**
- one sender can stream a real compressed video feed,
- one receiver can render it for 30+ minutes without runaway memory or stale textures,
- recovery/keyframe request still functions.

## Phase 2 — finish screen share properly
Goal: make screen share a shippable feature.

1. Replace fake backend wrappers with real capture implementations.
2. Make source selection map to real display/window/portal sources.
3. Respect configured bitrate/fps.
4. Add capture pacing and explicit frame-drop policy.
5. Implement system audio only if it is in v1 scope; otherwise remove the user-facing option.
6. Move the large `main.rs` session logic into a dedicated session/FSM module.

**Exit criteria:**
- display and window capture really work on the supported platforms,
- settings change actual media behavior,
- start/stop/restart/device-loss are reliable.

## Phase 3 — implement camera video calls
Goal: camera-based 1:1 and small group calls.

1. Add camera capture backends.
2. Populate `camera_video` capabilities.
3. Implement server handling for:
   - `StartCallRequest`
   - `AnswerCallRequest`
   - `EndCallRequest`
   - `SetVideoEnabledRequest`
4. Implement corresponding client control and UI handling.
5. Add participant stream ownership mapping.
6. Build local preview and remote participant rendering.
7. Add direct-call and group-call layout logic.

**Exit criteria:**
- direct call works end to end,
- participant join/leave/toggle-video states work,
- group call renders multiple participants,
- camera device changes and recovery are handled cleanly.

## Phase 4 — production hardening
Goal: move from feature-complete to operationally safe.

1. Real capability probing.
2. Backpressure-aware adaptation.
3. More metrics:
   - encode ms,
   - decode ms,
   - capture ms,
   - dropped frames by cause,
   - recovery/keyframe counts,
   - queue depth,
   - resolution/bitrate changes.
4. Long-session memory and texture cleanup.
5. Soak tests under packet loss / reordering / varying RTT.
6. Device-loss and permission-loss recovery.
7. Protocol-spec cleanup.

**Exit criteria:**
- stable under loss/recovery,
- no stale stream state after many start/stop cycles,
- capability reporting matches reality,
- documentation matches wire behavior.

---

## The 10 most important code-level fixes
If a developer wants the shortest useful checklist, this is it.

1. **Replace raw-BGRA “VP9/AV1” placeholders with real codecs.**
2. **Implement camera capture; there is no real camera pipeline today.**
3. **Implement client/server video-call control handlers; protobuf alone is not enough.**
4. **Stop advertising `supports_video_call` until `camera_video` is real.**
5. **Fix protocol comments so spec and implementation agree.**
6. **Replace fake/surrogate capture backends with real DXGI / PipeWire / X11/window capture.**
7. **Honor configured FPS/bitrate instead of hardcoded 30 fps / 2 Mbps.**
8. **Move decode off the receive hot path and stop swallowing decode failures silently.**
9. **Add stream/texture cleanup on unsubscribe.**
10. **Replace the synthetic “encode benchmark” with a real backend validation probe.**

---

## Final assessment
The repo is **not far from having a solid custom QUIC video transport layer**, but it is still **far from a finished production video feature**.

The transport and forwarding pieces are ahead of the media implementation itself.
The missing work is concentrated in:
- real codec integration,
- real capture backends,
- actual camera/video-call control plane,
- production lifecycle / cleanup / capability truthfulness.

If the goal is to finish this efficiently, the right path is:
1. make one real codec path work,
2. finish screen share properly,
3. then build camera/video call on the same transport foundation.
