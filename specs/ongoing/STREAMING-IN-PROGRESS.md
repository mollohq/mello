# Streaming — Current Implementation & Roadmap

> **Updated:** 2026-04-22
> **Target:** Windows host → Windows viewer via SFU, 1080p60 @ 8 Mbps

---

## Architecture Overview

```
 ┌──────────── Host ─────────────┐   SFU    ┌──── Viewer ────┐
 │ Capture → Preprocess → Encode │ → relay → │ Decode → Present│
 │  (DXGI)   (BGRA→NV12)  (NVENC)│          │  (DXVA)  (Slint)│
 └───────────────────────────────┘          └─────────────────┘
```

**Rust layer (`mello-core`):** stream lifecycle, SFU signaling (Nakama), chunking/reassembly, ABR, pacing, recovery/keyframe policy.

**C++ layer (`libmello`):** hardware capture (DXGI Desktop Duplication / WGC), GPU color-space conversion, hardware encode (NVENC/AMF/QSV), hardware decode (DXVA), cursor capture, audio capture. Linked via `mello-sys` FFI.

**Transport:** DataChannels over WebRTC (libdatachannel). Media is chunked (40 KB max payload) with sequence/chunk headers for reassembly on the viewer side. SFU (`mello-sfu`, Go) forwards media — no transcoding.

---

## Host Pipeline (Capture → Network)

### 1. Capture

Two backends, hot-swappable at runtime per process:

| Backend | API | When used |
|---------|-----|-----------|
| **DXGI-DDI** | `IDXGIOutputDuplication` (Desktop Duplication) | Fullscreen / exclusive-fullscreen games — captures the monitor output directly |
| **WGC** | `Windows.Graphics.Capture` | Windowed games — captures a specific window |

`ProcessCapture` wraps both. Given a PID, it finds the main game window via `EnumWindows` (largest restored-area, non-toolwindow), detects fullscreen (covers ≥90% of monitor in either current or restored placement), and picks DXGI or WGC accordingly. A background `monitor_thread` periodically re-evaluates and hot-swaps if the game transitions between windowed ↔ fullscreen.

**Deferred start:** if the target window is minimized at stream start (common when user tabs out to launch), capture waits. The monitor thread polls until the window is restored, then initializes the appropriate backend. Width/height return the restored dimensions during the wait so the encoder can pre-initialize. This matches Discord's behavior.

**Adaptive frame throttle (DXGI):** DXGI delivers at monitor refresh rate (60–360 Hz). We need exactly `target_fps` (typically 60). On startup, we calibrate the monitor's vsync interval from the first two acquired frames, then set a deadline of `target_interval - half_vsync` so that on any refresh rate we accept the closest vsync that satisfies the target — no excess frames, no under-delivery.

| Monitor Hz | Vsync | Deadline (60fps) | Delivered fps |
|------------|-------|-------------------|---------------|
| 60 | 16.67ms | 13.3ms | 60 (every frame) |
| 120 | 8.33ms | 12.5ms | 60 (every 2nd) |
| 144 | 6.94ms | 13.2ms | 60–72 (every 2nd) |
| 240 | 4.17ms | 14.6ms | 60 (every 4th) |

WGC frame pool uses 3 buffers to avoid stalls from the OS frame queue.

### 2. Preprocessing

GPU-side BGRA → NV12 conversion via a D3D11 compute shader. Uses a 3-slot NV12 ring buffer (`NV12_RING_SLOTS = 3`) so the convert output doesn't alias with an in-flight encode input. Typical `convert_ms` is 0.1–0.3ms (negligible).

### 3. Encode

NVENC hardware encoder. Current config:
- **Preset:** P1 + Ultra Low Latency (fastest hardware preset — matches Discord/Parsec)
- **Rate control:** CBR at configured bitrate (default 8 Mbps)
- **Codec:** H.264
- Fallback chain: P1+ULL → P1+LL → P4+ULL

Texture registration is cached per NV12 ring slot (`reg_cache_`) so `nvEncRegisterResource` runs once per slot, not per frame.

### 4. Encode Queue (decoupling capture from encode)

A dedicated `encode_thread` pulls from a bounded ring queue (`ENCODE_QUEUE_CAP = 2`). When the queue is full, the oldest job is evicted (newest-wins policy). This decouples the capture callback from the synchronous `nvEncEncodePicture` + `nvEncLockBitstream` blocking time.

Diagnostics emitted every 300 frames: `convert_ms`, `encode_ms`, `eq_depth`, `eq_drops`.

### 5. Chunking & Egress

Encoded NALUs are chunked at 40 KB (`SFU_CHUNK_MAX_PAYLOAD`) with a 12-byte header (msg_id, chunk_idx, chunk_count, payload). Sent over an unreliable DataChannel. The `SfuSink` egress task is lazily spawned on the first `enqueue_chunked_media` call (avoids Tokio runtime requirement at construction time).

---

## Viewer Pipeline (Network → Display)

1. **Reassembly:** chunks are collected by msg_id; once all chunks for a message arrive, the full NALU is delivered to decode.
2. **Decode:** DXVA hardware decode (Windows) or VideoToolbox (macOS). Pre-keyframe packets are dropped until the first keyframe is received.
3. **Present:** decoded frames are presented via the Slint UI render loop.

Key viewer diagnostics (per-second): `dec_fps`, `decode_stall_ms`, `ingress_kbps`, `chunk_completed_hz`, `backlog_guard` state.

---

## Stream Manager & Recovery

`mello-core::stream::manager` runs the host-side control loop:
- Bounded coalescing drain to avoid run-loop starvation.
- Queue-pressure keyframe requests (rate-limited, severe-coalesce only).
- Per-second diagnostics: `video_in_hz`, `send_fail_*_delta`, `recovery_mode`, queue lengths.

Viewer-side IDR request policy: rate-limited to one every 4s, requires 4 consecutive unrecoverable groups, and suppressed if a keyframe was recently received.

---

## Current Performance (measured 2026-04-22, release build)

**Setup:** Windows 11, RTX 3080 Ti, 144 Hz monitor, CS2 fullscreen 1920×1200, host+viewer on same machine via SFU.

| Metric | Value |
|--------|-------|
| NVENC preset | P1 + ULL |
| DXGI capture delivered | ~72 fps |
| `encode_ms` | 8–18ms (see TODO below) |
| `video_in_hz` (host) | 50–68, avg ~57 |
| `dec_fps` (viewer) | 50–67, avg ~58 |
| `decode_stall_ms` | 2–22ms |
| `eq_drops` | ~15/sec (72 capture - 57 encode) |
| `send_fail_video_delta` | 0 |
| `chunk_invalid/late/evicted` | 0 |
| `backlog_guard` activations | 0 |
| Bitrate | ~7–9 Mbps |

Deferred start, hot-swap, and fullscreen detection all working correctly.

---

## Probe Tooling

- **`stream-host`** — standalone host probe with Nakama auto-start (`--nakama-start-stream --crew-id`). Runs in release mode via `run-stream-host.ps1`.
- **`sfu-stream-viewer-probe`** — standalone viewer probe with auto `watch_stream` via Nakama. Per-second telemetry including decode, present, ingress, chunk, and backlog metrics. Runs in release mode via `run-stream-viewer.ps1`.
- **`coalesce_stream_timeline.py`** — merges host + viewer + SFU logs into a single ordered timeline for analysis. Supports `--session auto`.

---

## TODO — Path to Buttery 60fps

### High priority

1. **Async encode (eliminate `LockBitstream` blocking)**
   `encode_ms` is 8–18ms on P1+ULL which should be 1–3ms. The entire `nvEncEncodePicture` → `nvEncLockBitstream` path is synchronous — `LockBitstream` blocks until the GPU finishes. Switch to async mode: use `NV_ENC_PIC_PARAMS::completionEvent` with a Windows event object, call `nvEncEncodePicture` (returns immediately), then `WaitForSingleObject` on the event. This should bring `encode_ms` to true hardware latency (~2ms) and close the 57→60fps gap.

2. **Raise `ENCODE_QUEUE_CAP` to 3 or 4**
   Currently 2. With async encode + fast drain, a slightly larger queue provides better jitter absorption without meaningful latency increase. Should reduce `eq_drops` to near zero.

3. **Viewer-side frame pacing / jitter buffer**
   Currently the viewer presents frames immediately on decode completion. A small jitter buffer (1–2 frames) would smooth out decode timing variance and eliminate the residual 50→67fps oscillation visible in logs.

### Medium priority

4. **WGC capture rate parity**
   WGC doesn't have the same vsync-aligned delivery as DXGI. Validate capture rate when running windowed games and apply similar adaptive throttling if needed.

5. **AMF / QSV encoder backends**
   Currently NVENC-only. AMD (AMF) and Intel (QSV) hardware encoders need the same P1-equivalent fast-preset + async treatment for non-NVIDIA users.

6. **ABR v2 refinement**
   Bandwidth-aware bitrate adaptation exists but needs tuning: smoother ramp-up/down curves, oscillation dampening, and trend-based prediction instead of reactive step changes.

7. **Encoder resolution scaling**
   When bandwidth is constrained, dynamically scale encode resolution (1080p → 720p) rather than just dropping bitrate. Requires re-initializing NVENC session but avoids blocky artifacts at low bitrate.

### Lower priority

8. **DataChannel vs RTP evaluation**
   Current transport uses unreliable DataChannels with application-layer chunking/reassembly. RTP would give us standard jitter buffers, NACK-based retransmission, and FEC. Worth evaluating once the current path hits its ceiling.

9. **macOS viewer VideoToolbox stability**
   VTDecompressionSession churn (status `-12909`) causes decode FPS drops on macOS. Need to keep sessions alive when SPS/PPS are unchanged and rate-limit session recreation.

10. **P2P fallback parity**
    Ensure quality and stability match SFU path under adverse network conditions.

11. **Voice CPU efficiency**
    Current in-call CPU overhead is ~10% vs Discord's <2%. Needs profiling across capture, DSP, encode/decode, and mix paths.

---

## Key Files

| Area | Path |
|------|------|
| DXGI capture | `libmello/src/video/capture_dxgi.cpp` |
| WGC capture | `libmello/src/video/capture_wgc.cpp` |
| Process capture + hot-swap | `libmello/src/video/capture_process.cpp` |
| GPU preprocessor | `libmello/src/video/video_preprocessor.cpp` |
| Video pipeline + encode queue | `libmello/src/video/video_pipeline.cpp` |
| NVENC encoder | `libmello/src/video/encoder_nvenc.cpp` |
| SFU sink + chunking | `mello-core/src/stream/sink_sfu.rs` |
| Stream manager | `mello-core/src/stream/manager.rs` |
| Host probe | `tools/stream-host/src/main.rs` |
| Viewer probe | `tools/sfu-stream-viewer-probe/src/main.rs` |
| Timeline analysis | `scripts/coalesce_stream_timeline.py` |
| Launch scripts | `scripts/run-stream-host.ps1`, `scripts/run-stream-viewer.ps1` |
