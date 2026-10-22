# SFU Integration Specification

> **Component:** mello-core (Rust) · Backend/Nakama (Go)  
> **Status:** Implemented (RTP video + control-only DC on stream sessions)
> **Depends on:** [01-SFU.md](../../mello-sfu/01-SFU.md) (canonical SFU contract), [12-STREAMING.md](./12-STREAMING.md), [13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md)

---

## 1. Overview

This spec covers the integration work needed to connect the existing SFU server to the client and backend. The SFU itself is built and deployed. This spec implements the client-side `SfuSink`, the Nakama RPCs that route traffic to the SFU, and the JWT signing that authenticates clients with the SFU.

### What exists today

| Component | Status |
|---|---|
| SFU server (Go, Pion WebRTC) | Built; stream video = H.264 RTP relay, voice = Opus RTP |
| `PacketSink` trait in mello-core | RTP sinks ([12-STREAMING.md §6](./12-STREAMING.md)) |
| `SfuSink` | Native host RTP AU sender + control/feedback through `SfuConnection` |
| `SfuConnection` | Role-based peers; stream = control DC + RTP video track |
| `start_stream` / `watch_stream` RPCs | Always SFU mode with endpoint + JWT, or `SFU_UNAVAILABLE` |
| `voice_join` RPC | Always SFU mode with endpoint + JWT, or `SFU_UNAVAILABLE` |
| TURN server (coturn) | Deployed on GCP, working |

### Completed integration record

The table below records the completed integration. For current stream behavior
(RTP video, control-only DataChannel, REMB/PLI relay), use
[12-STREAMING.md](../12-STREAMING.md) and `mello-sfu/01-SFU.md`.

| Component | Change |
|---|---|
| `SfuSink` | `send_video_access_unit` over host RTP track; `send_control` on reliable DC |
| `SfuConnection` | Stream peers: one `control` DC + H.264 RTP track (no stream `media` DC) |
| `start_stream` RPC | SFU endpoint + JWT for every crew |
| `watch_stream` RPC | Viewers request their own SFU token |
| `voice_join` RPC | Return SFU endpoint + JWT |
| `VoiceManager` | Joins every voice channel through the SFU |
| JWT signing | Nakama module: sign RS256 tokens for SFU auth |
| SFU routing | Nakama module: region selection |

---

## 2. Backend: New Files

### 2.1 `sfu_auth.go` - JWT Signing

The SFU authenticates clients via short-lived RS256 JWTs signed by Nakama. The SFU verifies these with the corresponding public key. Nakama and the SFU share a key pair, not a shared secret.

**Key pair setup (one-time):**

```bash
# Generate RS256 key pair
openssl genrsa -out sfu_private.pem 2048
openssl rsa -in sfu_private.pem -pubout -out sfu_public.pem

# Private key goes to Nakama (signs tokens)
# Public key goes to SFU server (verifies tokens)
```

On Render.com, store `sfu_private.pem` contents in an env var `SFU_JWT_PRIVATE_KEY`. On the GCP SFU VM, the public key is at the path specified by `SFU_JWT_PUB_KEY`.

```go
// backend/nakama/data/modules/sfu_auth.go

package main

import (
    "crypto/rsa"
    "crypto/x509"
    "encoding/pem"
    "os"
    "time"

    "github.com/golang-jwt/jwt/v5"
)

type SFUTokenClaims struct {
    jwt.RegisteredClaims
    UserID    string `json:"uid"`
    SessionID string `json:"sid"`
    Type      string `json:"type"`     // "stream" or "voice"
    Role      string `json:"role"`     // "host", "viewer", "member"
    CrewID    string `json:"crew_id"`
    ChannelID string `json:"ch_id,omitempty"`
    Region    string `json:"region"`
}

var sfuPrivateKey *rsa.PrivateKey

func initSFUAuth() error {
    keyPEM := os.Getenv("SFU_JWT_PRIVATE_KEY")
    if keyPEM == "" {
        return nil // SFU auth disabled (no key configured)
    }

    block, _ := pem.Decode([]byte(keyPEM))
    if block == nil {
        return fmt.Errorf("sfu_auth: failed to decode PEM")
    }

    key, err := x509.ParsePKCS1PrivateKey(block.Bytes)
    if err != nil {
        // Try PKCS8 format
        k, err2 := x509.ParsePKCS8PrivateKey(block.Bytes)
        if err2 != nil {
            return fmt.Errorf("sfu_auth: parse private key: %v / %v", err, err2)
        }
        var ok bool
        key, ok = k.(*rsa.PrivateKey)
        if !ok {
            return fmt.Errorf("sfu_auth: key is not RSA")
        }
    }

    sfuPrivateKey = key
    return nil
}

func sfuAuthEnabled() bool {
    return sfuPrivateKey != nil
}

func signSFUToken(claims SFUTokenClaims) (string, error) {
    if sfuPrivateKey == nil {
        return "", fmt.Errorf("SFU auth not configured (SFU_JWT_PRIVATE_KEY not set)")
    }

    claims.RegisteredClaims = jwt.RegisteredClaims{
        ExpiresAt: jwt.NewNumericDate(time.Now().Add(5 * time.Minute)),
        IssuedAt:  jwt.NewNumericDate(time.Now()),
        Issuer:    "mello-nakama",
    }

    token := jwt.NewWithClaims(jwt.SigningMethodRS256, claims)
    return token.SignedString(sfuPrivateKey)
}
```

**Registration:** Call `initSFUAuth()` in the Nakama `InitModule` function:

```go
// In main.go InitModule()
if err := initSFUAuth(); err != nil {
    logger.Error("SFU auth init failed: %v", err)
    return err
}
if sfuAuthEnabled() {
    logger.Info("SFU JWT signing enabled")
} else {
    logger.Warn("SFU JWT signing disabled (SFU_JWT_PRIVATE_KEY not set)")
}
```

**Dependency:** `github.com/golang-jwt/jwt/v5`. Add to `go.mod` or vendor.

Note: Nakama Go modules run as compiled plugins. You cannot use `go.mod` in the modules directory directly. Instead, add the `golang-jwt` dependency to the Nakama plugin build. If using the `heroiclabs/nakama-pluginbuilder` Docker image, add the dependency in the build step. Alternatively, implement JWT signing manually (RS256 = RSASSA-PKCS1-v1_5 with SHA-256) to avoid the external dependency:

```go
// Manual RS256 signing (no external dependency)
func signSFUTokenManual(claims SFUTokenClaims) (string, error) {
    header := base64URLEncode([]byte(`{"alg":"RS256","typ":"JWT"}`))
    payload, _ := json.Marshal(claims)
    payloadEnc := base64URLEncode(payload)
    signingInput := header + "." + payloadEnc

    hash := sha256.Sum256([]byte(signingInput))
    sig, err := rsa.SignPKCS1v15(rand.Reader, sfuPrivateKey, crypto.SHA256, hash[:])
    if err != nil {
        return "", err
    }
    return signingInput + "." + base64URLEncode(sig), nil
}

func base64URLEncode(data []byte) string {
    return strings.TrimRight(base64.URLEncoding.EncodeToString(data), "=")
}
```

### 2.2 `sfu_routing.go` - Region Selection and Endpoints

```go
// backend/nakama/data/modules/sfu_routing.go

package main

import "os"

var sfuEndpoints = map[string]string{
    "eu-west": "wss://sfu-eu.m3llo.app/ws",
    "us-east": "wss://sfu-us.m3llo.app/ws",
}

func init() {
    // Allow override via env for dev/staging
    if eu := os.Getenv("SFU_ENDPOINT_EU"); eu != "" {
        sfuEndpoints["eu-west"] = eu
    }
    if us := os.Getenv("SFU_ENDPOINT_US"); us != "" {
        sfuEndpoints["us-east"] = us
    }
}

func selectSFURegion(userRegion string) string {
    // Simple geo-mapping for beta.
    // userRegion: derived from Nakama session context or IP geolocation.
    // For beta, default to eu-west since most testers are EU-based.
    // TODO: Integrate MaxMind GeoLite2 for IP-based region detection.
    switch userRegion {
    case "NA", "SA":
        return "us-east"
    default:
        return "eu-west"
    }
}

func sfuEndpointForRegion(region string) string {
    if ep, ok := sfuEndpoints[region]; ok {
        return ep
    }
    return sfuEndpoints["eu-west"] // fallback
}
```

**Local multi-device note:** when testing desktop + mobile against local Nakama/SFU, `SFU_ENDPOINT_EU` must be a LAN-reachable host (`ws://<lan-ip>:8443/ws`), not `localhost`. The SFU itself must also advertise a non-loopback ICE host candidate (`SFU_PUBLIC_IP=<lan-ip>`), or remote devices will connect signaling but fail media.

### 2.3 `sfu_entitlement.go` - Crew Kill Switch and the Typed Error

The SFU is the only media transport. The file holds:

| Name | What it does |
|---|---|
| `errSFUUnavailable` | The typed error `SFU_UNAVAILABLE` (code 14, gRPC UNAVAILABLE) |
| `sfuEnabledForCrew` | True unless the crew metadata has `"sfu_enabled": false`. The flag is an operator kill switch; the default is on. A crew that cannot be read is on |
| `sfuEnabledInMetadata` | The same rule on the stored metadata JSON. Only an explicit boolean `false` turns the SFU off |
| `sfuAvailableForCrew` | `sfuAuthEnabled() && sfuEnabledForCrew(...)` |

New crews store `"sfu_enabled": true` (`newCrewMetadata` in `crews.go`). To turn the SFU off for one crew, an operator sets `"sfu_enabled": false` in the Nakama console (Group → Edit Metadata).

---

## 3. Backend: Modified Files

### 3.1 `streaming.go` - `start_stream` and `watch_stream`

`start_stream`:

1. Validate the request and the crew membership.
2. If `sfuAvailableForCrew` is false, return `SFU_UNAVAILABLE`. Nothing is written.
3. Select the region (`preferred_region`, else the default) and sign the host token. A signing failure returns `SFU_UNAVAILABLE`. Nothing is written.
4. Write the stream state, the stream metadata, the presence, and the `stream_sessions` record (crew, host, `mode: "sfu"`, region, endpoint). Push `stream_started`.
5. Return `{stream_id, session_id, mode: "sfu", sfu_endpoint, sfu_token, capture}`.

`watch_stream`:

1. Load the `stream_sessions` record and check the crew membership.
2. A record without an SFU endpoint (from before the SFU was the only transport) returns `SFU_UNAVAILABLE`.
3. Sign the viewer token. A signing failure returns `SFU_UNAVAILABLE`.
4. Return `{mode: "sfu", sfu_endpoint, sfu_token, width, height, bitrate_kbps}`.

### 3.2 `voice_state.go` - `voice_join`

1. Resolve the channel and check the crew membership.
2. If `sfuAvailableForCrew` is false, return `SFU_UNAVAILABLE`. Nobody is seated.
3. Seat the member (`joinVoiceRoom`, capacity `MaxVoiceChannelMembers` = 50).
4. Sign the voice token for the room's region. A signing failure undoes the seat (`voiceLeaveInternal`) and returns `SFU_UNAVAILABLE`.
5. Return `{success, channel_id, voice_state, mode: "sfu", sfu_endpoint, sfu_token, bitrate_kbps}`.

### 3.3 `crews.go` - Crew Metadata

`newCrewMetadata` stores `"sfu_enabled": true` on every new crew. `sfuEnabledForCrew` reads the same key; an absent key also means on.

### 3.4 `main.go` - Register New RPCs

```go
func InitModule(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, initializer runtime.Initializer) error {
    // ... existing registrations ...

    // SFU auth init
    if err := initSFUAuth(); err != nil {
        logger.Error("SFU auth init failed: %v", err)
        // Non-fatal: SFU features just won't be available
    }

    // Register new/updated RPCs
    initializer.RegisterRpc("start_stream", StartStreamRPC)
    initializer.RegisterRpc("watch_stream", WatchStreamRPC)
    // voice_join is already registered, just updated in-place

    return nil
}
```

---

## 4. Client Integration (Historical Sketches)

> **Do not implement from the code samples in §4–5.** They describe the
> pre-RTP rollout shape and retain references to `StreamPacket`, `send_media`,
> and a stream `media` DataChannel. Current wiring is:
>
> - `client/streaming.rs`: SFU connect/join, fail-closed setup;
> - `stream/sink_sfu.rs`: complete Annex-B AU send through native RTP;
> - `transport/sfu_connection.rs`: WebSocket signaling, control DC, role-based RTP peer;
> - `stream/rtp_peer.rs`: safe native RTP receive wrappers.

### 4.1 `sfu_connection.rs` - WebSocket + WebRTC to SFU

This is the transport layer between the client and the SFU server. It manages:
- WebSocket connection for signaling (JSON messages)
- WebRTC PeerConnection via libdatachannel (role-based: `StreamHost`, `StreamViewer`, voice)
- **Stream sessions:** one H.264 RTP video track + reliable `control` DataChannel (no stream `media` DC)
- **Voice sessions:** unreliable `media` + reliable `control` DataChannels, plus Opus RTP tracks from SFU
- Background tasks for ICE candidates and signaling messages

```rust
// mello-core/src/transport/sfu_connection.rs

use crate::stream::StreamError;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

pub struct SfuConnection {
    ws: Mutex<WebSocketStream>,               // tungstenite or tokio-tungstenite
    peer_connection: Arc<PeerConnection>,       // libdatachannel via mello-core-sys
    media_channel: Arc<DataChannel>,            // voice only (unreliable)
    control_channel: Arc<DataChannel>,          // reliable (stream + voice)
    // Stream host/viewer video uses native RTP track APIs, not media_channel
    event_tx: mpsc::Sender<SfuEvent>,
    event_rx: Mutex<mpsc::Receiver<SfuEvent>>,
    server_id: String,
    region: String,
}

#[derive(Debug)]
pub enum SfuEvent {
    MemberJoined { user_id: String, role: String },
    MemberLeft { user_id: String, reason: String },
    ControlPacket { data: Vec<u8> },           // reliable control DC
    Disconnected { reason: String },
    // Stream video arrives via poll_received_access_unit / RTP track, not SfuEvent
}

impl SfuConnection {
    pub async fn connect(endpoint: &str, token: &str) -> Result<Self, StreamError> {
        // 1. WebSocket connect
        let url = format!("{}?token={}", endpoint, token);
        let ws = tokio_tungstenite::connect_async(&url).await
            .map_err(|e| StreamError::SfuConnectFailed(e.to_string()))?;

        // 2. Read welcome message
        let welcome: SignalingMessage = ws.recv_json().await?;
        if welcome.msg_type != "welcome" {
            return Err(StreamError::SfuProtocolError("expected welcome".into()));
        }

        // 3. Create PeerConnection via libdatachannel
        //    Uses the existing mello-core-sys FFI bindings.
        //    The PeerConnection config needs the SFU's public IP
        //    as a remote ICE candidate (ICE-lite server).
        let config = PeerConnectionConfig {
            ice_servers: vec![], // SFU uses ICE-lite, no STUN/TURN needed
        };
        let pc = PeerConnection::new(&config)?;

        // 4. Create DataChannels
        let media_dc = pc.create_data_channel("media", DataChannelConfig {
            ordered: false,
            max_retransmits: Some(0),
        })?;

        let control_dc = pc.create_data_channel("control", DataChannelConfig {
            ordered: true,
            max_retransmits: None, // reliable
        })?;

        // 5. Create and send SDP offer
        let offer = pc.create_offer().await?;
        pc.set_local_description(&offer).await?;
        ws.send_json(&SignalingMessage::offer(&offer.sdp)).await?;

        // 6. Receive and apply SDP answer
        let answer: SignalingMessage = ws.recv_json().await?;
        pc.set_remote_description(&answer.sdp()).await?;

        // 7. ICE candidate exchange
        //    With ICE-lite server, this is typically 1 round.
        //    Spawn background task for any trickle candidates.
        let (event_tx, event_rx) = mpsc::channel(256);

        let conn = Self {
            ws: Mutex::new(ws),
            peer_connection: Arc::new(pc),
            media_channel: Arc::new(media_dc),
            control_channel: Arc::new(control_dc),
            event_tx,
            event_rx: Mutex::new(event_rx),
            server_id: welcome.data.server_id,
            region: welcome.data.region,
        };

        // Spawn background signaling listener
        conn.spawn_signaling_listener();
        // Spawn DataChannel receive handlers
        conn.spawn_media_receiver();

        Ok(conn)
    }

    pub async fn send_media(&self, data: &[u8]) -> Result<(), StreamError> {
        self.media_channel.send_unreliable(data)
            .map_err(|e| StreamError::SfuSendFailed(e.to_string()))
    }

    pub async fn send_control(&self, data: &[u8]) -> Result<(), StreamError> {
        self.control_channel.send(data)
            .map_err(|e| StreamError::SfuSendFailed(e.to_string()))
    }

    pub async fn join_stream(&self, session_id: &str, role: &str) -> Result<SessionInfo, StreamError> {
        let msg = SignalingMessage::join_stream(session_id, role);
        self.ws.lock().await.send_json(&msg).await?;
        let resp = self.ws.lock().await.recv_json::<SignalingMessage>().await?;
        match resp.msg_type.as_str() {
            "joined" => Ok(resp.into_session_info()),
            "error" => Err(StreamError::SfuJoinFailed(resp.error_message())),
            _ => Err(StreamError::SfuProtocolError("unexpected response".into())),
        }
    }

    pub async fn join_voice(&self, crew_id: &str, channel_id: &str) -> Result<SessionInfo, StreamError> {
        let msg = SignalingMessage::join_voice(crew_id, channel_id);
        self.ws.lock().await.send_json(&msg).await?;
        let resp = self.ws.lock().await.recv_json::<SignalingMessage>().await?;
        match resp.msg_type.as_str() {
            "joined" => Ok(resp.into_session_info()),
            "error" => Err(StreamError::SfuJoinFailed(resp.error_message())),
            _ => Err(StreamError::SfuProtocolError("unexpected response".into())),
        }
    }

    pub async fn leave(&self) -> Result<(), StreamError> {
        let msg = SignalingMessage::leave();
        self.ws.lock().await.send_json(&msg).await.ok();
        Ok(())
    }

    pub async fn recv_event(&self) -> Option<SfuEvent> {
        self.event_rx.lock().await.recv().await
    }

    fn spawn_signaling_listener(&self) {
        // Background task: read WebSocket messages, parse, send to event_tx.
        // Handles: member_joined, member_left, ice_candidate, session_ended, error.
        // On WS close: send SfuEvent::Disconnected.
    }

    fn spawn_media_receiver(&self) {
        // Background task: read from media DataChannel, send SfuEvent::MediaPacket.
        // Background task: read from control DataChannel, send SfuEvent::ControlPacket.
        // These events are consumed by the StreamManager (viewer) or VoiceManager.
    }
}

// --- Signaling message types ---

#[derive(Debug, Serialize, Deserialize)]
struct SignalingMessage {
    #[serde(rename = "type")]
    msg_type: String,
    seq: i32,
    data: serde_json::Value,
}

impl SignalingMessage {
    fn offer(sdp: &str) -> Self { /* ... */ }
    fn join_stream(session_id: &str, role: &str) -> Self { /* ... */ }
    fn join_voice(crew_id: &str, channel_id: &str) -> Self { /* ... */ }
    fn leave() -> Self { /* ... */ }
}

pub struct SessionInfo {
    pub session_type: String,
    pub session_id: String,
    pub members: Vec<MemberInfo>,
}

pub struct MemberInfo {
    pub user_id: String,
    pub role: String,
}
```

**Key implementation note:** The WebSocket library choice depends on what mello-core already uses. If `tokio-tungstenite` is already a dependency (for Nakama WS), use that. If not, `tungstenite` with a `tokio` adapter works. The PeerConnection and DataChannel types are your existing `mello-core-sys` FFI wrappers around libdatachannel.

### 4.2 `sink_sfu.rs` - Replace the Stub

Replaces the existing stub in `mello-core/src/stream/sink_sfu.rs`:

```rust
// mello-core/src/stream/sink_sfu.rs

use crate::stream::{PacketSink, StreamPacket, StreamError};
use crate::transport::sfu_connection::{SfuConnection, SfuEvent};
use async_trait::async_trait;
use std::sync::Arc;

pub struct SfuSink {
    connection: Arc<SfuConnection>,
}

impl SfuSink {
    pub async fn new(endpoint: &str, token: &str) -> Result<Self, StreamError> {
        let connection = SfuConnection::connect(endpoint, token).await?;
        Ok(Self {
            connection: Arc::new(connection),
        })
    }

    pub fn connection(&self) -> &Arc<SfuConnection> {
        &self.connection
    }
}

#[async_trait]
impl PacketSink for SfuSink {
    pub async fn send_video(&self, annex_b: &[u8], capture_timestamp_us: u64, _is_keyframe: bool) -> Result<(), StreamError> {
        self.connection.send_video_access_unit(annex_b, capture_timestamp_us).await
    }

    pub async fn send_audio_stub(&self, byte_len: usize) { /* game audio not wired */ }

    pub async fn send_control(&self, data: &[u8]) -> Result<(), StreamError> {
        self.connection.send_control(data).await
    }

    async fn on_viewer_joined(&self, viewer_id: &str) {
        // SFU notifies the host via signaling, not via PacketSink.
        // The SfuConnection's signaling listener handles member_joined events.
        log::debug!("SFU sink: viewer joined {}", viewer_id);
    }

    async fn on_viewer_left(&self, viewer_id: &str) {
        log::debug!("SFU sink: viewer left {}", viewer_id);
    }
}
```

---

## 5. Client Changes (Historical Sketches)

### 5.1 `stream/host.rs` - Updated `start_stream`

The host always builds an `SfuSink` from the `sfu_endpoint` and `sfu_token` of the `start_stream` response. A failed SFU connect, join or control channel ends the start with a stream error (`client/streaming.rs`, `finish_stream_start`).

Verify this code exists and that it passes `sfu_endpoint` and `sfu_token` from the RPC response. The `StartStreamResponse` deserialization needs to include the new fields:

```rust
// mello-core/src/nakama/types.rs (or wherever RPC response types live)

#[derive(Debug, Deserialize)]
pub struct StartStreamResponse {
    pub session_id: String,
    pub mode: String, // always "sfu"

    #[serde(default)]
    pub sfu_endpoint: Option<String>,
    #[serde(default)]
    pub sfu_token: Option<String>,
}
```

### 5.2 SFU viewer path (implemented)

Viewer RTP receive lives in `mello-core/src/client/streaming.rs`, `mello-core/src/stream/rtp_peer.rs`, and libmello `RtpVideoReceiverSession`. The probe tool is `tools/sfu-stream-viewer-probe` (`scripts/run-stream-viewer.ps1`). There is no separate `stream/viewer.rs`; video arrives on the negotiated H.264 RTP track, not via DataChannel `MediaPacket` events.

<details>
<summary>Legacy planning snippet (pre-RTP migration — do not implement)</summary>

Obsolete sketch referenced `stream/viewer.rs` and `SfuEvent::MediaPacket` on DataChannel. Current code uses native RTP receive instead.

</details>

### 5.3 `voice/mod.rs` - SFU Voice

The client joins every voice channel through the SFU: `voice_join`, then the SFU connect and join off the command loop (`client/sfu_voice_join.rs`), then `VoiceManager::join_voice_sfu`. A failed join leaves voice disconnected, and the reconnect scheduler tries again (spec 10 section 7.6).

**Updated `VoiceJoinResponse` type:**

```rust
#[derive(Debug, Deserialize)]
pub struct VoiceJoinResponse {
    pub mode: String,
    pub members: Vec<VoiceMemberState>,
    pub sfu_endpoint: Option<String>,
    pub sfu_token: Option<String>,
}
```

### 5.4 `stream/mod.rs` - Add StreamError Variants

```rust
// Add to StreamError enum:
pub enum StreamError {
    // ... existing variants ...
    SfuConnectFailed(String),
    SfuSendFailed(String),
    SfuJoinFailed(String),
    SfuProtocolError(String),
}
```

Remove the existing `SfuNotImplemented` variant (it's no longer needed).

### 5.5 `transport/mod.rs` - Export SfuConnection

```rust
// mello-core/src/transport/mod.rs
pub mod sfu_connection;
```

---

## 6. Files Summary

### New files

```
backend/nakama/data/modules/
  sfu_auth.go              # JWT RS256 signing
  sfu_routing.go           # Region selection, endpoint config
  sfu_entitlement.go       # sfuEnabledForCrew() kill switch, SFU_UNAVAILABLE

mello-core/src/
  transport/sfu_connection.rs   # WebSocket + WebRTC connection to SFU
```

### Modified files

```
backend/nakama/data/modules/
  main.go                  # Register watch_stream, init SFU auth
  streaming.go             # start_stream and watch_stream answer SFU only
  voice_state.go           # voice_join answers SFU only
  crews.go                 # sfu_enabled on new crews

mello-core/src/
  stream/sink_sfu.rs       # SfuSink — native RTP AU send + control DC
  stream/rtp_peer.rs       # libmello RTP receive wrappers
  client/streaming.rs      # SFU viewer receive + presentation path
  stream/mod.rs            # StreamError variants
  voice/mod.rs             # VoiceManager, SFU voice
  transport/mod.rs         # Export sfu_connection
  nakama/types.rs          # Add sfu_endpoint, sfu_token to response types

libmello/src/transport/
  rtp_video_sender.cpp     # Host RTP send to the SFU with pacing + IDR gate
  rtp_video_receiver_session.cpp  # Viewer AU assembly + decode feed
```

---

## 7. Testing Checklist

> Historical implementation checklist. Current commands and release gates are
> maintained in [`TESTING.md`](../../TESTING.md) and
> `scripts/run-stream-certification.ps1`.

### Backend RPCs
- [ ] `start_stream` -> returns `mode: "sfu"` with endpoint + token
- [ ] `start_stream` with no `SFU_JWT_PRIVATE_KEY` -> `SFU_UNAVAILABLE`, no stream state written
- [ ] `start_stream` on a crew with `"sfu_enabled": false` -> `SFU_UNAVAILABLE`
- [ ] `watch_stream` with valid session -> returns viewer token
- [ ] `watch_stream` with wrong crew -> returns `NOT_CREW_MEMBER`
- [ ] `watch_stream` for a session without an SFU endpoint -> `SFU_UNAVAILABLE`
- [ ] `voice_join` -> returns `mode: "sfu"` with endpoint + token, cap at 50
- [ ] `voice_join` with no `SFU_JWT_PRIVATE_KEY` -> `SFU_UNAVAILABLE`, nobody seated
- [ ] JWT token is valid RS256, SFU can verify it

### Client SfuSink
- [ ] `SfuSink::new()` connects to SFU WebSocket
- [ ] WebRTC PeerConnection established (ICE-lite)
- [ ] Stream: control DC open + host RTP video track open
- [ ] `send_video()` sends Annex-B AUs via RTP track
- [ ] `send_control()` sends via control DataChannel
- [ ] Viewer receives complete AUs via `poll_received_access_unit`
- [ ] Host receives aggregated PLI/REMB via `poll_video_feedback`

### Client VoiceManager
- [ ] SFU join: connects to SFU, joins voice session
- [ ] SFU voice: outgoing audio sent via SfuConnection
- [ ] SFU voice: incoming audio from SFU fed to libmello
- [ ] Leave: SFU connection closed, server notified

### End-to-End
- [ ] Host streams via SFU RTP, viewer decodes video
- [ ] Multiple viewers receive the same RTP relay
- [ ] Viewer REMB/PLI reaches host (bitrate adjusts, IDR on PLI)
- [ ] Voice works through SFU (2+ members)
- [ ] Disconnection handled cleanly (no orphaned sessions)

---

## 8. Robustness (v0.3)

The original `SfuConnection` sketch above is augmented with liveness and lifecycle hardening (client behaviour in [02-MELLO-CORE.md](../02-MELLO-CORE.md)).

- **Liveness keepalive / RTT.** The client sends `{"type":"ping","ts":<ms>}` on the reliable control DataChannel (~2s). The SFU echoes the payload as `"pong"` and the client derives control-path RTT/pong-age from `ts`.
  - Health is **multi-signal**, not only `is_connected()`: `is_connected && control_dc_open && pong_fresh`, with a short signaling grace window so renegotiation churn does not cause false reconnects.
  - The client declares dead after 3 consecutive unhealthy checks (~6s) and tears down via a unified SFU disconnect path (`liveness_timeout`) before reconnect.
  - *Server note:* the pong type flip is byte-sensitive (`"ping"` -> `"pong"`). Keep it on the text/control path and preserve the correct byte index; malformed `"pong"` silently breaks RTT/liveness.
- **Server-initiated disconnect signaling.** `error` / `session_ended` / slow-peer signals are surfaced to the client as `SfuEvent::Disconnected` to drive reconnect, rather than being logged-only.
- **`SfuConnection` task lifecycle.** The signaling-listener and `client_stats`-reporter tasks are tracked and **aborted on `Drop`** so a replaced/stale connection's tasks stop immediately instead of ticking against a dead socket.
- **Reconnect decoupled from Nakama.** A Nakama WS reconnect re-asserts roster membership only; SFU media rebuild is owned solely by the voice tick, so Nakama blips don't churn the SFU peer.
- **Renegotiation-tolerant track reads.** The SFU raises/pauses its consecutive-EOF inbound-track cutoff during active renegotiation so transient read errors don't kill healthy audio tracks.
- **Join failure policy.** Stream sessions are fail-closed: connect, join, or control-channel failure surfaces a stream error. Voice retains its separately documented reconnect behavior. When Nakama cannot sign an SFU token, the RPC returns `SFU_UNAVAILABLE`.
- **Diagnostics.** The SFU logs `peer_close_called` (with a compact caller stack) and `pc_closed` at INFO so every peer teardown is attributable to a concrete cause.

Reconcile-oracle (Nakama → SFU admin API) is covered in [04-BACKEND.md](../04-BACKEND.md). Soak/fault harnesses are in [`TESTING.md`](../../TESTING.md).
