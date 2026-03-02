# Voice audio quality recommendations (Windows + Linux)

This document lists practical, implementation-ready improvements to increase perceived voice quality for end users on both Windows and Linux clients.

## Current baseline (from code)

- Voice is encoded with Opus using a VoIP profile by default, with runtime controls for bitrate, in-band FEC, and packet-loss tuning. 
- Capture and playout both run a DSP chain (AGC + RNNoise denoise/VAD, optional AEC).
- Linux capture can run PipeWire (preferred in auto mode) with PulseAudio fallback; resampling currently uses a linear interpolation resampler.
- Windows uses WASAPI shared/event mode and also relies on client-side resampling when device rate differs.

## Highest-impact cross-platform recommendations

1. **Upgrade resampling quality (biggest quality-per-effort gain).**
   - Replace `LinearResampler` with a higher-quality low-latency resampler (e.g., SpeexDSP or rubato sinc mode) for both capture and playout paths.
   - Keep linear mode as low-CPU fallback for very old devices.
   - Why: current resampler is explicitly “not audiophile-grade,” and resampling artifacts are commonly audible on speech transients.

2. **Tighten Opus adaptation policy by network class.**
   - Define explicit operating points, e.g.:
     - Good network: 32–40 kbps mono voice, FEC off or light.
     - Moderate loss/jitter: 24–32 kbps with in-band FEC and 8–12% packet-loss expectation.
     - Poor network: 16–24 kbps, stronger FEC, preserve intelligibility over brightness.
   - Use existing telemetry (RTT/loss/jitter/jitter-buffer depth) to trigger profile transitions with hysteresis.

3. **Stabilize AGC behavior to avoid “pumping.”**
   - Add per-user AGC presets (Conservative, Balanced, Boosted) with different target dBFS and attack/release.
   - Clamp maximum gain in noisy environments so denoiser artifacts are not amplified.

4. **Expose an explicit “Voice Processing Mode” UX.**
   - Presets: `Natural`, `Noise suppression`, `Low bandwidth`, `Music`.
   - Map directly to existing encoder profile + DSP toggles for predictable outcomes.

5. **Improve packet-loss concealment policy.**
   - Continue PLC/FEC use, but add bounded burst-loss behavior:
     - after N consecutive misses, cross-fade to comfort noise instead of repeated PLC voicing;
     - recover via short fade-in to reduce robotic transitions.

## Windows-specific recommendations

1. **Offer optional WASAPI exclusive mode for “Studio/USB” devices.**
   - Keep shared mode default; provide advanced toggle for exclusive low-latency/high-fidelity capture/playout.
   - Validate and auto-revert to shared mode on failure.

2. **Device format alignment helper.**
   - In settings, suggest (or auto-negotiate) 48 kHz mono/stereo formats where supported.
   - This reduces avoidable sample-rate conversion and improves clarity.

3. **AEC path hardening on speaker endpoints.**
   - Ensure echo reference feed is consistently enabled when output is speakers (not headset).
   - Add diagnostics if AEC is enabled but no valid reference is flowing.

4. **Endpoint health auto-recovery.**
   - If capture/playout stalls are detected repeatedly, trigger seamless stream re-init with user-visible toast.

## Linux-specific recommendations

1. **Prefer native PipeWire processing where possible.**
   - Keep PipeWire-first selection in auto mode; add explicit quality badge in UI (“PipeWire native”).
   - Provide a one-click fallback to PulseAudio only when PW stream setup repeatedly fails.

2. **PipeWire graph/quantum guidance.**
   - Detect high quantum / unstable graph timing and recommend profile changes (e.g., 48 kHz, smaller quantum where hardware allows).
   - Surface this in diagnostics rather than forcing it.

3. **PulseAudio compatibility tuning.**
   - For Pulse fallback, increase internal capture ring safety margin under high scheduler jitter.
   - Keep playout latency target slightly higher than PW defaults to minimize crackle on busy desktops.

4. **Portal/session-manager diagnostics.**
   - Add actionable logs for WirePlumber/xdg-desktop-portal permission or node-selection issues to reduce “bad default mic” scenarios.

## Rollout plan

1. **Phase 1 (1–2 sprints):** resampler upgrade + preset UX + Opus policy tables.
2. **Phase 2 (1 sprint):** Windows exclusive-mode toggle + Linux diagnostics/quality badges.
3. **Phase 3 (1 sprint):** advanced PLC burst-loss smoothing + AGC preset tuning from real-call telemetry.

## Success metrics

- Improve user-reported MOS/proxy quality score in telemetry panel cohorts.
- Reduce “robotic”, “tinny”, and “cuts out” issue labels in support tickets.
- Lower jitter-buffer underruns and capture underflow/stall events per active hour.
