# Debug & Telemetry Specification

> **Component:** libmello (C++) · mello-core (Rust) · mello-client (Rust)
> **Status:** v0.2 Target
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md), [12-STREAMING.md](./12-STREAMING.md)

---

## 1. Overview

This spec covers the debug logging and runtime telemetry infrastructure for Mello. The goals are:

- A unified log stream across the C++ (libmello) and Rust (mello-core, mello-client) layers
- Structured, filterable console output useful during development
- A real-time stats dialog in the UI for quick-glancing pipeline health during development and testing

This is a developer-facing feature. The stats dialog is not intended as an end-user feature for v0.2, though nothing prevents it from being surfaced to users later.

---

## 2. Logging Architecture

### 2.1 Two Layers, One Stream

mello-core uses the `tracing` crate for all structured logging. libmello emits logs via a C callback registered at init time. mello-core registers itself as that callback at startup, forwarding all libmello log output into the `tracing` subscriber. The result is a single unified log stream in the console, with consistent formatting and filtering.

The libmello callback contract is already part of the existing `mello_init()` infrastructure. The bridge in mello-core looks roughly like:

> Register a callback with libmello that maps libmello log levels to `tracing` levels (ERROR→error, WARN→warn, INFO→info, DEBUG→debug) and emits the message with a `target` of `"libmello"` so it can be filtered independently from mello-core output.

### 2.2 Log Filtering

Use standard `RUST_LOG` environment variable filtering via `tracing-subscriber`. Suggested defaults for development:

| Scenario | `RUST_LOG` value |
|---|---|
| Normal development | `info` |
| Debugging stream pipeline | `info,libmello=debug,mello_core::stream=debug` |
| Debugging network/FEC | `info,mello_core::stream::fec=debug,mello_core::stream::sink=debug` |
| Full firehose | `debug` |

### 2.3 Log Format

All log lines should include timestamp, level, target module, and message. Example:

```
2026-03-13T14:22:01.443Z  INFO libmello: [video/device] D3D11 device created: adapter="NVIDIA GeForce RTX 4070" vram=8176MB
2026-03-13T14:22:01.891Z  INFO mello_core::stream: Stream session started session_id=abc123 mode=p2p viewers_max=5
2026-03-13T14:22:02.103Z DEBUG mello_core::stream::fec: FEC group complete n=5 parity_bytes=1240
```

---

## 3. mello-core Logging Guidelines

These describe what to log and when in the Rust layer. Use `tracing` macros (`info!`, `warn!`, `debug!`, `error!`) with structured fields where the value is variable — not string interpolation.

### 3.1 Topology Selection

**Level:** `INFO`

Log when the `start_stream` RPC response is received and the sink type is determined. This is the most important single log line for understanding session configuration.

```
mode=p2p max_viewers=5 session_id=abc123
mode=sfu endpoint=wss://sfu.mello.app/eu-west/abc123 session_id=def456
```

If `SfuSink::new()` fails, log at `ERROR` with the reason.

### 3.2 Viewer Join and Leave

**Level:** `INFO`

Log every viewer connection and disconnection with their ID and, where known, whether they are connecting via TURN relay or direct P2P.

```
viewer_joined viewer_id=xyz789 transport=direct viewers_now=2
viewer_left viewer_id=xyz789 reason=disconnect viewers_now=1
viewer_rejected viewer_id=uvw000 reason=viewer_limit_reached
```

### 3.3 Stream Manager Lifecycle

**Level:** `INFO` for start/stop, `DEBUG` for the poll loop internals.

Log when the stream manager starts and stops its run loop, and include the final session summary on stop.

```
stream_manager_started session_id=abc123 encoder=NVENC codec=H264
stream_manager_stopped session_id=abc123 uptime_secs=142 video_packets=8542 audio_packets=8498
```

### 3.4 ABR Decisions

**Level:** `INFO` — these are infrequent and always worth knowing about.

Log every bitrate change decision with the triggering viewer, the loss rate that caused it, and the old and new bitrate.

```
abr_step_down viewer_id=xyz789 loss_pct=7.2 bitrate_kbps=12000 → 9000
abr_step_up   viewer_id=xyz789 loss_pct=0.4 bitrate_kbps=9000 → 9900
```

### 3.5 FEC and Loss Recovery

**Level:** `DEBUG` for FEC group completions, `WARN` for unrecoverable groups and IDR requests.

FEC group completions are high-frequency — only log at DEBUG. Unrecoverable loss and IDR requests are important events worth flagging.

```
// DEBUG — hot path, filtered out at INFO
fec_group_complete n=5 seq_start=1200 parity_bytes=1380

// WARN — always visible
fec_unrecoverable seq_start=1400 lost=2 of n=5 viewer_id=xyz789
idr_requested viewer_id=xyz789 reason=sustained_loss rate_limited=false
idr_rate_limited viewer_id=xyz789 last_idr_ms_ago=800 (min=2000)
```

### 3.6 Packet Flow (Periodic)

**Level:** `DEBUG`

Log a rolling summary every 300 video packets (~5 seconds at 60fps). Do not log individual packet sends — too noisy.

```
// host side
packet_stats interval=300 video_bytes=24.8MB audio_bytes=0.9MB fec_groups=60 viewers=3

// viewer side  
packet_stats interval=300 received=298 lost_pre_fec=4 lost_post_fec=1 idr_requests=0
```

### 3.7 Voice Session

**Level:** `INFO` for session start/stop and peer connections, `DEBUG` for VAD state changes and periodic stats, `WARN` for packet loss spikes.

Log when a voice session starts with the selected mic device and Opus config. Log each peer connection with their transport type. VAD state changes are `DEBUG` — too frequent for `INFO`.

```
voice_session_started mic="Blue Yeti" opus_bitrate=64kbps rnnoise=true
voice_peer_connected peer_id=abc123 transport=direct
voice_peer_connected peer_id=xyz789 transport=relay(TURN)
voice_peer_disconnected peer_id=abc123 reason=left_crew

// DEBUG — VAD, high frequency
vad_speaking peer_id=self
vad_silent peer_id=self

// DEBUG — periodic, every 300 audio frames (~6 seconds at 50fps Opus)
voice_stats peer=abc123 loss=0.3% jitter=8ms
voice_stats peer=xyz789 loss=4.1% jitter=22ms

// WARN — loss spike worth flagging immediately
voice_loss_spike peer_id=xyz789 loss_pct=18.4
```

### 3.8 Nakama Backend

**Level:** `INFO` for connect/disconnect, `WARN` for reconnects, `DEBUG` for ping.

```
nakama_connected server=api.mello.app
nakama_disconnected reason=network_error
nakama_reconnecting attempt=1
nakama_reconnected attempt=1 downtime_ms=1240

// DEBUG — sampled periodically, not every ping
nakama_ping rtt_ms=24
```

### 3.9 Signaling and ICE

**Level:** `INFO` for connection state transitions, `DEBUG` for individual ICE candidates.

```
peer_connecting viewer_id=xyz789
peer_ice_gathering viewer_id=xyz789
peer_connected viewer_id=xyz789
peer_failed viewer_id=xyz789 reason=ice_timeout
```

---

## 4. Stats Data Model

The stats dialog is driven by a `MelloStats` struct populated by mello-core and passed to the UI on a 1-second refresh tick.

```rust
// mello-core/src/stats.rs

/// Full stats snapshot. Refreshed every second by mello-core.
/// Passed to mello-client via the existing event callback mechanism.
#[derive(Debug, Clone, Default)]
pub struct MelloStats {
    pub host:    Option<HostStats>,    // Some when locally hosting a stream
    pub viewer:  Option<ViewerStats>,  // Some when watching a stream
    pub voice:   Option<VoiceStats>,   // Some when in a voice session
    pub backend: BackendStats,         // Always populated
    pub system:  SystemStats,          // Always populated
}

#[derive(Debug, Clone)]
pub struct HostStats {
    // Encoder
    pub encoder_name:   String,         // "NVENC", "AMF", "QSV-oneVPL", "x264"
    pub codec:          String,         // "H264", "AV1"
    pub resolution:     (u32, u32),     // e.g. (1920, 1080)
    pub fps_target:     u32,
    pub fps_actual:     f32,
    pub bitrate_target: u32,            // kbps
    pub bitrate_actual: u32,            // kbps (last measured)
    pub keyframes_sent: u32,
    pub last_keyframe_reason: String,   // "scheduled", "viewer_joined", "loss_recovery"

    // Capture
    pub capture_backend: String,        // "DXGI-DDI", "WGC"
    pub capture_source:  String,        // e.g. "Minecraft (pid 18432)", "Monitor 0"

    // Per-viewer (P2P mode only; empty slice in SFU mode)
    pub viewers: Vec<ViewerPeerStats>,

    // Topology
    pub topology: String,               // "p2p" or "sfu"
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub struct ViewerPeerStats {
    pub viewer_id:    String,
    pub loss_pct:     f32,              // Packet loss % after FEC
    pub bytes_sent:   u64,
    pub transport:    String,           // "direct" or "relay (TURN)"
    pub bitrate_kbps: u32,             // Current per-viewer target bitrate
}

#[derive(Debug, Clone)]
pub struct ViewerStats {
    // Decoder
    pub decoder_name:      String,      // "NVDEC", "AMF-Decode", "D3D11VA", "SW-FFmpeg"
    pub codec:             String,
    pub resolution:        (u32, u32),
    pub fps_actual:        f32,
    pub frames_decoded:    u64,
    pub frames_dropped:    u64,

    // Network (from viewer's perspective)
    pub loss_pct_pre_fec:  f32,         // Loss before FEC recovery
    pub loss_pct_post_fec: f32,         // Loss after FEC recovery — delta shows FEC value
    pub fec_recoveries:    u32,         // Packets silently recovered by FEC
    pub fec_unrecoverable: u32,         // Groups FEC could not save
    pub idr_requests:      u32,         // Keyframe requests sent to host

    // Performance
    pub staging_copy_avg_ms: f32,       // Average VRAM→CPU copy time
    pub latency_estimate_ms: Option<f32>, // None if clock delta unknown (cross-machine)

    // Source
    pub host_id: String,
    pub topology: String,
}

#[derive(Debug, Clone)]
pub struct VoiceStats {
    // Own state
    pub mic_device:      String,        // Active input device name
    pub opus_bitrate:    u32,           // kbps
    pub sample_rate:     u32,           // Hz (always 48000)
    pub rnnoise_active:  bool,
    pub is_speaking:     bool,          // Current VAD state
    pub is_muted:        bool,
    pub is_deafened:     bool,

    // Per-peer (flat list — one entry per connected voice peer)
    pub peers: Vec<VoicePeerStats>,
}

#[derive(Debug, Clone)]
pub struct VoicePeerStats {
    pub peer_id:       String,
    pub display_name:  String,
    pub is_speaking:   bool,
    pub is_muted:      bool,
    pub loss_pct:      f32,            // Packet loss % on this leg
    pub jitter_buf_ms: f32,            // Current jitter buffer depth
    pub transport:     String,         // "direct" or "relay (TURN)"
}

#[derive(Debug, Clone, Default)]
pub struct BackendStats {
    // Nakama connection
    pub nakama_connected:   bool,
    pub nakama_ping_ms:     u32,        // Last WebSocket round-trip
    pub nakama_reconnects:  u32,        // Total reconnects this session

    // Memory (process-level totals — easy to obtain, no per-subsystem breakdown)
    pub process_rss_mb:     u32,        // Total resident set size
    pub stream_buffer_mb:   u32,        // Decode buffer + staging texture
}

#[derive(Debug, Clone)]
pub struct SystemStats {
    pub gpu_adapter:        String,     // "NVIDIA GeForce RTX 4070"
    pub vram_mb:            u32,
    pub available_encoders: Vec<String>, // All probed, in priority order
    pub available_decoders: Vec<String>,
    pub active_encoder:     String,
    pub active_decoder:     String,
    pub software_encoding:  bool,       // true if x264 is active (show warning)
}
```

---

## 5. Stats Dialog (UI)

### 5.1 Access

Toggled by `Ctrl+Shift+D` from anywhere in the app. Opens as a floating overlay panel in the top-right corner of the stream view — does not interrupt or cover the main UI controls.

### 5.2 Refresh Rate

Stats refresh every **1 second**. mello-core emits a `StatsUpdated(MelloStats)` event on the existing event callback, and the client re-renders the dialog on receipt.

### 5.3 Layout

The dialog shows only sections relevant to the current session state. `BackendStats` and `SystemStats` are always shown. Sections are rendered in order: HOST → VIEWER → VOICE → BACKEND → SYSTEM.

```
┌──────────────────────────────────────────┐
│  🎮 HOST                                 │
│  Encoder   NVENC · H.264 · 1080p60       │
│  Bitrate   11,840 / 12,000 kbps          │
│  FPS       59.8                          │
│  Capture   WGC · Minecraft (pid 18432)   │
│  Keyframes 4  (last: loss_recovery)      │
│  Topology  P2P                           │
│                                          │
│  Viewers                                 │
│  abc123   loss 0.3%  11840kbps  direct   │
│  xyz789   loss 6.1%   9000kbps  relay    │
├──────────────────────────────────────────┤
│  👁 VIEWER                               │
│  Decoder   NVDEC · H.264 · 1080p60       │
│  FPS       59.6   Dropped  12            │
│  Loss      pre-FEC 3.2%  post-FEC 0.1%  │
│  FEC saves 18   Unrecoverable  1         │
│  IDR reqs  2                             │
│  Stage copy  0.4ms avg                   │
│  Latency   ~38ms                         │
├──────────────────────────────────────────┤
│  🎙 VOICE                                │
│  Mic     Blue Yeti · Opus 64kbps         │
│  State   speaking  muted=no  deaf=no     │
│  NS      RNNoise · adaptive gate active  │
│                                          │
│  Peers                                   │
│  peer_id   name     spk  loss  jit  tpt  │
│  abc123    Alice     ●   0.2%  8ms  direct│
│  xyz789    Bob       ○   4.1%  22ms relay│
│  uvw000    Charlie   ○   0.8%  11ms direct│
├──────────────────────────────────────────┤
│  🔗 BACKEND                              │
│  Nakama   connected  ping 24ms  recon 0  │
│  Memory   RSS 74MB   stream buf 18MB     │
├──────────────────────────────────────────┤
│  ⚙ SYSTEM                               │
│  GPU   NVIDIA GeForce RTX 4070  8176MB   │
│  Enc   NVENC ✓  AMF –  QSV –  x264 ✓   │
│  Dec   NVDEC ✓  AMF –  D3D11VA ✓  SW ✓ │
└──────────────────────────────────────────┘
```

**Colour coding** (minimal, non-distracting):
- Loss values: green <1%, yellow 1–5%, red >5%
- FPS: green ≥58, yellow 45–57, red <45
- Jitter buffer: green <15ms, yellow 15–40ms, red >40ms
- Nakama ping: green <50ms, yellow 50–150ms, red >150ms; red bg if disconnected
- Memory RSS: yellow if >150MB, red if >250MB (against the <100MB viewer target)
- Software encoding warning: yellow banner across the HOST section if `software_encoding=true`
- Latency: green <40ms, yellow 40–80ms, red >80ms
- Speaking indicator (●/○): green dot when VAD active, grey when silent

### 5.4 Latency Estimate

The viewer displays an estimated end-to-end latency derived from the `timestamp_us` field in the packet header (the host's capture timestamp) minus the viewer's current time. This is only meaningful when host and viewer are on the same machine or LAN with NTP sync. When the clock delta appears unreliable (negative or >500ms), display `~` prefix and a muted colour to signal it is approximate.

---

## 6. What Is Not In Scope

- Persistent telemetry / analytics — no data is written to disk or sent to any server in v0.2
- Performance counters exported to external tools (Windows Performance Monitor, etc.)
- Crash reporting
- Remote diagnostics

These can be revisited post-beta once the pipeline is stable.
