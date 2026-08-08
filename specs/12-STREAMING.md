# Streaming

> **Component:** libmello (C++) · mello-core (Rust) · mello-client (Rust/Slint) · Backend (Go/Nakama)
> **Status:** H.264 RTP video path implemented (P2P + SFU); production SFU deployed; client runtime certification remains
> **Related:** [03-LIBMELLO.md](./03-LIBMELLO.md), [02-MELLO-CORE.md](./02-MELLO-CORE.md), [14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md), [EXTERNAL-SFU.md](./EXTERNAL-SFU.md)

---

## 1. Goals

Ship 1080p60 game streaming comparable to Discord/Parsec with low idle RAM and hardware-composited video in the desktop client: hardware-accelerated encode/decode, sub-60ms WAN latency, and stable UI render cadence. Favor visible quality loss (artifacts, lower bitrate) over lag or stalling.

---

## 2. Layer Overview

The streaming system is split across four layers. Understanding ownership boundaries is the most important thing for working on this stack.

```
┌─────────────────────────────────────────────────────────────────┐
│  mello-client (Rust/Slint)                                      │
│  DComp composition presenter, geometry sync, stream card UI      │
├─────────────────────────────────────────────────────────────────┤
│  mello-core  (Rust)                                             │
│  Stream lifecycle, native RTP transport, REMB congestion,       │
│  PLI/IDR recovery, access-unit polling, frame-lifecycle, telemetry │
├─────────────────────────────────────────────────────────────────┤
│  libmello  (C++)                                                │
│  Hardware capture, GPU color conversion, hardware encode/decode, │
│  decoded-frame ring, native frame callback (NT shared handles)   │
├─────────────────────────────────────────────────────────────────┤
│  mello-sys  (Rust FFI bindings, auto-generated via bindgen)     │
│  Thin unsafe bridge between mello-core and libmello             │
└─────────────────────────────────────────────────────────────────┘
```

mello-core never touches pixel memory. libmello owns native RTP mechanics (packetization, pacing, repair, RTCP, and AU assembly), while mello-core owns topology, congestion, and recovery policy. mello-client owns presentation and composition. `mello-sys` is the FFI membrane.

---

## 3. Host Pipeline

The host captures frames, converts them, encodes them, and hands complete Annex-B access units to the Rust layer, which forwards them through native H.264 RTP senders.

```
Capture → GPU Preprocess → Encode Queue → Encode Thread → Stream Manager → RTP egress
(DXGI/WGC) (BGRA→NV12)    (bounded ring)   (NVENC async)   (pacing, REMB)   (SFU/P2P)
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

NVENC tries P4+ULL (Ultra Low Latency) first, then P1+ULL, then P1+LL. The effective preset is logged at init.

**Async mode:** The encoder initializes with `enableEncodeAsync = 1` and registers a Windows completion event. `nvEncEncodePicture` returns immediately while the GPU works; the encode thread waits on the event before calling `nvEncLockBitstream`. Falls back to synchronous mode if the driver doesn't support async.

Rate control is VBR with 1.25× max headroom. The VBV spans ~0.5 s of the max rate (floored at 4 frames of bits, `vbvInitialDelay = vbv/2`) so IDRs are not rate-starved. Bitrate reconfigures reuse the full init-time NVENC config (the driver does not merge sparse configs on re-init) and force an IDR only on down-steps > 25%. Texture registration is cached per NV12 ring slot so `nvEncRegisterResource` runs once per slot, not per frame. `repeatSPSPPS = 1` ensures every keyframe is self-contained.

**Other encoder backends:** AMF (AMD), QSV/oneVPL (Intel), VideoToolbox (macOS) exist in the codebase but are less battle-tested than NVENC.

### 3.5 Encoded Packet Handoff

The encode thread's `packet_cb_` fires with the encoded NALU bytes. This callback was set up by `mello-core` via `mello_stream_start_host` — it sends the bytes over an mpsc channel (capacity 32) to the Rust `StreamManager`.

---

## 4. Stream Manager (Host-side Rust)

`mello-core::stream::manager::StreamManager` is the host-side control loop. It receives encoded Annex-B access units from libmello and sends them through native RTP sinks (`PacketSink::send_video`).

### What it does each tick:

1. **Drain video packets** from the mpsc channel (bounded coalescing to avoid starvation).
2. **Send** complete access units via `PacketSink::send_video` (native RTP packetization in libmello).
3. **Poll RTCP feedback** (PLI, REMB) from sinks and request host keyframes or adjust bitrate.
4. **Aggregate REMB** — P2P: per-viewer minimum; SFU: single aggregated target from the SFU relay path.
5. **Send game audio** — Opus packets from loopback capture via `PacketSink::send_audio`.
6. **Emit telemetry** every second: `video_in_hz`, `audio_in_hz`, `audio_out_hz`, `send_fail_*_delta`, `recovery_mode`, queue depths, `bitrate_kbps`.

### Recovery policy

- **Queue-pressure keyframe:** If the video queue grows too large (severe coalescing), force an IDR. Rate-limited.
- **Viewer-requested keyframe:** Forwarded from control packets, rate-limited.
- **Recovery mode:** A reference-chain gap, queue overflow, or failed accepted AU enters an IDR gate. Queued/dependent deltas are dropped and one rate-limited local keyframe request is emitted; deltas remain gated until a complete IDR arrives.

---

## 5. Video Transport (H.264 RTP)

Video uses **H.264 RTP/RTCP** for media — no custom `StreamPacket` framing or DataChannel chunking. **ULPFEC** (RFC 5109 XOR parity, PT 127) is optional when negotiated in SDP.

| Parameter | Value |
|-----------|-------|
| Payload type | 96 (H.264 media) |
| FEC payload type | 127 (`ulpfec/90000`, RFC 5109 level-0 XOR parity) |
| FEC SSRC | `media_ssrc + 1` (parallel RTP stream; host offer includes `a=ssrc-group:FEC-FR`; SFU viewer leg uses separate FEC m-line/track) |
| Clock rate | 90 kHz |
| Packetization | RFC 6184 mode 1, max payload 1100 bytes |
| Host output | Annex-B access units (SPS/PPS on every IDR, no B-frames) |
| RTCP feedback | `nack`, `nack pli`, `goog-remb`, `transport-cc` |
| Header extension | TWCC (`transport-wide-cc`, id 3) |

**Sender (libmello):** bounded AU queue, per-packet leaky-bucket pacing (fragments of a frame are spread across their wire time, with a two-packet-interval lag allowance), a caching NACK responder whose retransmits drain through a priority RTX queue on the pacing worker (rate-accounted, 512-packet cache with 1 s TTL), PLI/REMB RTCP callbacks. Rust sets pacing ceilings via `PacketSink::set_pacing_kbps`. When ULPFEC is negotiated (`ulpfec/90000` PT 127 in SDP), the sender emits one parity packet per group of 10 consecutive media RTP packets (XOR over pre-TWCC-stamp bytes; parity rides PT 127 on `media_ssrc + 1`). The host offer SDP advertises `a=ssrc-group:FEC-FR media_ssrc (media_ssrc+1)` so intermediaries (SFU/Pion) bind the repair SSRC.

**TWCC congestion control:** when negotiated, egress packets carry transport-wide sequence numbers (stamped in the pacer at emit time; retransmits get fresh seqs). The receiver emits TWCC feedback every ~50 ms; the sender's delay-gradient estimator (GCC-style: accumulated-delay trendline, overuse detector, AIMD + loss cap) produces a send-side target. The pacer runs at `min(manager ceiling, estimator target)`; the estimator target is forwarded to Rust as `GCC_TARGET` feedback and applied to the encoder immediately (the estimator smooths internally — no 5 %/s REMB ramp). Per viewer, a fresh GCC estimate supersedes that viewer's REMB.

**Receiver (libmello):** reorder buffer, NACK/PLI/RR/REMB/TWCC. NACK retry budget is RTT-adaptive (one attempt per ~20 ms of measured RTT, clamped 2–8). Access units expire on a 120 ms stall (no fragment progress) or a 600 ms hard age cap, so paced large AUs (e.g. IDRs at low bitrates) complete while lost tails fail fast — only repaired complete access units reach the decoder. When ULPFEC is negotiated, parity packets (`ssrc == remote_media_ssrc + 1`, PT 127) feed a recovery buffer; `send_nack` tries XOR reconstruction before emitting NACK; eager repair retries on FEC and media arrivals and on idle worker ticks when covered sequences remain missing. Recovered packets are injected outside receiver callbacks (no re-entrancy). Stats: `rx_fec_recovered`, `rx_fec_unrecoverable`.

**Stream session DataChannel:** reliable `control` only (viewer PLI/loss metadata, cursor, ping/pong). There is no unreliable stream-video DataChannel. Host and viewer send a control-channel ping every ~2 s so both sides have a live RTT measurement (`rtt_ms` in telemetry).

Implementation: `libmello/src/transport/rtp_video_sender.cpp`, `rtp_video_receiver_session.cpp`; Rust wrappers in `mello-core/src/stream/rtp_peer.rs`.

---

## 6. Transport

### 6.1 PacketSink Trait

The stream manager sends access units to a `PacketSink` — it does not know whether they go to P2P peers or an SFU. Two implementations:

| Sink | Transport | Max viewers | Video path |
|------|-----------|-------------|------------|
| `P2PFanoutSink` | One native RTP sender per viewer | 5 | Independent seq/pacer/NACK per peer |
| `SfuSink` | One SFU signaling connection + one host RTP track | 100 by current backend/SFU defaults | One encoded stream; SFU relays RTP |

Both sinks expose `native_rtp_telemetry()`, `poll_video_feedback()` (PLI/REMB), and `set_pacing_kbps()`. P2P also fans out per-viewer REMB; SFU aggregates viewer REMB through the relay and reports it under synthetic viewer id `sfu`.

### 6.2 SFU Connection

`SfuConnection` handles the SFU lifecycle: WebSocket signaling (connect, join, negotiate ICE/SDP), **one H.264 RTP video track** (host send / viewer recv), reliable **control** DataChannel only, and event polling. The SFU is a Go service (`mello-sfu`) that forwards RTP without transcoding.

When the RTP video track or control channel closes, send attempts return errors that flow through `video_send_fail_total` telemetry counters.

### 6.3 Topology Selection

The backend is authoritative for topology. `start_stream` / `watch_stream` return `mode: "p2p" | "sfu"` based on entitlement and SFU configuration. If SFU token creation is unavailable, the backend may return P2P. Once an RPC returns `mode: "sfu"`, client connect/join/control-channel setup is fail-closed: it reports a stream error instead of silently switching that session to P2P.

---

## 7. Viewer Pipeline

```
RTP ingress → Access-unit poll → Pre-keyframe gate → Decode → NativeSurfaceFrame slot → DComp presenter
 (native RTCP)   (complete AU only)  (PLI/REMB)      (HW dec)   (latest-frame-wins)      (shared texture)
```

### 7.1 Native RTP receive

`poll_received_access_unit` in `mello-core/src/stream/rtp_peer.rs` pulls complete Annex-B access units from libmello's RTP receiver. Incomplete or gated access units are dropped before decode. The viewer's `ViewerCongestionController` samples native RTP stats every 500 ms and emits REMB receive targets when loss, jitter, or gate pressure warrants a change.

### 7.2 Viewer recovery policy

- **Pre-keyframe gating:** All access units before the first keyframe are dropped.
- **PLI/IDR:** Native receiver sends PLI when gated; host manager also honors viewer-join and queue-pressure keyframe requests.
- **H.264 IDR detection:** Scans all NALs in the access unit for type 5 (IDR), not just the first.
- **REMB uplink:** Viewer congestion controller drives receive targets; host steps down immediately on severe loss, ramps up slowly (5%/s cap in SFU aggregate mode).

### 7.3 Hardware Decode

NVDEC (CUDA↔D3D11 interop, zero-copy R8 layout), AMF, D3D11VA, OpenH264 on Windows. VideoToolbox on macOS. The decoder outputs to a GPU texture which goes into the decoded-frame ring. With async decode, CUDA/NVDEC runs on the decode worker; any D3D11 immediate-context updates (`CopyResource`, `UpdateSubresource`) are deferred to `Decoder::publish_d3d11_frame()` on the present/feed thread.

### 7.4 Decoded-Frame Ring

A 3-slot ring buffer in `VideoPipeline` holds decoded GPU textures. Guarded by a mutex: `push_decoded` (decode/feed thread) and `pop_decoded` (present path) are synchronized.

When the ring is full, the oldest frame is evicted (newest-wins, same principle as the encode queue).

### 7.5 Jitter Buffer and Native Surface Contract

`present_frame()` doesn't pop immediately. It waits until the ring has >= 2 frames (or 50ms since the last present, whichever comes first). This absorbs network/decode jitter and stabilizes cadence.

The Rust `stream_tick` drives `mello_stream_present_frame`, which emits `on_viewer_native_frame` metadata into a single latest-frame slot (`NativeSurfaceFrame`). The slot carries an NT shared handle (`DXGI_FORMAT_R8G8B8A8_UNORM`) created by libmello via `IDXGIResource1::CreateSharedHandle`. The client's DComp presenter opens the handle with `ID3D11Device1::OpenSharedResource1` and copies it to the swap chain back buffer.

### 7.6 DirectComposition Rendering (Windows)

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

**Lifecycle:** `DCompPresenter` is created when `StreamWatching` fires and dropped on `StreamWatchingStopped`. The Slint card's video area is transparent and the `Image` element is hidden (`visible: false`) because the DComp layer renders the actual video. The current visual is composition content above the Slint surface, not a true window underlay; see Known Gaps.

### 7.7 Backlog Guard

If the decode queue depth exceeds a threshold, the viewer drops incoming delta frames (keeping keyframes) and optionally requests an IDR. This prevents the decode ring from falling behind during sustained network bursts.

---

## 8. Quality Presets and Congestion Control

### 8.1 Presets

| Preset | Resolution | FPS | Bitrate (H.264) |
|--------|-----------|-----|-----------------|
| **Ultra** | 1920×1080 | 60 | 8 Mbps |
| **High** | 1920×1080 | 30 | 4.5 Mbps |
| **Medium** | 1280×720 | 60 | 4 Mbps |
| **Low** | 1280×720 | 30 | 2.5 Mbps |
| **Potato** | 854×480 | 30 | 1.5 Mbps |

Default is Medium. The host can select a preset before starting. The GPU preprocessor downscales capture to the preset's target resolution. Preset `fec_n` fields remain in config for schema compatibility but are unused on the RTP path.

### 8.2 REMB congestion control

**Viewer (`ViewerCongestionController`):** Samples native RTP receiver stats every 500 ms. Severe loss (>5%), incomplete AUs, or gate pressure step the receive target down 25%; mild loss (2–5%) or jitter >20 ms steps down 15%; ten consecutive good samples increase by max(100 kbps, 5%). Emits REMB at significant changes or every 2 s heartbeat.

**Host (`StreamManager`):** Applies the minimum fresh REMB target across active viewers (3 s stale expiry). Decreases apply immediately; increases are rate-limited to 5%/s. Bitrate changes trigger encoder reconfigure (+ IDR on down-steps > 25%). When every estimate is stale or missing (lost RTCP), the host **holds** the current target — restoring max on transient REMB loss ramps the host back into the congestion that caused the silence. A last-viewer-leave is an explicit signal and does restore the configured ceiling for the next viewer. Pacing target includes RTP header headroom via `calc_stream_pacing_target_kbps`.

In SFU mode all viewers share one encoded stream. The SFU terminates TWCC per hop: it generates TWCC feedback to the host (host→SFU leg), stamps per-leg transport-wide sequences toward viewers, and runs a per-viewer GCC estimator from each viewer's TWCC feedback. Each viewer leg has a token-bucket egress pacer tracking that estimate, and the aggregated minimum (GCC estimates, superseding client REMBs) is forwarded upstream as REMB on the ~1 s tick. The SFU also caches the last complete IDR access unit and replays it to newly wired viewers for instant late-join start.

---

## 9. Audio Streaming

Game audio is wired end-to-end on Windows and macOS:

| Parameter | Value |
|-----------|-------|
| Capture (Windows) | WASAPI loopback (`eRender` + `AUDCLNT_STREAMFLAGS_LOOPBACK`) |
| Capture (macOS) | ScreenCaptureKit, on the SCStream that already captures video |
| Encode | Opus stereo 48 kHz, 20 ms frames, `OPUS_APPLICATION_AUDIO`, ~96 kbps |
| RTP payload type | 111 (Opus), same as voice but on a separate stream peer connection |
| Host SDP | sendonly audio m-line alongside sendonly H.264 |
| Viewer SDP | recvonly audio m-line alongside recvonly H.264 |
| Viewer RTP receive | `RtcpReceivingSession` on the recvonly audio track before `onMessage` (libdatachannel media-receiver pattern) |
| SFU relay | 1 host ingress → N viewer fan-out (`fanOutAudioRTP`), queue depth 32 |
| Viewer playout | `mello_stream_feed_audio_packet` → Opus decode → `create_audio_playback()` (stereo: WASAPI on Windows, CoreAudio on macOS) |

Host path: `mello_stream_start_audio` → `MelloAudioPacketCallback` → `StreamManager::handle_audio` → `PacketSink::send_audio` → `mello_peer_send_audio`. Viewer path (SFU): `AudioTrackData` events; P2P: `mello_peer_set_audio_track_callback` → same feed function. Stream viewer receive is wired on the offered recvonly audio track (not voice `onTrack`); if `onTrack` fires for the same `mid`, callbacks move to that track instance.

### 9.1 Inbound audio framing

Packets handed to Rust by an audio track callback are **not** raw Opus. After
stripping the RTP header, `PeerConnectionImpl::wire_incoming_audio_track_callbacks`
prepends four bytes:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 2 | RTP sequence number, little-endian |
| 2 | 2 | Reserved, always zero |
| 4 | n | Opus payload |

Callers must skip those four bytes before decoding. A packet of four bytes or
fewer carries no payload and is dropped.

This framing is **shared with voice**, which strips it in `voice/mod.rs` before
forwarding to the SFU; the stream path strips it in
`client/stream_ffi.rs::feed_viewer_audio_packet` (`AUDIO_SEQ_HEADER_LEN`).

The sequence number is currently unused by both paths — RTP already handles
ordering and loss detection. It is carried because the receive handler has it
cheaply to hand, and would be the input to Opus PLC concealment if packet-loss
concealment is added later.

### 9.2 macOS capture (ScreenCaptureKit)

macOS has no loopback device. ScreenCaptureKit delivers system audio on the
same `SCStream` that already captures video, so `StreamAudioHostPipeline` opens
no source of its own: `mello.cpp` registers a `CaptureSource::set_audio_callback`
that pushes into `feed_float_pcm`.

Four constraints, none of which are visible from the call site:

- **`capturesAudio` is always enabled when hosting.** `SCStream` fixes it at
  *creation*, but `mello_stream_start_audio()` runs after
  `mello_stream_start_host()`. Turning it on later is impossible, so it is
  always on and the audio callback decides whether samples go anywhere.
- **SCK delivers planar (non-interleaved) float.** Audio must be read through
  `CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer`, one `AudioBuffer`
  per channel. Reading the block buffer directly as interleaved — the shape
  `CMBlockBufferGetDataPointer` invites — produces garbled audio with no error.
  Formats that are not 32-bit float are rejected and logged, not reinterpreted.
- **48 kHz only.** There is no resampler in libmello. SCK is configured for
  48 kHz stereo; audio arriving at another rate is dropped with a warning rather
  than encoded at the wrong pitch. Channel count *is* adapted — mono is
  duplicated to both sides, more than two channels keeps the front pair.
- **Detach before teardown.** The capture callback holds a raw pointer to the
  pipeline and fires on SCK's own audio queue, so both `mello_stream_stop_audio`
  and `mello_stream_stop_host` clear the callback before dropping the pipeline
  (`stream_audio_teardown`).

Covered by `libmello/tests/test_stream_audio_pipeline.cpp`, which decodes the
emitted Opus and asserts on signal level per channel. Packet counts alone do not
discriminate: the output buffer size drives them even when the conversion fills
only part of each frame.

**Windows smoke test (Aug 2026):** host `audio_out_hz≈50`, viewer `audio_fed_hz≈50`, `rx_audio_packets` climbing, `viewer playout started`. Same-machine host+viewer may loop back via WASAPI loopback — use headphones or separate machines for listen tests.

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
3. Uses the encode resolution and bitrate returned by `watch_stream` (falling back to crew-state/default values only for older responses).
4. Creates the decoder pipeline at the correct resolution (`mello_stream_start_viewer`).
5. Creates `DCompPresenter` with the stream resolution and parent HWND (Windows).
6. `stream_tick` runs each frame: poll RTP access units → feed decoder → present shared handle → DComp swap chain; `ViewerCongestionController` emits REMB from native stats.
7. `VideoRect.geometry-changed` callback keeps the DComp visual in sync with the Slint card layout.

### Teardown

Host: signal `StreamSession::stop_and_wait` so the manager drains sinks before peer teardown. Viewer: stop pipeline, release GPU resources, leave SFU/P2P session.

---

## 12. Telemetry and Diagnostics

### Host-side (per second)

`Stream manager diag`: `video_in_hz`, `audio_in_hz`, `audio_out_hz`, `coalesced_hz`, `recovery_mode`, `keyframe_req_*_total`, `send_fail_video_delta`, `send_fail_audio_delta`, queue lengths/max, `bitrate_kbps`.

`Stream RTP pacing`: `target_kbps`, `out_kbps`, `tx_bytes_total` (from `PacketSink::pacing_telemetry`).

`host_probe_tick` (stream-host tool): `tx_aus_sent`, `tx_aus_dropped`, `tx_rtp_packets`, `tx_rtp_bytes`, `tx_pacing_target_kbps`, `tx_pli`, `tx_remb`, `video_open`, `control_open`, `rtt_ms`.

Encoder periodic (every 300 frames): `convert_ms`, `encode_ms`, `eq_depth`, `eq_drops`.

### Viewer-side (per second)

`viewer_probe_tick` / client tick: `dec_fps`, `native_fps`, `present_hz`, `au_received_hz`, `au_fed_hz`, `decode_queue_depth`, `decode_stall_ms`, `rtt_ms`, `rx_ingress_pps`, `rx_ingress_kbps`, `rx_missing_hz`, `rx_repaired_hz`, `rx_nacks_hz`, `rx_pli_hz`, `rx_jitter`, `rx_gated`, `rx_receive_target_bps`.

`viewer_probe_native_rtp`: `rx_complete`, `rx_emitted`, `rx_incomplete`, `gate_dropped`, `buffered_aus`, `receive_target_bps`.

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
| `tools/sfu-stream-viewer-probe` | Standalone viewer; minifb window or `--native-metrics` headless RTP telemetry |
| `scripts/run-stream-host.ps1` | Launch host probe (release, Nakama `start_stream`) |
| `scripts/run-stream-viewer.ps1` | Launch viewer probe; `-NativeMetrics` for headless soak |
| `scripts/coalesce_stream_timeline.py` | Merges host + viewer + SFU logs into a single timeline |

---

## 13. Key Files

| Area | Path |
|------|------|
| **Rust stream module** | `mello-core/src/stream/` |
| Stream manager | `mello-core/src/stream/manager.rs` |
| PacketSink trait | `mello-core/src/stream/sink.rs` |
| SFU sink (RTP) | `mello-core/src/stream/sink_sfu.rs` |
| P2P fan-out sink (RTP) | `mello-core/src/stream/sink_p2p.rs` |
| RTP peer FFI | `mello-core/src/stream/rtp_peer.rs` |
| Viewer congestion (REMB) | `mello-core/src/stream/congestion.rs` |
| Quality presets + config | `mello-core/src/stream/config.rs` |
| Pacing telemetry | `mello-core/src/stream/pacer.rs` |
| Host session setup | `mello-core/src/stream/host.rs` |
| Viewer tick loop | `mello-core/src/client/streaming.rs` |
| Viewer AU poll + REMB | `mello-core/src/client/stream_ffi.rs` |
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

**What works well:** Process-aware capture with hot-swap, deferred start, DXGI adaptive throttle, GPU preprocessing, async NVENC, mutex-guarded decoded ring, proper IDR detection, H.264 RTP transport (P2P + SFU), REMB congestion control, native RTCP recovery, SFU control-only DataChannel, probe tooling with native RTP telemetry, jitter buffer, DComp rendering with NT shared handle import (Windows RGBA8 path), callback-driven geometry sync.

**Known gaps and future work:**

| Gap | Impact | Effort |
|-----|--------|--------|
| ~~WGC has no frame throttling~~ **Fixed (v0.4)** — accumulator throttle delivers exactly target_fps | — | — |
| AMF/QSV encoders less tested | No smooth experience for AMD/Intel GPU users | Medium |
| Viewer jitter buffer is simple depth-gate, not PID-paced | Residual cadence oscillation under varying network conditions | Medium |
| ~~Game audio not wired~~ **Fixed** — system audio capture (WASAPI loopback / ScreenCaptureKit) → Opus → SFU relay → viewer playout | — | — |
| Input passthrough not implemented | No remote control | Large |
| DComp visual uses overlay, not true underlay (`WS_EX_NOREDIRECTIONBITMAP` not set) | Video composites on top of Slint content; stream card badges moved to bottom bar as workaround | Medium |
| Adapter/device mismatch diagnostics are log-based only | Better in-UI error reasons still needed | Small |
| End-to-end certification matrix pending | Performance targets not yet certified across LAN/loss/multi-viewer gates | Medium |
| macOS viewer has no DComp equivalent | macOS needs its own compositor path (Core Animation layer) | Medium |
| macOS VideoToolbox session churn | Decode FPS drops on SPS/PPS change | Small |

---

## 15. Validation Playbook (Windows)

Use this checklist to validate the DComp rendering path after stream-related changes.

### 15.1 Pre-conditions

- Host and viewer run in release mode.
- Test both 720p60 and 1080p60 scenarios.
- Keep game scene representative (motion + static UI content).

### 15.2 Run commands

Host:

- `./scripts/run-stream-host.ps1 -CrewId "<crew-id>"`

Viewer:

- `./scripts/run-stream-viewer.ps1 -Session auto` (reads the latest session from the host log)

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
