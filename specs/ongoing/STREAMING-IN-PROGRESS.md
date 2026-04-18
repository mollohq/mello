# Streaming In-Progress Handoff (Windows Focus)

> **Status:** IN PROGRESS  
> **Updated:** 2026-04-18  
> **Priority:** Windows host -> Windows viewer (primary), macOS viewer secondary

---

## 1) Current Snapshot

Windows->Windows SFU probe path is now stable end-to-end with significantly better smoothness than baseline, but residual quality issues remain (micro-freezes + persistent blockiness/artifacts).

What is now true (2026-04-18):
- Host no longer hard-stalls after startup.
- Viewer no longer enters "keyframe then long decay corruption" state.
- Host probe now updates backend with actual encode resolution (no stale requested dimensions).
- Viewer join triggers explicit host keyframe request.
- Probe host exits on sustained SFU channel closure (no endless encode with 0 kbps egress).
- SFU viewer probe runs at correct decoded size via `watch_stream` response.
- Typical probe metrics now sit around:
  - decode/native FPS: ~42-48 (with dips into mid/upper 30s)
  - ingress: ~46-50 pps, ~5.8-7.4 Mbps
  - present_hz above decode_hz (present path not primary limiter)

Open quality issues:
- Periodic micro-freezes remain.
- Persistent blockiness/artifacts remain visible most of the time.
- First keyframe still frequently fails once (`feed_packet failed for keyframe`), then recovery continues.
- Host still reports recurring `Stream manager video coalesce` events (often even before viewer joins).

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

### 2.7 Probe tooling for SFU-only isolation (new)
- Added dedicated SFU viewer probe:
  - `tools/sfu-stream-viewer-probe`
  - title-bar + log telemetry (`dec/native/present/msg_hz/ingress/rtt`)
- Added direct Nakama `watch_stream` helper flow in viewer probe:
  - `--watch-stream-print --nakama-http-base --nakama-auth-token --session`
  - auto-populates endpoint/token and prints response fields
- Added SFU host mode to stream-host probe:
  - manual mode: `--sfu-endpoint --sfu-token --sfu-session`
  - auto mode: `--nakama-start-stream --nakama-http-base --nakama-auth-token --crew-id`

### 2.8 Host probe robustness fixes (new)
- On SFU host probe start, backend resolution now updated to actual encode size:
  - `update_stream_resolution` RPC call after `mello_stream_get_host_resolution`
- On SFU `MemberJoined`, host probe now requests immediate keyframe.
- Host probe now auto-stops if media/control channels remain closed for multiple ticks.

### 2.9 Stream send-path tuning (new)
- SFU-only chunk payload reduced from 60k to 40k:
  - `mello-core/src/stream/sink_sfu.rs`
- Queue-pressure keyframe requests heavily rate-limited and only for severe coalesce:
  - `mello-core/src/stream/manager.rs`
- Coalescer corrected to avoid fast-forwarding to arbitrary delta frames:
  - only jumps ahead when a keyframe is available
  - added test `coalesce_video_packet_keeps_oldest_delta_without_keyframe`

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
6. **Voice CPU efficiency pass (VC baseline parity)**
   - profile and cut in-call CPU overhead (current ~10% vs Discord <2%) across capture, DSP, encode/decode, and mix hot paths.

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

---

## 9) 2026-04-18 Continuation Handoff (Critical)

### 9.1 Latest interpretation
- We have materially improved quality and removed worst regressions, but not hit "buttery" parity.
- Current symptom profile suggests residual loss/jitter/backpressure behavior under SFU relay and/or packet scheduling interactions.
- This is likely no longer a pure UI presenter bottleneck.

### 9.2 Key evidence from latest logs
- Host:
  - encode pipeline healthy at 60 fps (`libmello::video/pipeline` host counters stable)
  - pacing output typically ~6.0-7.1 Mbps against 8.16 Mbps target
  - recurring `Stream manager video coalesce` persists
- Viewer:
  - decode usually ~42-48 fps with periodic dips to high/mid 30s
  - ingress remains active and fairly stable
  - first keyframe still occasionally fails once, then decode recovers

### 9.3 Exact next actions (next session)
1. **Bring in `mello-sfu` repo (same parent directory) and instrument relay path**
   - per-session recv/send msg rates
   - per-session forwarded bytes and drop counters
   - buffered/backpressure metrics per peer
   - keyframe-sized message handling visibility
2. **Correlate timestamps across host + viewer + SFU**
   - specifically around decode FPS dips and visible micro-freezes
3. **Decide SFU-side mitigation**
   - queue policy, prioritization, or pacing handoff behavior
   - keyframe-path handling strategy under congestion

### 9.4 Commands used for current probe loop
- Host probe (auto Nakama start):
  - `cargo run -p stream-host -- --fps 60 --bitrate 8000 --nakama-start-stream --nakama-http-base <...> --nakama-auth-token <...> --crew-id <...>`
- Viewer probe (auto watch_stream fetch):
  - `cargo run -p sfu-stream-viewer-probe -- --watch-stream-print --nakama-http-base <...> --nakama-auth-token <...> --session <...> --native-metrics`

### 9.5 Files changed during this continuation slice
- `mello-core/src/stream/sink_sfu.rs`
- `mello-core/src/stream/manager.rs`
- `tools/stream-host/src/main.rs`
- `tools/stream-host/Cargo.toml`
- `tools/sfu-stream-viewer-probe/src/main.rs`
- `tools/sfu-stream-viewer-probe/Cargo.toml`
- `Cargo.toml` (workspace tools members)

### 9.6 Most recent run result (after latest coalescer fix)
- "Keyframe then long decay" failure mode appears resolved.
- Remaining symptom is now:
  - periodic micro-freezes,
  - constant low-level blockiness/artifacts.
- Latest logs still show:
  - host recurring `Stream manager video coalesce` (typically dropped_stale=1..2),
  - viewer decode oscillating around ~40-48 FPS with periodic dips.
- Practical conclusion for next session:
  - continue with SFU-side instrumentation in `mello-sfu` before further client-side tuning,
  - correlate host/viewer/SFU timelines around dips and visible freeze moments,
  - focus on relay/backpressure/jitter behavior during keyframe and near-keyframe windows.

