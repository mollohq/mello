# Streaming In-Progress Handoff (Windows Focus)

> **Status:** IN PROGRESS  
> **Updated:** 2026-04-14  
> **Priority:** Windows host -> Windows viewer (primary), macOS viewer secondary

---

## 1) Current Snapshot

Major SFU pipeline blockers were fixed and stream now runs end-to-end with stable host pacing and responsive UI controls.

What is now true:
- Host no longer hard-stalls after first packets.
- Stream stop works and manager exits cleanly.
- SFU relay path is active (packets and bytes flow host -> SFU -> viewer).
- Host-side pacing telemetry updates continuously.

Open quality issue:
- macOS viewer presentation is around ~30-35 FPS even when sender is configured 720p60.

---

## 2) What Was Fixed In This Session

### 2.1 SFU startup/egress reliability
- Added send-failure logging in:
  - `mello-core/src/stream/sink_sfu.rs`
  - `mello-core/src/stream/manager.rs`
- Added explicit no-silent-fallback guard when backend mode is SFU:
  - `mello-core/src/client/streaming.rs`

### 2.2 DataChannel readiness correctness
- Added explicit unreliable/reliable DataChannel open tracking in libmello:
  - `libmello/src/transport/peer_connection_impl.hpp`
  - `libmello/src/transport/peer_connection.cpp`
  - `libmello/include/mello.h`
  - `libmello/src/mello.cpp`
- Wired those checks into Rust SFU transport:
  - `mello-core/src/transport/sfu_connection.rs`
- `wait_for_datachannel_open()` now waits for ICE + both channel states, not ICE only.

### 2.3 Stream task starvation fixes
- Bounded coalescing drain in stream manager to avoid run-loop starvation:
  - `mello-core/src/stream/manager.rs`
  - Added test: `coalesce_video_packet_caps_drain_to_avoid_starvation`

### 2.4 Pacer deadlock fix
- Fixed token-bucket infinite-wait on oversized packets/chunks:
  - `mello-core/src/stream/pacer.rs`
  - Added test: `oversized_payload_does_not_deadlock`

### 2.5 Stop-time crash fix
- Fixed teardown ordering so callback contexts outlive C++ host shutdown:
  - `mello-core/src/stream/host.rs`

### 2.6 Present/tick scheduling improvements
- Present is no longer gated by `fed_any` burst timing:
  - `mello-core/src/client/streaming.rs`
- Split voice and stream ticks (20ms voice, 16ms stream):
  - `mello-core/src/client/mod.rs`

---

## 3) Quick Analysis From Latest Run

### Observed (macOS viewer log)
- SFU connect/join succeeds.
- Ingress keeps increasing (no transport stall).
- Viewer decode continues (VideoToolbox decoding active).
- Present markers improve but still around ~33 FPS:
  - frame #300, #600, #900, ... roughly every ~8.9s.

### Additional signal
- Frequent VideoToolbox session recreation:
  - repeated `Format description created` + `VTDecompressionSession created`
- Frequent decode callback errors:
  - `Decode callback: status=-12909 image_buffer=0x0`
- These correlate with reduced visual smoothness on macOS viewer.

Interpretation:
- Network/SFU path is mostly healthy.
- Residual FPS cap is likely decoder/session-churn and/or VT error recovery behavior on macOS viewer path, not pure transport starvation.

---

## 4) General Priority Queue (toward Discord/Parsec quality)

This is the broader queue we have been executing across slices, with current status.

### Completed foundations
- [x] Baseline stream instrumentation and impairment matrix.
- [x] Capture reliability hardening (fullscreen/swap/keyframe behavior).
- [x] SFU transport hardening and chunking/reassembly reliability.
- [x] ABR v2 base with bandwidth-aware clamp and step-up capping.
- [x] Sink-level pacing with host pacing telemetry in UI.
- [x] Stream manager starvation/deadlock fixes and stop-path crash fix.

### In progress (high leverage)
- [~] Real-app gate closure at 720p60 and 1080p60 in gameplay scenes (not just synthetic soak).
- [~] Decoder/present stability (especially macOS VideoToolbox churn `-12909` path).
- [~] ABR controller refinement (trend/ramp guardrails, smoother oscillation control).

### Next priority slices
1. **Windows->Windows quality gate (primary user lane)**
   - lock target thresholds for present FPS, ingress kbps stability, and frame-drop ceiling.
2. **Encoder/decode deep tuning per backend**
   - NVENC/AMF/QSV/VT parameter tuning, keyframe policy, copy-pressure reduction.
3. **P2P fallback parity hardening**
   - ensure behavior and stability match SFU lane under adverse networks.
4. **Real-world 1080p60 ship gate**
   - pass/fail criteria on motion-heavy scenes over WAN.
5. **Architecture checkpoint: DataChannel vs RTP video**
   - decide using measured failure modes + quality/latency envelope, not preference.

### Exit criteria for "close to Discord-quality"
- Stable W->W 720p60 and 1080p60 in real scenes.
- No control-plane deadlocks/crashes under repeated start/stop.
- ABR avoids visible oscillation under normal WAN jitter/loss.
- P2P fallback does not materially regress UX from SFU baseline.

---

## 5) Windows-to-Windows Priority Plan (next session)

Given user distribution is mostly Windows, prioritize this path first:

1. Validate baseline on W->W:
   - host 720p60 Medium preset
   - 2-minute run
   - collect present_fps, ingress_kbps, dropped decode frames
2. If W->W is near target (>=50 FPS stable):
   - ship W->W improvements first
   - track macOS viewer as separate compatibility lane
3. If W->W is also low:
   - investigate present gating on Windows UI thread
   - inspect decode/present cadence in DXVA/NVDEC path
   - compare decoded FPS vs presented FPS and look for frame_slot backpressure

---

## 6) Immediate TODO Queue

### A) Must verify next
- [ ] Repeat stop/start stream 10 times (ensure no crash regression).
- [ ] Run dedicated W->W measurement (2 minutes static + 2 minutes motion).
- [ ] Record present FPS every 10s from debug panel.

### B) If macOS viewer still ~30-35 FPS
- [ ] Add focused VT decoder diagnostics around status `-12909`.
- [ ] Confirm if VT session reset is triggered on every keyframe or only error bursts.
- [ ] Rate-limit keyframe requests when decoder is unstable.
- [ ] Consider keeping existing VT session when SPS/PPS unchanged.

### C) Transport/control-loop follow-up
- [ ] Continue ABR v2 refinement for bandwidth trend/ramp behavior.
- [ ] Keep SFU and P2P pacing telemetry parity.
- [ ] Re-run strict SFU soak gate after next pacing/decoder adjustments.

---

## 7) Repro + Evidence Checklist

For each test run, collect:
- Host app logs:
  - `Stream pacing:`
  - `Stream manager video coalesce`
  - any `send failed` lines
- Viewer app logs:
  - `Stream ingress:`
  - `Stream frame presented #`
  - decoder errors
- SFU logs for session id:
  - `stats stream_<id>` (pkts/bytes recv+sent)
  - `pc_state`, `webrtc_disconnected`, `peer_left`

Pass indicators:
- No stalls, no teardown crash.
- SFU `pkts_sent` tracks `pkts_recv` closely with active viewer.
- Presented FPS near target for selected platform lane.

---

## 8) Notes For Next Agent Session

- Treat this as active implementation state, not a greenfield design.
- Keep changes incremental and evidence-driven.
- Do not remove existing diagnostics until W->W gate is green and stable.
- Primary user goal: excellent Windows-to-Windows streaming first, then macOS viewer polish.

