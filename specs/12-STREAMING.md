# Streaming

> **Component:** libmello (C++) · mello-core (Rust) · mello-client (Rust/Slint) · Backend (Go/Nakama)
> **Status:** Windows DComp underlay rendering integrated, geometry sync implemented, tuning/validation in progress
> **Related:** [03-LIBMELLO.md](./03-LIBMELLO.md), [02-MELLO-CORE.md](./02-MELLO-CORE.md), [14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md), [13-SFU.md](./13-SFU.md)

---

## 1. Goals

Ship 1080p60 game streaming comparable to Discord/Parsec with low idle RAM and hardware-composited video in the desktop client: hardware-accelerated encode/decode, sub-60ms WAN latency, and stable UI render cadence. Favor visible quality loss (artifacts, lower bitrate) over lag or stalling.

---

## 2. Layer Overview

The streaming system is split across four layers. Understanding ownership boundaries is the most important thing for working on this stack.

```
┌─────────────────────────────────────────────────────────────────┐
│  mello-client (Rust/Slint)                                      │
│  DComp underlay presenter, geometry sync, stream card UI         │
├─────────────────────────────────────────────────────────────────┤
│  mello-core  (Rust)                                             │
│  Stream lifecycle, transport, framing, FEC, ABR, recovery,      │
│  viewer chunk reassembly, frame-lifecycle contract, telemetry    │
├─────────────────────────────────────────────────────────────────┤
│  libmello  (C++)                                                │
│  Hardware capture, GPU color conversion, hardware encode/decode, │
│  decoded-frame ring, native frame callback (NT shared handles)   │
├─────────────────────────────────────────────────────────────────┤
│  mello-sys  (Rust FFI bindings, auto-generated via bindgen)     │
│  Thin unsafe bridge between mello-core and libmello             │
└─────────────────────────────────────────────────────────────────┘
```

mello-core never touches pixel memory. libmello never touches transport policy. mello-client owns presentation and composition. `mello-sys` is the FFI membrane.

---

## 3. Host Pipeline

The host captures frames, converts them, encodes them, and hands encoded NALUs to the Rust layer which chunks and sends them over the network.

```
Capture → GPU Preprocess → Encode Queue → Encode Thread → Stream Manager → Egress
(DXGI/WGC) (BGRA→NV12)    (bounded ring)   (NVENC async)   (FEC, chunking)  (SFU/P2P)
```

### 3.1 Capture

Two backends, selected automatically per-process:

| Backend | API | When |
|---------|-----|------|
| **DXGI-DDI** | `IDXGIOutputDuplication` | Fullscreen / exclusive-fullscreen games |
| **WGC** | `Windows.Graphics.Capture` | Windowed games |

`ProcessCapture` wraps both. Given a PID it finds the main game window (`EnumWindows`, largest restored-area, non-toolwindow), detects fullscreen (covers ≥90% of monitor), and picks the backend. A background `monitor_thread` periodically re-evaluates and hot-swaps if the game transitions windowed↔fullscreen — triggering a keyframe on swap.

**Deferred start:** If the target window is minimized at stream start (user tabbed out to launch the stream), capture waits. The monitor thread polls until the window is restored, then initializes the backend. Width/height return restored dimensions during the wait so the encoder can pre-initialize. This matches Discord's behaviour.

**Adaptive DXGI throttle:** DXGI delivers at the monitor's refresh rate (60–360 Hz). We only want `target_fps` (typically 60). On startup, we calibrate the monitor's vsync interval from the first two acquired frames, then set a deadline of `target_interval - half_vsync`. This ensures we accept the closest vsync that satisfies the target on any refresh rate, without over- or under-delivering.

**macOS:** `ScreenCaptureKit` (SCK) backend exists for macOS capture.

### 3.2 GPU Preprocessing

BGRA→NV12 conversion via a D3D11 compute shader. Uses a 3-slot NV12 ring buffer so the convert output doesn't alias with an in-flight encode input. Typical `convert_ms` is 0.1–0.3ms. Also handles GPU downscale when the capture resolution exceeds the target encode resolution.

### 3.3 Encode Queue

A dedicated `encode_thread` pulls from a bounded ring queue (`ENCODE_QUEUE_CAP = 2`). When the queue is full, the oldest job is evicted (newest-wins). This decouples the capture callback thread from the potentially-blocking encode path.

### 3.4 Hardware Encode

NVENC with P1+ULL (Ultra Low Latency) preset by default. Fallback chain: P1+ULL → P1+LL → P4+ULL.

**Async mode:** The encoder initializes with `enableEncodeAsync = 1` and registers a Windows completion event. `nvEncEncodePicture` returns immediately while the GPU works; the encode thread waits on the event before calling `nvEncLockBitstream`. Falls back to synchronous mode if the driver doesn't support async.

Rate control is VBR with 1.25× max headroom. Texture registration is cached per NV12 ring slot so `nvEncRegisterResource` runs once per slot, not per frame. `repeatSPSPPS = 1` ensures every keyframe is self-contained.

**Other encoder backends:** AMF (AMD), QSV/oneVPL (Intel), VideoToolbox (macOS) exist in the codebase but are less battle-tested than NVENC.

### 3.5 Encoded Packet Handoff

The encode thread's `packet_cb_` fires with the encoded NALU bytes. This callback was set up by `mello-core` via `mello_stream_start_host` — it sends the bytes over an mpsc channel (capacity 32) to the Rust `StreamManager`.

---

## 4. Stream Manager (Host-side Rust)

`mello-core::stream::manager::StreamManager` is the host-side control loop. It receives encoded video and audio packets from libmello and routes them through FEC, chunking, and the transport sink.

### What it does each tick:

1. **Drain video packets** from the mpsc channel (bounded coalescing to avoid starvation).
2. **Wrap** each NALU in a `StreamPacket` (12-byte header: type, flags, sequence, timestamp).
3. **FEC encode** — for every N video packets, emit one XOR parity packet. FEC group resets on each keyframe. Group size is controlled by ABR.
4. **Send** via the `PacketSink` trait (see §6).
5. **Drain audio packets** similarly (no FEC — Opus has built-in PLC).
6. **Process control packets** from viewers (loss reports, keyframe requests).
7. **Emit telemetry** every second: `video_in_hz`, `send_fail_*_delta`, `recovery_mode`, queue depths.

### Recovery policy

- **Queue-pressure keyframe:** If the video queue grows too large (severe coalescing), force an IDR. Rate-limited.
- **Viewer-requested keyframe:** Forwarded from control packets, rate-limited.
- **Recovery mode:** Temporary state after sustained losses — drops delta frames until next keyframe to help the decoder converge faster.

---

## 5. Packet Format

### 5.1 StreamPacket wire format

Every packet on the wire has a 12-byte header:

```
[type:1][flags:1][sequence:2 BE][timestamp_us:8 BE][payload...]

type: 0x01=Video, 0x02=Audio, 0x03=FEC, 0x04=Control
flags: bit0=IS_KEYFRAME, bit1=FEC_GROUP_LAST, bit2=CODEC_AV1
```

Implementation: `mello-core/src/stream/packet.rs`.

### 5.2 DataChannel Message Chunking

Encoded packets (especially keyframes at 100–400 KB) must be chunked before sending over unreliable DataChannels. SCTP fragments large messages internally, and losing a single fragment in unreliable mode drops the entire message.

Each `StreamPacket` is split into chunks with a 6-byte header:

```
[msg_id:2 LE][chunk_idx:2 LE][chunk_count:2 LE][payload ≤ N bytes]
```

Chunk payload limits differ by transport:
- **SFU:** 40,000 bytes (`SFU_CHUNK_MAX_PAYLOAD`)
- **P2P:** 60,000 bytes (`CHUNK_MAX_PAYLOAD`)

**Whole-frame drop policy:** Before chunking, the sender checks that the egress queue has room for all chunks. If not, the entire frame is dropped — never partial. This prevents the viewer from receiving incomplete messages it can never reassemble.

### 5.3 FEC

XOR-based forward error correction over groups of N video packets. When a group completes, one parity packet (XOR of all N payloads) is sent.

- Loss < 1%: FEC disabled
- Loss 1–5%: N = 10 (10% overhead)
- Loss > 5%: N = 5 (20% overhead)

FEC recovers any single packet loss within a group with zero latency and zero round-trips. Group boundaries reset on keyframes. Implementation: `mello-core/src/stream/fec.rs`.

---

## 6. Transport

### 6.1 PacketSink Trait

The stream manager sends packets to a `PacketSink` — it doesn't know whether they go to P2P peers or an SFU. Two implementations:

| Sink | Transport | Max viewers | Chunk size |
|------|-----------|-------------|------------|
| `P2PFanoutSink` | Direct DataChannel per viewer | 5 | 60 KB |
| `SfuSink` | Single SFU WebSocket + DataChannel | Unlimited (SFU-managed) | 40 KB |

Both sinks have an async egress task with a bounded mpsc queue and a token-bucket `EgressPacer`. There's also a `DualSink` that sends to both simultaneously (e.g. P2P + SFU during migration).

### 6.2 SFU Connection

`SfuConnection` handles the SFU lifecycle: WebSocket signaling (connect, join, negotiate ICE/SDP), DataChannel media/control/audio send, and event polling. The SFU is a Go service (`mello-sfu`) that forwards media without transcoding.

When the SFU media channel is closed, send attempts return errors that flow through the existing `video_send_fail_total` telemetry counters.

### 6.3 Topology Selection

The client never decides topology. The backend's `start_stream` RPC response carries `mode: "p2p" | "sfu"` based on the crew's entitlement. mello-core instantiates the appropriate sink.

---

## 7. Viewer Pipeline

```
DataChannel → Chunk Reassembly → StreamViewer → Decode → NativeSurfaceFrame slot → DComp underlay
                (ChunkAssembler)   (FEC, loss)    (HW dec)   (latest-frame-wins)      (shared texture → swap chain)
```

### 7.1 Chunk Reassembly

`ChunkAssembler` in `mello-core/src/client/stream_ffi.rs` collects incoming chunks by `msg_id`. When all `chunk_count` chunks arrive, the original payload is reconstructed. Incomplete assemblies are evicted when they fall 64 msg_ids behind or after 500ms.

### 7.2 StreamViewer (Rust)

`StreamViewer` handles FEC decode, loss tracking, and IDR request policy:

- **Pre-keyframe gating:** All packets before the first keyframe are dropped.
- **FEC recovery:** `FecDecoder` can recover a single missing packet per group.
- **IDR request:** After 4 consecutive unrecoverable FEC groups, request a keyframe from the host. Rate-limited to once per 4 seconds, and suppressed if a keyframe was received within the last 2 seconds.
- **H.264 IDR detection:** Scans all NALs in the access unit for type 5 (IDR), not just the first. Needed because NVENC emits SPS+PPS before the IDR slice.
- **Loss reports:** Sent to the host every second with packets received/lost and observed rx bitrate.

### 7.3 Hardware Decode

NVDEC (CUDA↔D3D11 interop, zero-copy R8 layout), AMF, D3D11VA, OpenH264 on Windows. VideoToolbox on macOS. The decoder outputs to a GPU texture which goes into the decoded-frame ring.

### 7.4 Decoded-Frame Ring

A 3-slot ring buffer in `VideoPipeline` holds decoded GPU textures. Guarded by a mutex: `push_decoded` (decode/feed thread) and `pop_decoded` (present path) are synchronized.

When the ring is full, the oldest frame is evicted (newest-wins, same principle as the encode queue).

### 7.5 Jitter Buffer and Native Surface Contract

`present_frame()` doesn't pop immediately. It waits until the ring has >= 2 frames (or 50ms since the last present, whichever comes first). This absorbs network/decode jitter and stabilizes cadence.

The Rust `stream_tick` drives `mello_stream_present_frame`, which emits `on_viewer_native_frame` metadata into a single latest-frame slot (`NativeSurfaceFrame`). The slot carries an NT shared handle (`DXGI_FORMAT_R8G8B8A8_UNORM`) created by libmello via `IDXGIResource1::CreateSharedHandle`. The client's DComp presenter opens the handle with `ID3D11Device1::OpenSharedResource1` and copies it to the swap chain back buffer.

### 7.6 DirectComposition Underlay Rendering (Windows)

Video frames bypass Slint's renderer entirely. A separate D3D11 device, composition swap chain, and DComp visual tree are created when the viewer starts watching. Slint continues to run with its default software renderer for the UI, keeping idle RAM low (~80 MB target). The GPU context exists only while a stream is active.

**DComp visual tree:**

```
IDCompositionTarget (bound to the Slint HWND)
  └─ IDCompositionVisual
       ├─ Content: IDXGISwapChain1 (CreateSwapChainForComposition)
       ├─ Offset: SetOffsetX/Y (physical pixels)
       ├─ Transform: Matrix3x2 scale (stream resolution → card size)
       └─ Clip: IDCompositionRectangleClip (scroll viewport intersection)
```

**Per-frame present path:** The 16ms frame timer reads the latest `NativeSurfaceFrame` shared handle, opens it via `OpenSharedResource1`, copies to the swap chain back buffer with `CopyResource`, and calls `Present(0, 0)` (non-blocking, DWM manages VSync).

**Geometry sync:** The Slint stream card contains a zero-size `geo-tracker` element with properties bound to `media-rect.absolute-position` and dimensions. Slint `changed` handlers fire a `VideoRect.geometry-changed` callback synchronously during every layout pass (scroll, resize, reflow). Rust wires this callback to `DCompPresenter::update_geometry`, which:

1. Multiplies logical pixel coords by `window.scale_factor()` to get physical pixels.
2. Intersects the canvas rect with the scroll container (Flickable) viewport.
3. Calls `SetOffsetX/Y`, `SetTransform2` (scale matrix), `SetClip` (viewport intersection), `Commit`.
4. When fully scrolled out of view, removes swap chain content from the visual (`SetContent(None)`).

This pipeline runs entirely on the UI thread with no queueing, so geometry tracks the Slint layout frame-by-frame. Scroll is cheap: only offset + clip + commit, no swap chain resize.

**Swap chain format:** `DXGI_FORMAT_R8G8B8A8_UNORM`, `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`, 2 buffers, `DXGI_ALPHA_MODE_IGNORE`. Matches libmello's shared texture format. Swap chain is created at stream resolution; a DComp scale transform maps it to the card's display size, avoiding `ResizeBuffers` during window resize.

**Lifecycle:** `DCompPresenter` is created when `StreamWatching` fires and dropped on `StreamWatchingStopped`. The Slint card's video area is transparent, letting the DComp underlay show through. The `Image` element is kept but hidden (`visible: false`) since the DComp layer renders the actual video.

### 7.7 Backlog Guard

If the decode queue depth exceeds a threshold, the viewer drops incoming delta frames (keeping keyframes) and optionally requests an IDR. This prevents the decode ring from falling behind during sustained network bursts.

---

## 8. Quality Presets and ABR

### 8.1 Presets

| Preset | Resolution | FPS | Bitrate (H.264) | FEC N |
|--------|-----------|-----|-----------------|-------|
| **Ultra** | 1920×1080 | 60 | 8 Mbps | 5 |
| **High** | 1920×1080 | 60 | 6 Mbps | 5 |
| **Medium** | 1920×1080 | 30 | 4 Mbps | 5 |
| **Low** | 1280×720 | 30 | 2.5 Mbps | 3 |
| **Potato** | 854×480 | 30 | 1.5 Mbps | 3 |

Default is Medium. The host can select a preset before starting. The GPU preprocessor downscales capture to the preset's target resolution.

### 8.2 Adaptive Bitrate

`AbrController` adjusts bitrate and FEC group size based on viewer loss reports:
- **Step down:** >5% loss → reduce bitrate by 25%
- **Step up:** <1% loss for 10 consecutive seconds → increase bitrate by 10%
- **Floor:** Never below Potato bitrate
- **FEC adaptation:** Group size adjusted alongside bitrate based on loss ratio

In P2P mode, ABR can operate per-viewer. In SFU mode the host sends one stream; per-viewer adaptation is an SFU responsibility (future).

---

## 9. Audio Streaming

Game audio is captured via WASAPI loopback (the render endpoint). Mic audio and game audio are separate streams — not mixed before sending. The viewer receives and plays them independently, enabling future features like independent volume control.

The C API (`mello_stream_start_audio`, `mello_stream_feed_audio_packet`) exists but the audio capture implementation is currently stubbed. Audio packets use the same `StreamPacket` format with `type=0x02`, no FEC (Opus has built-in PLC).

---

## 10. Cursor Streaming

The host captures cursor state (position, visibility, shape RGBA) alongside video frames. Cursor data is serialized into a compact binary packet and sent via the control channel. The viewer deserializes and renders the cursor overlay independently of video frames.

---

## 11. Stream Lifecycle

### Host start

1. User picks a capture source (game process or monitor) in the UI.
2. `mello-core` calls `start_stream` RPC to Nakama → gets session ID, mode, SFU endpoint.
3. `start_host` creates the libmello video pipeline (capture + preprocess + encoder) and sets up mpsc channels for encoded packets.
4. `create_stream_session` creates a `StreamManager` with the appropriate `PacketSink` and spawns its async `run` loop.

### Viewer start

1. Viewer discovers stream via crew state (Nakama).
2. For SFU: connects to the SFU endpoint, joins the session, negotiates WebRTC.
3. Waits for the first signaling exchange to learn the host's encode resolution.
4. Creates the decoder pipeline at the correct resolution (`mello_stream_start_viewer`).
5. Creates `DCompPresenter` with the stream resolution and parent HWND (Windows).
6. `stream_tick` runs each frame: poll network → reassemble → feed decoder → present shared handle → DComp swap chain.
7. `VideoRect.geometry-changed` callback keeps the DComp visual in sync with the Slint card layout.

### Teardown

Both sides: stop the pipeline, drain queues, release GPU resources, leave the SFU/P2P session.

---

## 12. Telemetry and Diagnostics

### Host-side (per second)

`video_in_hz`, `audio_in_hz`, `coalesced_hz`, `recovery_mode`, `keyframe_req_*_total`, `send_fail_video_delta`, `send_fail_fec_delta`, `send_fail_audio_delta`, video/audio queue lengths and max, pacing stats.

Encoder periodic (every 300 frames): `convert_ms`, `encode_ms`, `eq_depth`, `eq_drops`.

### Viewer-side (per second)

`dec_fps`, `native_fps`, `present_true_hz`, `ingress_kbps`, `feed_video_hz`, `feed_video_fail_hz`, `decode_stall_ms`, `decode_backlog_est`, chunk stats (`completed_hz`, `invalid_hz`, `evicted_hz`, `late_hz`), `backlog_guard_*`.

DComp presenter diagnostics:

- `ui_render_fps` (DComp present cadence)
- `presented_frames` (total frames presented to swap chain)
- native surface descriptor cadence + sequence gaps
- `DComp present failed` error logs (OpenSharedResource1, CopyResource, Present failures)
- geometry-changed callback frequency (implicit via scroll/resize tracking)
- explicit fatal init error logs that trigger clean `StopWatching`

### Probe tools

| Tool | Purpose |
|------|---------|
| `tools/stream-host` | Standalone host with Nakama auto-start, release mode |
| `tools/sfu-stream-viewer-probe` | Standalone viewer with full per-second telemetry |
| `scripts/coalesce_stream_timeline.py` | Merges host + viewer + SFU logs into a single timeline |
| `scripts/run-stream-host.ps1` | Launch script (default 60fps, release) |
| `scripts/run-stream-viewer.ps1` | Launch script (release) |

---

## 13. Key Files

| Area | Path |
|------|------|
| **Rust stream module** | `mello-core/src/stream/` (14 files) |
| Stream manager | `mello-core/src/stream/manager.rs` |
| PacketSink trait | `mello-core/src/stream/sink.rs` |
| SFU sink + chunking | `mello-core/src/stream/sink_sfu.rs` |
| P2P fan-out sink | `mello-core/src/stream/sink_p2p.rs` |
| Viewer FEC/loss/IDR | `mello-core/src/stream/viewer.rs` |
| Packet format | `mello-core/src/stream/packet.rs` |
| Quality presets + config | `mello-core/src/stream/config.rs` |
| FEC encoder/decoder | `mello-core/src/stream/fec.rs` |
| ABR controller | `mello-core/src/stream/abr.rs` |
| Host session setup | `mello-core/src/stream/host.rs` |
| Viewer tick loop | `mello-core/src/client/streaming.rs` |
| Chunk assembler | `mello-core/src/client/stream_ffi.rs` |
| SFU connection | `mello-core/src/transport/sfu_connection.rs` |
| DComp presenter (Windows) | `client/src/dcomp_presenter.rs` |
| Client render loop + metrics | `client/src/main.rs` |
| Slint stream card UI + geo-tracker | `client/ui/panels/active_streams_panel.slint` |
| VideoRect global (geometry callback) | `client/ui/types.slint` |
| CrewFeed (Flickable viewport source) | `client/ui/panels/crew_feed.slint` |
| **C++ video pipeline** | `libmello/src/video/video_pipeline.cpp` |
| DXGI capture | `libmello/src/video/capture_dxgi.cpp` |
| WGC capture | `libmello/src/video/capture_wgc.cpp` |
| Process capture + hot-swap | `libmello/src/video/capture_process.cpp` |
| GPU preprocessor | `libmello/src/video/video_preprocessor.cpp` |
| NVENC encoder | `libmello/src/video/encoder_nvenc.cpp` |
| Encoder factory | `libmello/src/video/encoder_factory.cpp` |
| Decoder factory | `libmello/src/video/decoder_factory.cpp` |
| NVDEC decoder | `libmello/src/video/decoder_nvdec.cpp` |
| Staging / readback | `libmello/src/video/staging_texture.cpp` |
| C API (streaming) | `libmello/src/mello.cpp` (search `mello_stream_`) |
| **Probe tools** | |
| Stream host probe | `tools/stream-host/src/main.rs` |
| Viewer probe | `tools/sfu-stream-viewer-probe/src/main.rs` |
| Timeline script | `scripts/coalesce_stream_timeline.py` |

---

## 14. Current State and Known Gaps

**What works well:** Process-aware capture with hot-swap, deferred start, DXGI adaptive throttle, GPU preprocessing, async NVENC, mutex-guarded decoded ring, whole-frame egress drops, proper IDR detection, SFU telemetry, jitter buffer, DComp underlay rendering with NT shared handle import (Windows RGBA8 path), callback-driven geometry sync (scroll/resize/DPI), scroll viewport clipping, FEC, rate-limited recovery, probe tooling.

**Known gaps and future work:**

| Gap | Impact | Effort |
|-----|--------|--------|
| WGC has no frame throttling (accepts compositor-rate) | Excess encode queue pressure for windowed games | Medium |
| AMF/QSV encoders less tested | No smooth experience for AMD/Intel GPU users | Medium |
| Viewer jitter buffer is simple depth-gate, not PID-paced | Residual cadence oscillation under varying network conditions | Medium |
| No dynamic resolution scaling | Under severe bandwidth constraints, quality degrades but resolution stays fixed | Medium |
| Audio capture is stubbed | Game audio doesn't stream yet | Small |
| Input passthrough not implemented | No remote control | Large |
| ABR needs tuning | Step changes can oscillate; needs trend-based smoothing | Medium |
| DComp visual uses overlay, not true underlay (`WS_EX_NOREDIRECTIONBITMAP` not set) | Video composites on top of Slint content; stream card badges moved to bottom bar as workaround | Medium |
| Adapter/device mismatch diagnostics are log-based only | Better in-UI error reasons still needed | Small |
| 720p60/1080p60 acceptance sweep still pending on full host/viewer setup | Performance target not yet certified end-to-end | Medium |
| macOS viewer has no DComp equivalent | macOS needs its own compositor path (Core Animation layer) | Medium |
| macOS VideoToolbox session churn | Decode FPS drops on SPS/PPS change | Small |
| Per-viewer ABR in SFU mode | SFU doesn't transcode; all viewers get same bitrate | Large (SFU work) |

---

## 15. Validation Playbook (Windows)

Use this checklist to validate the DComp underlay rendering path after stream-related changes.

### 15.1 Pre-conditions

- Host and viewer run in release mode.
- Test both 720p60 and 1080p60 scenarios.
- Keep game scene representative (motion + static UI content).

### 15.2 Run commands

Host:

- `./scripts/run-stream-host.ps1 -CrewId "<crew-id>"`

Viewer:

- `./scripts/run-stream-viewer.ps1 -HostId "<host-user-id>" -CrewId "<crew-id>"`

Optional local loopback smoke:

- `cargo test -p mello-sys --test video_pipeline host_to_viewer_loopback -- --nocapture`

### 15.3 Acceptance gates

- `dbg_stream_ui_render_fps` tracks near source cadence (target: near 60 on stable 60fps source).
- `DComp present failed` error logs stay at zero during steady state.
- Scrolling the feed: video moves perfectly with the card, no stutter, no bleed outside the scroll container.
- Resizing the window: video scales with the card, no crash, no black frames.
- Scrolling the card fully out of view: DComp visual is hidden (no overlap with surrounding content).
- DPI change (drag between monitors): video repositions correctly, no crash.
- watch-stream init failures surface as explicit UI/log errors and stop watching cleanly.
- Idle RAM stays below ~80 MB (Slint software renderer, no GPU context when not streaming).
