# Screen Share Implementation Plan

## Architecture Overview

TSOD already has the ideal foundation for cutting-edge screenshare: **QUIC datagrams over quinn** for unreliable low-latency media, a **protobuf control plane**, an **SFU voice forwarder** pattern, and a **native Rust desktop client** with `egui`. The screenshare proto (`screenshare.proto`) and control wire types (fields 150-153) are already defined. This plan builds on all of that.

### Why This Will Surpass Discord

| Dimension | Discord | TSOD Screenshare |
|---|---|---|
| **Transport** | WebRTC (DTLS/SRTP over UDP) | QUIC datagrams (0-RTT, built-in congestion control, multiplexed with voice) |
| **Latency** | ~100-200ms typical | Target **30-80ms** glass-to-glass via hardware encode + QUIC datagrams + zero-copy decode |
| **Capture** | Browser `getDisplayMedia()` limited to ~30fps | Native OS capture APIs: PipeWire DMA-BUF (Linux), DXGI Desktop Duplication (Windows), ScreenCaptureKit (macOS) — 60fps+ with GPU-resident frames |
| **Encoding** | VP8/VP9 via browser WebRTC stack | Hardware-accelerated VP9/AV1 (where available) tuned for screen content |
| **Quality** | Locked behind Nitro paywall at higher res | Full quality by default — simulcast lets viewers pick layer |
| **Client** | Electron/browser overhead | Native Rust — direct GPU texture upload, no JS event loop |
| **System Audio** | OS-dependent, inconsistent | First-class loopback capture via PipeWire/WASAPI/CoreAudio |

---

## Technology Choices

### 1. Screen Capture (Per-Platform)

**Linux — PipeWire + DMA-BUF (primary), X11 fallback**
- Use PipeWire's screencast portal (`org.freedesktop.portal.ScreenCast`) via D-Bus
- Negotiate DMA-BUF format for zero-copy GPU frame handoff (avoids CPU readback entirely)
- Frames arrive as `DmaBufFrame` → pass file descriptor directly to VAAPI encoder
- Crate: `pipewire` (already a dependency) + `ashpd` for portal negotiation
- Fallback: X11 `XShmGetImage` for non-PipeWire systems (e.g., bare X11 without a portal)

**Windows — DXGI Desktop Duplication API**
- `IDXGIOutputDuplication::AcquireNextFrame()` gives GPU-resident `ID3D11Texture2D`
- Zero-copy path: hand texture directly to Media Foundation / NVENC encoder
- 60fps capable, ~1ms capture latency
- Crate: `windows` (already a dependency — add `Win32_Graphics_Dxgi`, `Win32_Graphics_Direct3D11` features)

**macOS — ScreenCaptureKit**
- `SCStream` with `SCStreamConfiguration` for frame rate, resolution, cursor capture
- Frames arrive as `CMSampleBuffer` (IOSurface-backed) → zero-copy to VideoToolbox
- Crate: `screencapturekit-rs` or raw `objc2` FFI bindings

**System Audio Capture**
- Linux: PipeWire monitor source (captures desktop audio output)
- Windows: WASAPI loopback capture (already have WASAPI bindings via `wasapi` crate)
- macOS: ScreenCaptureKit audio stream (built-in)
- Audio is Opus-encoded separately and either:
  - Sent as a parallel voice stream with the `has_audio_payload` flag (0x04), or
  - Interleaved in screen datagrams when the `has_audio` flag is set on `StartScreenShareRequest`

### 2. Video Encoding

**Primary: Hardware-accelerated VP9 with screen-content tuning**

Why VP9 first (not AV1, for this plan):
- Universal hardware encode support (NVENC, VAAPI, VideoToolbox, QSV, AMF)
- Universal hardware decode support on all viewers
- Lowest encode latency (~2-5ms for a frame on hardware)
- Prioritizes broad decode compatibility and predictable latency for screen content
- AV1 hardware encode is still spotty (only RTX 40-series+, Intel Arc+)

Encoding configuration for ultra-low latency:
```
Profile:        High (enables screen content tools)
Tune:           Zero-latency / low-latency
Rate control:   CBR or constrained VBR
Slice mode:     1 slice per frame (simplifies fragmentation)
B-frames:       0 (eliminates reorder delay)
Lookahead:      0 (eliminates buffering delay)
GOP:            Infinite (IDR only on request/loss/layer switch)
Intra refresh:  Rolling intra (gradual IDR recovery without full keyframe spike)
```

**Encoding pipeline (Rust crate choices):**

| Platform | Hardware Encoder | Crate/Binding |
|---|---|---|
| Linux (NVIDIA) | NVENC | `nvenc` crate (FFI to `libnvidia-encode`) |
| Linux (Intel/AMD) | VAAPI | `va` crate + custom encoder wrapper |
| Windows (NVIDIA) | NVENC | `nvenc` crate (same API) |
| Windows (Intel) | QSV via MFT | `windows` crate Media Foundation bindings |
| Windows (AMD) | AMF | `amf` FFI bindings |
| macOS | VideoToolbox | `videotoolbox-rs` or `objc2` FFI |
| Fallback (any) | Software libvpx | `libvpx` FFI |

**Future: AV1 tier**
- Add as optional codec behind `av1` feature flag
- NVENC AV1 (RTX 40+), VAAPI AV1 (Intel Arc+), VideoToolbox AV1 (M3+)
- Software fallback: `rav1e` (pure Rust AV1 encoder)
- Better compression = lower bitrate for same quality, but higher encode latency

### 3. Transport — QUIC Datagrams (Already Defined)

The proto defines a 24-byte screen datagram header. This is the right approach:

```
Byte layout:
  [version(1)] [type=0x02(1)] [hdr_len(2)] [stream_id_hash(4)]
  [layer_id(1)] [flags(1)] [fragment_idx(2)] [fragment_total(2)]
  [frame_seq(4)] [ts_ms(4)] [payload_offset(2)]
```

**Fragmentation strategy:**
- QUIC datagram MTU is configured at 1200 bytes (matching `QUIC_MAX_DATAGRAM_BYTES`)
- After the 24-byte header + 32-byte forwarder stamp = **1144 bytes** of payload per datagram
- A 1080p VP9 frame at medium motion ≈ 15-60KB → fragments into 13-53 datagrams
- A 1080p keyframe ≈ 80-200KB → fragments into 70-175 datagrams
- Fragment reassembly uses `frame_seq` + `fragment_idx` + `fragment_total`
- `end_of_frame` flag (0x02) triggers decode once all fragments received

**Loss recovery:**
- No retransmission for non-keyframes (stale by the time retransmit arrives)
- On loss detected (gap in `fragment_idx`): drop entire frame, request keyframe via control plane
- FEC option: Reed-Solomon over fragment groups (e.g., 10 data + 2 parity datagrams)
  - Recovers from 1-2 lost datagrams per frame without keyframe request
  - Adds ~20% bandwidth overhead but dramatically reduces keyframe storms

### 4. Server-Side: Stream Forwarder (SFU)

Build `StreamForwarder` mirroring the existing `VoiceForwarder` pattern:

```rust
pub struct StreamForwarder {
    cfg: StreamForwarderConfig,
    sessions: Arc<dyn SessionRegistry>,
    membership: Arc<dyn MembershipProvider>,
    metrics: Arc<dyn StreamMetrics>,

    /// Active streams: stream_id → StreamState
    streams: RwLock<HashMap<StreamId, StreamState>>,

    /// Per-viewer layer selection
    viewer_layers: RwLock<HashMap<(UserId, StreamId), u32>>,

    /// Per-sender rate limit
    rate: RwLock<HashMap<UserId, RateState>>,
}
```

Key behaviors:
- **Datagram type dispatch**: Gateway reads byte[1] of each datagram. `0x01` = voice → `VoiceForwarder`. `0x02` = screen → `StreamForwarder`
- **SFU forwarding**: Server does NOT decode video. It examines the 24-byte header only, then forwards matching layer datagrams to each viewer
- **Simulcast layer filtering**: Each viewer selects a layer via `SelectScreenShareLayerRequest`. Server only forwards datagrams matching that `layer_id`
- **Keyframe relay**: `RequestKeyframeRequest` from a viewer is forwarded to the sender via control plane; sender produces IDR on next frame
- **Rate limiting**: Higher limits than voice — configurable per stream, e.g., 8 Mbps per sender for screen share vs 512 Kbps for voice
- **Bandwidth estimation**: Server tracks `StreamReceiverReport` from viewers to detect congestion and can issue `ServerHint.max_stream_bitrate_bps` to the sender

### 5. Client-Side: Decode + Render

**Decoding pipeline:**
- Hardware decode via the same platform APIs (VAAPI, DXGI, VideoToolbox)
- Decoded frames are GPU textures — upload directly to egui as `TextureHandle`
- Software fallback: `libvpx` for VP9 decode

**Frame reassembly buffer:**
```rust
pub struct FrameAssembler {
    /// frame_seq → partial frame state
    pending: HashMap<u32, PartialFrame>,
    /// Max frames to buffer before dropping oldest
    max_pending: usize,
    /// Timeout for incomplete frame assembly
    frame_timeout: Duration,
}

struct PartialFrame {
    fragments: Vec<Option<Bytes>>,
    received_count: u16,
    total: u16,
    first_received_at: Instant,
    is_keyframe: bool,
}
```

**Jitter buffer for video:**
- Simpler than audio jitter buffer — video is display-rate driven, not playout-clock driven
- Target: 1-2 frame buffer depth (16-33ms at 60fps)
- On loss: skip to next keyframe, display freeze frame in between
- Adaptive: increase buffer depth on lossy networks, decrease on clean networks

**Rendering in egui:**
- Decoded YUV/NV12 → RGB conversion (GPU shader via `wgpu` if available, CPU fallback via `yuv` crate)
- Upload as `egui::TextureHandle`, render in panel with `ui.image()`
- Viewer panel: resizable, detachable, fullscreen toggle, picture-in-picture mode
- Cursor overlay: sender transmits cursor position in datagram header extension → viewer renders cursor sprite on top

### 6. Simulcast Layers

Three tiers matching the proto's `SimulcastLayer`:

| Layer | Resolution | FPS | Bitrate | Use Case |
|---|---|---|---|---|
| 0 (thumb) | 320x180 | 5 | 150 Kbps | Thumbnail preview, many viewers |
| 1 (medium) | 960x540 | 30 | 1.5 Mbps | Default viewing |
| 2 (full) | Native (up to 1920x1080+) | 60 | 4-8 Mbps | Focused viewer, fullscreen |

- Sender encodes all accepted layers simultaneously (3 parallel encode sessions, or SVC if codec supports it)
- Server selects which layer to forward per viewer based on their `SelectScreenShareLayerRequest`
- Auto-selection: viewer client can request layer switch based on render viewport size:
  - Panel < 480px wide → layer 0
  - Panel < 960px wide → layer 1
  - Panel >= 960px or fullscreen → layer 2

---

## Implementation Phases

### Phase 1: Core Pipeline (MVP — End-to-End Single Layer)

**Goal:** A single user can share their screen, and other channel members see it.

1. **Screen capture module** (`client/src/screen/capture.rs`)
   - Linux PipeWire portal capture (most common desktop Linux)
   - Windows DXGI Desktop Duplication
   - Trait: `ScreenCapture { fn next_frame() -> CapturedFrame }`
   - `CapturedFrame`: raw BGRA/NV12 pixels + timestamp + resolution

2. **Encoder module** (`client/src/screen/encode.rs`)
   - Start with software `libvpx` encoder (works everywhere, no GPU deps)
   - Trait: `VideoEncoder { fn encode(frame) -> EncodedFrame }`
   - Configure for screen content: low latency, CBR, no B-frames
   - Single layer only (layer_id=2, full quality)

3. **Frame fragmenter** (`client/src/screen/fragment.rs`)
   - Split `EncodedFrame` into 1144-byte datagram payloads
   - Write 24-byte header per fragment
   - Return `Vec<Bytes>` to send as QUIC datagrams

4. **Stream forwarder** (`server/media/stream_forwarder.rs`)
   - Datagram type dispatch in gateway (byte[1] == 0x02 → stream forwarder)
   - Parse 24-byte header, validate stream membership
   - Forward to all viewers in channel (no layer filtering yet)
   - Rate limiting (token bucket, higher limits than voice)

5. **Frame assembler + decoder** (`client/src/screen/decode.rs`)
   - Reassemble fragments into complete frames
   - Decode with `libvpx` (software)
   - Output: raw YUV frame

6. **Viewer rendering** (`client/src/screen/render.rs` + `client/src/ui/panels/screenshare.rs`)
   - YUV→RGB conversion
   - Upload to `egui::TextureHandle`
   - Basic viewer panel in UI

7. **Control plane integration**
   - Wire up `StartScreenShareRequest/Response` in gateway + dispatcher
   - Wire up `StopScreenShareRequest/Response`
   - Wire up `ScreenShareEvent` push events (started/stopped)
   - Add `screen_share` feature flag activation in client caps

### Phase 2: Hardware Acceleration + Performance

**Goal:** Sub-50ms latency with hardware encode/decode, 60fps.

1. **Hardware encoder backends**
   - VAAPI encoder (Linux Intel/AMD)
   - NVENC encoder (Linux/Windows NVIDIA)
   - VideoToolbox encoder (macOS)
   - Media Foundation encoder (Windows Intel/AMD)
   - Runtime capability detection: probe available encoders, pick best

2. **Hardware decoder backends**
   - Mirror of encoder backends for viewer-side
   - Zero-copy texture output where possible

3. **GPU-resident capture pipeline**
   - Linux: PipeWire DMA-BUF → VAAPI encode (zero CPU copy)
   - Windows: DXGI texture → NVENC/MFT encode (zero CPU copy)
   - macOS: ScreenCaptureKit IOSurface → VideoToolbox encode (zero CPU copy)

4. **Latency optimizations**
   - Encode starts before capture of next frame completes (pipelined)
   - First datagram sent as soon as first slice is encoded (sub-frame latency)
   - Viewer: decode starts on first fragment received, doesn't wait for full frame if using slices

### Phase 3: Simulcast + Adaptive Bitrate

**Goal:** Multiple quality layers, automatic quality adaptation.

1. **Multi-layer encoding**
   - Sender runs 2-3 encoder instances (or SVC encoding if using VP9/AV1)
   - Each produces fragments with different `layer_id`

2. **Server-side layer selection**
   - `StreamForwarder` filters datagrams by viewer's selected `layer_id`
   - `SelectScreenShareLayerRequest` handling

3. **Client-side auto layer selection**
   - Monitor viewport size, network conditions
   - Auto-switch layer based on available bandwidth + viewport size

4. **Bandwidth estimation**
   - `StreamReceiverReport` (already in proto at field 61)
   - Server aggregates reports, sends `ServerHint.max_stream_bitrate_bps`
   - Sender adjusts encoder bitrate in real-time

### Phase 4: System Audio + Polish

1. **System audio capture**
   - PipeWire monitor source (Linux)
   - WASAPI loopback (Windows — already have bindings)
   - ScreenCaptureKit audio (macOS)
   - Opus encode, mux alongside screen datagrams or as parallel stream

2. **FEC (Forward Error Correction)**
   - Reed-Solomon coding over fragment groups
   - Configurable redundancy ratio based on measured loss rate

3. **Keyframe request optimization**
   - Rolling intra refresh instead of full IDR (smoother recovery, lower bitrate spike)
   - Periodic forced keyframes at configurable interval (e.g., every 10s) for late joiners

4. **UI polish**
   - Screen/window/monitor picker dialog
   - Cursor capture + overlay rendering on viewer
   - Viewer controls: pause, screenshot, fullscreen, PiP
   - Presenter controls: pause, annotation overlay, region selection
   - Thumbnail previews in channel member list

---

## File Structure

```
client/src/screen/
├── mod.rs              # Feature-gated module root
├── capture/
│   ├── mod.rs          # ScreenCapture trait + CapturedFrame type
│   ├── pipewire.rs     # Linux PipeWire portal capture
│   ├── dxgi.rs         # Windows DXGI Desktop Duplication
│   ├── sckit.rs        # macOS ScreenCaptureKit
│   └── x11.rs          # X11 fallback (XShm)
├── encode/
│   ├── mod.rs          # VideoEncoder trait + EncodedFrame type
│   ├── libvpx.rs      # Software VP9 (cross-platform fallback)
│   ├── vaapi.rs        # Linux VAAPI hardware encoder
│   ├── nvenc.rs        # NVIDIA NVENC hardware encoder
│   ├── vtbox.rs        # macOS VideoToolbox hardware encoder
│   └── mft.rs          # Windows Media Foundation hardware encoder
├── decode/
│   ├── mod.rs          # VideoDecoder trait
│   ├── libvpx.rs      # Software VP9 decode
│   ├── vaapi.rs        # VAAPI hardware decode
│   ├── nvenc.rs        # NVDEC hardware decode
│   └── vtbox.rs        # VideoToolbox hardware decode
├── fragment.rs         # Fragmenter (encode → datagrams) + assembler (datagrams → frame)
├── pipeline.rs         # Capture → Encode → Fragment → Send orchestration
├── render.rs           # YUV→RGB + egui texture upload
└── audio.rs            # System audio capture + Opus encode

server/media/
├── voice_forwarder.rs  # (existing)
└── stream_forwarder.rs # New: SFU for screen share datagrams

client/src/ui/panels/
└── screenshare.rs      # Viewer panel UI
```

---

## Latency Budget (Target: < 80ms Glass-to-Glass)

| Stage | Target | Notes |
|---|---|---|
| Screen capture | 0-16ms | DMA-BUF/DXGI: ~0ms; PipeWire SHM: ~16ms (1 frame @ 60fps) |
| Encode (HW) | 2-5ms | NVENC/VAAPI/VTBox single-frame latency |
| Fragmentation | < 1ms | Pure memory copy |
| Network (LAN) | 1-5ms | QUIC datagram, no retransmit |
| Network (Internet) | 20-60ms | Typical RTT/2 |
| SFU forwarding | < 1ms | Header parse + forward, no decode |
| Reassembly | 0-16ms | Waiting for last fragment of frame |
| Decode (HW) | 2-5ms | Hardware decode |
| Render | < 1ms | GPU texture upload |
| **Total (LAN)** | **~10-30ms** | |
| **Total (Internet)** | **~30-80ms** | |

---

## Key Crate Dependencies to Add

```toml
# client/Cargo.toml additions (behind feature flags)

# Screen capture
ashpd = "0.10"                    # Linux XDG portal (PipeWire screencast)

# Video encoding/decoding (software fallback)
libvpx = "*"                    # VP9 software codec bindings

# Hardware encode (behind per-platform features)
# vaapi, nvenc, vtbox bindings as needed

# YUV conversion
dcv-color-primitives = "0.7"      # Fast SIMD YUV↔RGB conversion (from AWS)
```

---

## Congestion Control Strategy

1. **Sender-side**: Start at configured bitrate. On `ServerHint.max_stream_bitrate_bps`, immediately reduce encoder target
2. **Viewer-side**: Track `frame_seq` gaps, report in `StreamReceiverReport` every 1-2 seconds
3. **Server-side**: Aggregate loss rates. If loss > 5%, send `ServerHint` to reduce bitrate. If loss > 15%, force layer downgrade
4. **Recovery**: Exponential increase when loss drops below 1% for 5+ seconds
5. **QUIC CC**: Quinn's built-in congestion controller (BBR or Cubic) handles datagram pacing at the transport level

---

## Summary of What Already Exists vs What Needs Building

**Already exists (ready to use):**
- `screenshare.proto` — full datagram header spec, control RPCs, events
- `control.proto` — fields 150-153 wired for screen share RPCs
- `caps.proto` — `ScreenShareCaps`, `VideoCaps` with VP9/AV1 codec enum
- `VoiceForwarder` — pattern to replicate for `StreamForwarder`
- `voice_datagram.rs` — pattern to replicate for screen datagram construction
- `JitterBuffer` — reference for video frame buffering
- QUIC datagram infrastructure (quinn, gateway datagram recv loop)
- `client/Cargo.toml` has `screen-share = []` feature flag defined
- Client caps report `supports_screen_share` (currently `false`)
- `FeatureCaps.supports_screen_share` field in proto
- PipeWire dependency already in client

**Needs to be built:**
- Screen capture backends (per-platform)
- Video encoder/decoder modules
- Fragment/reassemble logic
- `StreamForwarder` (server SFU)
- Gateway datagram type dispatch (add byte[1] check)
- Control plane handlers for screenshare RPCs
- Dispatcher + push event handling for screenshare events
- Viewer UI panel
- System audio capture
- Simulcast layer management
