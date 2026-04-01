# Streaming Implementation Specification

> **Component:** libmello (C++) · mello-core (Rust) · mello-client (Rust) · Backend (Go)
> **Status:** v0.2 Target
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [03-LIBMELLO.md](./03-LIBMELLO.md), [02-MELLO-CORE.md](./02-MELLO-CORE.md), [04-BACKEND.md](./04-BACKEND.md)

---

## 1. Overview

Streaming is Mello's core differentiator. The goal is **Parsec-parity quality**: 1080p60, sub-60ms WAN latency, hardware-accelerated encode/decode, and graceful degradation under network stress — favouring visible quality loss over lag.

This spec covers the **mello-core orchestration layer**: transport framing and FEC, loss recovery, quality presets and adaptive bitrate, multi-viewer fan-out, Stream Manager, topology selection (P2P vs. SFU), and the input passthrough stub.

Video capture, GPU color conversion, hardware encode/decode, and the Slint frame handoff are specified in **[14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md)**.

---

## 2. Design Principles

These principles govern every tradeoff in this spec:

| Principle | Implication |
|---|---|
| **Latency over quality** | Drop frames, lower bitrate, or degrade resolution before buffering |
| **No B-frames** | Encode profile must be latency-optimised (Baseline/Main, no B-frames, minimal VBV buffer) |
| **Rate-limit recovery events** | IDR requests capped to avoid compounding congestion |
| **Topology-agnostic core** | Stream Manager sends packets to a `PacketSink` — it does not know or care about P2P vs. SFU |
| **Server decides topology** | The client never assumes; the backend's `start_stream` response determines the sink type |

---

## 3. Codec Stack

Video codec configuration, encoder/decoder selection and fallback, and all associated libmello C API types (`MelloCodec`, `MelloEncoderBackend`, `MelloDecoderBackend`, `mello_get_encoders`, `mello_get_decoders`) are fully specified in **[14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md)**.

Summary for context:
- **Primary:** H.264, low-latency profile (no B-frames, CBR, 1-second VBV)
- **Stretch goal:** AV1 — activated only when both host and viewer confirm hardware support
- **Encoder priority:** NVENC → AMF → QSV (oneVPL) → x264 (software, capped 720p30)
- **Decoder priority:** NVDEC → AMF → D3D11VA → FFmpeg SW
- When x264 is active, mello-core surfaces a UI warning via `mello_encoder_is_software()`

### 3.4 Audio Codec

Opus at **128 kbps, stereo, 48 kHz**. Stream audio uses a separate Opus encoder instance from the voice encoder, at a higher bitrate. No new API required.

---


## 4. Stream Audio

### 4.1 Behaviour

Game audio is **always captured and always sent** whenever a stream is active. There is no opt-out toggle in v0.2. (A mute-game-audio control can be added as a UI affordance later without spec changes.)

### 4.2 Capture: WASAPI Loopback

Game audio is captured via the WASAPI loopback interface — the same WASAPI stack already in libmello, but pointed at the render endpoint (what the host's speakers are playing) rather than the capture endpoint (microphone).

```c
// New API call — initialises a loopback capture session
MelloResult mello_stream_start_audio(
    MelloStreamHost* host
);

void mello_stream_stop_audio(MelloStreamHost* host);

// Returns encoded Opus packet. Same polling pattern as mello_stream_get_video_packet.
int mello_stream_get_audio_packet(
    MelloStreamHost* host,
    uint8_t*         buffer,
    int              buffer_size
);
```

Internally this reuses `WasapiCapture` with the loopback flag set:

```cpp
// src/audio/capture_wasapi.cpp
// Existing capture_thread() gains a loopback mode:
bool WasapiCapture::initialize_loopback() {
    // Use eRender + eConsole endpoint instead of eCapture
    // IAudioClient::Initialize with AUDCLNT_STREAMFLAGS_LOOPBACK
    ...
}
```

### 4.3 Mixing

Mic audio and game audio are **separate streams** — they are not mixed before sending. The viewer receives and plays them independently via two Opus decode instances. This keeps them separable for future features (e.g. independent volume sliders, viewer-side muting of game audio while keeping voice).

### 4.4 Viewer Side

```c
// Feed game audio packet (separate from voice packet)
MelloResult mello_stream_feed_audio_packet(
    MelloStreamView* view,
    const uint8_t*   data,
    int              size
);
```

Playback goes through the existing WASAPI playback path. The viewer's OS audio mixer handles final output.

---

## 5. Transport Framing

### 5.1 DataChannel Configuration

All stream data flows over **unreliable, unordered DataChannels**. This is non-negotiable — reliable/ordered channels introduce head-of-line blocking which destroys latency. Loss is handled by FEC and IDR recovery (see §6).

One DataChannel per viewer connection. Voice continues to use its own DataChannel as today.

### 5.2 Packet Format

Every packet sent over the stream DataChannel uses the following binary header:

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
├─────────────────────────────────────────────────────────────────┤
│  type (1B) │  flags (1B) │        sequence (2B)                 │
├─────────────────────────────────────────────────────────────────┤
│                     timestamp_us (8B)                           │
├─────────────────────────────────────────────────────────────────┤
│                     payload (variable)                          │
└─────────────────────────────────────────────────────────────────┘

type:
  0x01 = video
  0x02 = audio
  0x03 = fec (parity)
  0x04 = control (keyframe request, quality change)

flags (bitfield):
  bit 0 = is_keyframe
  bit 1 = fec_group_last  (last data packet in FEC group; parity follows)
  bit 2 = codec_av1       (0 = H.264, 1 = AV1; set on every video packet)

sequence: monotonically increasing per type (video seq, audio seq tracked separately)
timestamp_us: capture timestamp in microseconds (host clock)
```

This header is 12 bytes. Parsing is done in mello-core (Rust), not libmello.

### 5.3 Forward Error Correction (XOR FEC)

FEC operates over groups of video packets. For every N data packets, one parity packet is sent. The parity is the XOR of all N payloads (after stripping the 12-byte header; the header of the parity packet carries its own sequence and the `type = 0x03` marker).

FEC overhead is **adaptive** — the ABR controller adjusts `fec_n` dynamically based on viewer loss reports (see §7.2):
- Loss < 1%: FEC disabled (0% overhead)
- Loss 1–5%: N = 10 (10% overhead)
- Loss > 5%: N = 5 (20% overhead)

Streams start with FEC disabled. This recovers any single packet loss within a group silently, with zero added latency, and avoids wasting bandwidth on healthy connections.

FEC group formation:

```
Packets:  [V1] [V2] [V3] [V4] [V5*] [FEC]  [V6] [V7] ...
                                     parity
* fec_group_last flag set on V5
```

The FEC group boundary resets on every keyframe (IDR). This ensures a keyframe always starts a fresh group — partial groups from before the IDR are discarded by the viewer.

FEC does **not** apply to audio packets. Opus has built-in PLC (Packet Loss Concealment) that handles audio loss gracefully.

FEC group size is managed by the ABR controller at runtime. The `FecEncoder` supports `set_group_size(n)` for live adjustment — setting n < 2 disables FEC entirely.

### 5.4 DataChannel Message Chunking

Unreliable DataChannels use SCTP under the hood. When a message exceeds the SCTP MTU (~1200 bytes), SCTP fragments it internally. In unreliable mode, losing **any single SCTP fragment** drops the **entire application-level message**. A single H.264 keyframe can be 200–400 KB — SCTP splits that into hundreds of fragments, making loss near-certain even on good networks.

To mitigate this, the host performs **application-level chunking** before handing data to the DataChannel. Each serialized `StreamPacket` is split into chunks of at most **60,000 bytes** (well under the 64 KB SCTP message size limit). Each chunk carries a 6-byte header:

```
 0       2       4       6
├───────┼───────┼───────┤
│msg_id │chunk_i│chunk_n│  payload (≤60,000 bytes)
│ (u16) │ (u16) │ (u16) │
└───────┴───────┴───────┘

msg_id:   monotonically increasing message counter (wraps at u16::MAX)
chunk_i:  0-based index of this chunk within the message
chunk_n:  total number of chunks in this message
```

A single-chunk message (payload ≤ 60 KB) still carries the header with `chunk_i=0, chunk_n=1`.

**Viewer reassembly:** The viewer maintains a `ChunkAssembler` that collects incoming chunks keyed by `msg_id`. When all `chunk_n` chunks for a `msg_id` arrive, the original `StreamPacket` payload is reconstructed and fed to `StreamViewer`. Incomplete assemblies are evicted after newer message IDs arrive (stale threshold: 256 msg_ids behind).

This chunking layer sits **between** the StreamPacket serialization and the DataChannel send — it is transparent to FEC, ABR, and all higher-level logic.

```
Host:  StreamPacket → serialize → chunk (60KB) → DataChannel send
Viewer: DataChannel recv → reassemble chunks → deserialize → StreamViewer
```

#### Rust implementation

```
mello-core/src/stream/sink_p2p.rs   — chunking (host side)
mello-core/src/client.rs            — ChunkAssembler (viewer side)
```

#### Rust implementation location (FEC)

```
mello-core/src/stream/fec.rs
```

```rust
pub struct FecEncoder {
    n: usize,              // group size
    group: Vec<Vec<u8>>,   // accumulated data packets
}

impl FecEncoder {
    pub fn new(n: usize) -> Self { ... }

    /// Push a data packet. Returns Some(parity_payload) when group is complete.
    pub fn push(&mut self, payload: &[u8]) -> Option<Vec<u8>> { ... }

    pub fn reset(&mut self) { self.group.clear(); }
}

pub struct FecDecoder {
    n: usize,
    group: HashMap<u16, Vec<u8>>,  // seq -> payload
    parity: Option<Vec<u8>>,
}

impl FecDecoder {
    /// Feed a received packet (data or parity).
    /// Returns recovered payload if a loss was just repaired.
    pub fn feed(&mut self, seq: u16, ptype: PacketType, payload: &[u8]) -> Option<Vec<u8>> { ... }
}
```

---

## 6. Loss Recovery

Three-tier strategy, in order of preference:

### Tier 1 — FEC (silent, zero latency)

Any single packet loss within a FEC group is recovered by XOR parity. No round-trip, no visible artefact.

### Tier 2 — Drop and continue

If a FEC group loses 2+ packets (unrecoverable), the viewer simply skips those packets. The decoder will produce artefacted or skipped frames. This is acceptable — **visible artefacts are preferred over stalling**.

### Tier 3 — Rate-limited IDR request

If the viewer detects 2 consecutive unrecoverable FEC groups (i.e. sustained loss), it requests a keyframe from the host. IDR requests are **rate-limited to one per 2 seconds** — on a bad network, hammering the host with IDR requests would make things worse, not better. The IDR request is a control packet (type `0x04`).

On the host side, `mello_stream_request_keyframe()` is called when the control packet is received. The next encoded frame will be an IDR.

There is no retransmission (ARQ/NACK). The round-trip cost at any non-trivial network latency exceeds the latency budget.

---

## 7. Quality Presets and Adaptive Bitrate

### 7.1 Presets

Bitrate targets are tuned for Discord-competitive bandwidth. The host encodes at the preset's target resolution, **not** the native capture resolution — the pipeline downscales via the GPU video preprocessor when capture exceeds the target (see 14-VIDEO-PIPELINE.md §5.5).

| Preset | Resolution | FPS | Bitrate (H.264) | Bitrate (AV1) | Est. Total (+ FEC) |
|---|---|---|---|---|---|
| **Ultra** | 1080p | 60 | 8 Mbps | 5 Mbps | ~8–10 Mbps |
| **High** | 1080p | 60 | 6 Mbps | 4 Mbps | ~6–7 Mbps |
| **Medium** | 1080p | 30 | 4 Mbps | 2.5 Mbps | ~4–5 Mbps |
| **Low** | 720p | 30 | 2.5 Mbps | 1.5 Mbps | ~2.5–3 Mbps |
| **Potato** | 720p | 30 | 1.5 Mbps | — | ~1.5–2 Mbps |

**Default preset:** Medium (1080p30 @ 4 Mbps H.264). Suitable for most internet connections. Power users can select High/Ultra from the stream source picker UI.

Preset selection is exposed as a UI control — the host can override it manually before starting the stream.

### 7.2 Adaptive Bitrate (ABR)

ABR is host-driven. The host monitors per-viewer packet loss acknowledgements (via periodic control packets sent by viewers, see below) and adjusts bitrate dynamically.

ABR rules:
- **Step down:** if a viewer reports >5% packet loss (after FEC), reduce bitrate by 25% within 1 second
- **Step up:** if all viewers report <1% loss for 10 consecutive seconds, increase bitrate by 10%
- **Minimum bitrate:** never go below the Potato preset values
- **Bitrate changes** are applied via `mello_stream_set_bitrate()` (hot-reconfigures the encoder without restarting the session)
- **Adaptive FEC:** the ABR controller also adjusts `fec_n` based on loss ratio (see §5.3). FEC changes are applied to the `FecEncoder` via `set_group_size()` alongside bitrate changes.

ABR operates **independently per viewer** when in P2P mode — the host can send different bitrates to different viewers. In SFU mode, the SFU handles per-viewer adaptation; the host sends a single stream at its chosen quality and the SFU is responsible for transcoding tiers (future spec).

### 7.3 Viewer Loss Report Packet

Sent by viewer to host every 1 second via the control DataChannel:

```
type: 0x04 (control)
payload: {
    u8  subtype = 0x01 (loss_report)
    u16 packets_received
    u16 packets_lost          // after FEC recovery
    u8  reserved
}
```

### 7.4 libmello API

The ABR controller calls `mello_stream_set_bitrate()` and `mello_stream_get_stats()`. Both are defined in **[14-VIDEO-PIPELINE.md §10](./14-VIDEO-PIPELINE.md)**. `MelloStreamStats` includes `encoder_name` so mello-core can surface the software encoding warning when x264 is active.

---

## 8. Multi-Viewer Fan-out and Stream Manager

### 8.1 PacketSink trait

The Stream Manager in mello-core sends packets to a `PacketSink`. This trait is the only abstraction needed between the stream pipeline and the transport topology.

```rust
// mello-core/src/stream/sink.rs

#[async_trait]
pub trait PacketSink: Send + Sync {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError>;
    async fn send_audio(&self, packet: &StreamPacket) -> Result<(), StreamError>;
    async fn send_control(&self, packet: &StreamPacket) -> Result<(), StreamError>;

    /// Called when a new viewer joins mid-session (triggers keyframe request).
    async fn on_viewer_joined(&self, viewer_id: &str);

    /// Called when a viewer leaves.
    async fn on_viewer_left(&self, viewer_id: &str);
}
```

Two implementations:

```
P2PFanoutSink      — sends to N DataChannel connections (max 5 viewers)
SfuSink            — sends to one SFU WebSocket connection (protocol in 13-SFU.md)
```

### 8.2 P2PFanoutSink

```rust
// mello-core/src/stream/sink_p2p.rs

pub struct P2PFanoutSink {
    viewers: RwLock<HashMap<String, Arc<DataChannel>>>,
    max_viewers: usize,  // = 5
}

impl P2PFanoutSink {
    pub fn new() -> Self { ... }

    pub fn add_viewer(&self, viewer_id: String, channel: Arc<DataChannel>) -> Result<(), StreamError> {
        let mut viewers = self.viewers.write().unwrap();
        if viewers.len() >= self.max_viewers {
            return Err(StreamError::ViewerLimitReached);
        }
        viewers.insert(viewer_id, channel);
        Ok(())
    }

    pub fn remove_viewer(&self, viewer_id: &str) { ... }
}

#[async_trait]
impl PacketSink for P2PFanoutSink {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        let viewers = self.viewers.read().unwrap();
        for channel in viewers.values() {
            // Best-effort — individual send failures do not abort fan-out
            let _ = channel.send_unreliable(packet.as_bytes()).await;
        }
        Ok(())
    }
    // ... send_audio, send_control mirror send_video
}
```

Fan-out is fire-and-forget per viewer. A slow or disconnected viewer does not stall the pipeline for other viewers.

### 8.3 Stream Manager

```rust
// mello-core/src/stream/manager.rs

pub struct StreamManager {
    lib: Arc<LibMello>,
    sink: Arc<dyn PacketSink>,
    fec_encoder: FecEncoder,
    video_seq: AtomicU16,
    audio_seq: AtomicU16,
    abr: AbrController,
}

impl StreamManager {
    pub fn new(lib: Arc<LibMello>, sink: Arc<dyn PacketSink>, config: StreamConfig) -> Self { ... }

    /// Main loop — called from a dedicated thread after stream start.
    pub async fn run(&mut self, mut stop: tokio::sync::oneshot::Receiver<()>) {
        loop {
            tokio::select! {
                _ = &mut stop => break,
                _ = tokio::time::sleep(Duration::from_millis(1)) => {
                    self.poll_video().await;
                    self.poll_audio().await;
                }
            }
        }
    }

    async fn poll_video(&mut self) {
        let mut buf = [0u8; 65536];
        let mut is_keyframe = false;
        let n = self.lib.stream_get_video_packet(&mut buf, &mut is_keyframe);
        if n <= 0 { return; }

        let seq = self.video_seq.fetch_add(1, Ordering::Relaxed);
        let packet = StreamPacket::video(&buf[..n as usize], seq, is_keyframe);

        if is_keyframe {
            self.fec_encoder.reset();
        }

        // FEC encode — sends parity packet when group completes
        if let Some(parity) = self.fec_encoder.push(packet.payload()) {
            let fec_packet = StreamPacket::fec(&parity, seq);
            let _ = self.sink.send_video(&fec_packet).await;
        }

        let _ = self.sink.send_video(&packet).await;
    }

    async fn poll_audio(&mut self) { ... }  // mirrors poll_video, no FEC
}
```

### 8.4 Viewer Count Enforcement

P2P viewer limit is **5 viewers**. This is enforced in two places:
1. `P2PFanoutSink::add_viewer()` — hard limit at the connection layer
2. `start_stream` RPC on the backend — the server will reject watch requests beyond the limit and return an error to the requesting viewer's client

The "upgrade to premium for more viewers" upsell is triggered by the backend when a 6th viewer attempts to join a P2P stream.

---

## 9. Topology Selection (P2P vs. SFU)

### 9.1 How the Client Learns Its Topology

The client never decides. When the host calls `start_stream`, the backend determines topology based on:
1. Is this a self-hosted Nakama instance? → always P2P
2. Does the host/crew have active premium entitlement? → SFU available

The `start_stream` RPC response carries the topology descriptor:

```go
// nakama/data/modules/streaming.go

type StartStreamRequest struct {
    CrewID string `json:"crew_id"`
    // Codec negotiation hint from host
    SupportsAV1 bool   `json:"supports_av1"`
    // Actual encode resolution (from capture source, see §9.3)
    Width       uint32 `json:"width"`
    Height      uint32 `json:"height"`
}

type StartStreamResponse struct {
    SessionID string `json:"session_id"`
    Mode      string `json:"mode"`       // "p2p" | "sfu"

    // P2P fields (mode == "p2p")
    MaxViewers int `json:"max_viewers,omitempty"` // always 5

    // SFU fields (mode == "sfu")
    SFUEndpoint string `json:"sfu_endpoint,omitempty"` // "wss://sfu.mello.app/..."
    SFUToken    string `json:"sfu_token,omitempty"`
}

func StartStreamRPC(ctx context.Context, ...) (string, error) {
    // 1. Validate auth, validate crew membership
    // 2. Check entitlement (credits system, spec 10-CREDITS-IMPLEMENTATION.md)
    // 3. Build response
    if hasPremium {
        resp = StartStreamResponse{
            SessionID:   newSessionID(),
            Mode:        "sfu",
            SFUEndpoint: sfuEndpointForRegion(userRegion),
            SFUToken:    signSFUToken(userID, sessionID),
        }
    } else {
        resp = StartStreamResponse{
            SessionID:  newSessionID(),
            Mode:       "p2p",
            MaxViewers: 5,
        }
    }
    // 4. Update presence: StreamingTo = crew_id
    // 5. Broadcast "X is streaming" to crew via Nakama notification
}
```

### 9.2 mello-core Sink Instantiation

```rust
// mello-core/src/stream/host.rs

pub async fn start_stream(
    lib: Arc<LibMello>,
    nakama: Arc<NakamaClient>,
    config: StreamConfig,
) -> Result<StreamSession, StreamError> {

    let resp = nakama.rpc("start_stream", &StartStreamRequest::from(&config)).await?;

    let sink: Arc<dyn PacketSink> = match resp.mode.as_str() {
        "p2p"  => Arc::new(P2PFanoutSink::new()),
        "sfu"  => Arc::new(SfuSink::new(&resp.sfu_endpoint, &resp.sfu_token).await?),
        other  => return Err(StreamError::UnknownMode(other.to_string())),
    };

    let manager = StreamManager::new(lib, sink, config);
    Ok(StreamSession { manager, session_id: resp.session_id })
}
```

`SfuSink` wraps an `SfuConnection` (see EXTERNAL-SFU.md) and forwards `StreamPacket` data via the SFU's media DataChannel.

```rust
// mello-core/src/stream/sink_sfu.rs

pub struct SfuSink {
    conn: Arc<SfuConnection>,
}

impl SfuSink {
    pub fn new(conn: Arc<SfuConnection>) -> Self { Self { conn } }
}

impl PacketSink for SfuSink {
    fn send_video(&self, packet: &[u8]) -> Result<(), StreamError> {
        self.conn.send_media(packet)
    }
}
```

**Viewer SFU lifecycle:** When a viewer connects via SFU, the `stream_tick` polls both media packets and SFU signaling events. If the SFU sends `session_ended` (host disconnected) or the WebSocket drops, the viewer automatically tears down: emits `StreamWatchingStopped`, drops the `ViewerState`, and clears the last video frame (`set_stream_frame(Image::default())`). The `StreamEnded` event (host-side) also clears the frame.

### 9.3 Resolution Negotiation

The host encodes at its **native capture resolution** (determined by the capture source — e.g. 2560x1440 for a monitor, or the game window size). This resolution is not known until after the capture pipeline starts, so it cannot be hardcoded or assumed by the viewer.

The resolution is propagated through two paths:

**Path 1 — Nakama storage (for stream discovery UI):**
The host calls `mello_stream_get_host_resolution()` after `mello_stream_start_host()` returns, then includes the actual `width` and `height` in the `start_stream` RPC. The backend stores these in `StreamMeta` and returns them in `CrewState.stream.width/height`. The viewer UI uses these values to display stream info before watching.

**Path 2 — WebRTC signaling (for decoder initialization):**
When the host responds to a viewer's WebRTC Offer with an Answer, it includes `stream_width` and `stream_height` in the `SignalEnvelope`:

```rust
// mello-core/src/voice/mesh.rs

pub struct SignalEnvelope {
    pub purpose: SignalPurpose,
    pub message: SignalMessage,
    /// Host encode resolution — included in Stream Answer so the viewer
    /// can initialize the decoder at the correct size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_height: Option<u32>,
}
```

**Viewer deferred initialization:** The viewer does **not** create its decoder pipeline (`mello_stream_start_viewer`) when it sends the Offer. It waits for the Answer, reads the resolution from the envelope, and only then initializes the decoder at the correct dimensions. The `ViewerState.viewer` handle is `Option<*mut MelloStreamView>` — `None` until the Answer arrives.

This prevents the resolution mismatch that occurs when the viewer assumes 1920x1080 but the host encodes at a different resolution (causes green screen / CUDA errors on NVDEC).

---

## 10. Input Passthrough (Stub)

Input passthrough allows a viewer to send keyboard and mouse events to the host for remote control. This is deferred to a future spec. The interface is defined here so the rest of the system can account for it.

```rust
// mello-core/src/stream/input.rs  (stub)

/// Opaque input event — encoding TBD in input passthrough spec.
pub struct InputEvent {
    pub raw: Vec<u8>,
}

pub trait InputPassthrough: Send + Sync {
    /// Viewer side: send an input event to the host.
    fn send_event(&self, event: InputEvent) -> Result<(), StreamError>;

    /// Host side: register a callback to receive input events from viewers.
    fn on_event(&self, callback: Box<dyn Fn(InputEvent) + Send + Sync>);
}

/// No-op implementation used until the feature is specced and built.
pub struct InputPassthroughStub;

impl InputPassthrough for InputPassthroughStub {
    fn send_event(&self, _: InputEvent) -> Result<(), StreamError> {
        Err(StreamError::NotImplemented)
    }
    fn on_event(&self, _: Box<dyn Fn(InputEvent) + Send + Sync>) {}
}
```

The `StreamManager` holds an `Arc<dyn InputPassthrough>` initialised to `InputPassthroughStub`. When input passthrough is implemented, it replaces the stub without any other changes to the stream pipeline.

---

## 11. Performance Targets

| Metric | Target | Notes |
|---|---|---|
| Stream latency (LAN) | <20ms | Capture → decode → render |
| Stream latency (WAN, 30ms ping) | <60ms | |
| Encoder latency contribution | <8ms | See 14-VIDEO-PIPELINE.md |
| FEC overhead | 0–20% bandwidth | Adaptive: 0% healthy, 10% moderate, 20% high loss |
| IDR frequency (stable network) | ≤1 per 120s | Keyframe interval only |
| IDR frequency (lossy network) | ≤1 per 2s | Rate-limited floor |
| Host CPU overhead (encoding) | <5% | HW encode; SW encode higher |
| Viewer RAM | <100MB | Decode buffer + frame buffer |

---

## 12. Future Work

| Item | Spec |
|---|---|
| SFU wire protocol | 13-SFU.md |
| Input passthrough (keyboard/mouse) | Future spec |
| AV1 codec negotiation handshake | 14-VIDEO-PIPELINE.md (extend when AV1 ships) |
| Per-viewer ABR in SFU mode | 13-SFU.md |
| macOS/Linux platform support | 14-VIDEO-PIPELINE.md (platform backends) |
