# SFU Server Specification

> **Component:** SFU Server (Go) · mello-core SfuSink (Rust) · Backend Integration (Go)  
> **Version:** 0.1  
> **Status:** Planned  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)  
> **Related:** [12-STREAMING.md](./12-STREAMING.md), [13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md), [04-BACKEND.md](./04-BACKEND.md)  
> **Repository:** Private (proprietary component)

---

## 1. Overview

The SFU (Selective Forwarding Unit) is Mello's server-side media relay. It receives media packets from senders and forwards them to receivers without transcoding, mixing, or inspecting contents. For **streaming**, the SFU forwards packets over DataChannels using the custom-framed protocol from [12-STREAMING.md §5](./12-STREAMING.md). For **voice**, the SFU forwards Opus audio via RTP tracks (see §5.3).

### 1.1 What the SFU Enables

| Capability | Without SFU (P2P) | With SFU |
|---|---|---|
| Stream viewers | Max 5 (host uploads 5×) | Unlimited (host uploads 1×) |
| Voice per channel | Max 6 (full mesh, 5 connections each) | Unlimited (each member: 1 upload, 1 download) |
| Host bandwidth | Scales linearly with viewers | Constant regardless of viewer count |
| Stream quality consistency | Varies per viewer (per-viewer ABR) | Single quality, server relays equally |

### 1.2 Design Principles

| Principle | Implication |
|---|---|
| **Opaque forwarding** | SFU never parses video/audio payloads — it forwards the 12-byte-header packets from spec 12 verbatim |
| **DataChannel protocol parity** | Client uses the same packet format for both P2P and SFU paths — zero transport-layer changes |
| **Server decides topology** | Unchanged from spec 12 §9 — `start_stream` RPC returns `"sfu"` mode with endpoint and token |
| **Latency over features** | No server-side transcoding, no mixing, no simulcast in v1 — raw forwarding only |
| **Stateless sessions** | Session state is in-memory only — if the SFU restarts, clients reconnect and resume |
| **Region-aware routing** | Nakama routes clients to the nearest SFU region |

### 1.3 What This Spec Does NOT Cover

- Simulcast / SVC (multiple quality layers) — future spec
- Server-side recording — future spec, separate service
- Server-side transcoding — explicitly out of scope, possibly forever
- Analytics / metrics pipeline — see [15-DEBUG-TELEMETRY.md](./15-DEBUG-TELEMETRY.md) for client-side; server-side telemetry is a future spec

---

## 2. Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                            SFU SERVER (Go)                              │
│                                                                         │
│  ┌───────────────┐    ┌───────────────────────────────────────────────┐ │
│  │  WebSocket     │    │              Session Router                   │ │
│  │  Signaling     │───▶│                                               │ │
│  │  Server        │    │  ┌─────────────┐  ┌─────────────────────┐    │ │
│  └───────────────┘    │  │  Stream      │  │  Voice               │    │ │
│                        │  │  Sessions    │  │  Sessions            │    │ │
│  ┌───────────────┐    │  │             │  │                     │    │ │
│  │  WebRTC        │    │  │  1 host →   │  │  N members ↔       │    │ │
│  │  Transport     │◀──▶│  │  N viewers  │  │  N members          │    │ │
│  │  (Pion)        │    │  │             │  │                     │    │ │
│  └───────────────┘    │  └─────────────┘  └─────────────────────┘    │ │
│                        └───────────────────────────────────────────────┘ │
│                                                                         │
│  ┌───────────────┐    ┌───────────────┐                                 │
│  │  Auth          │    │  Health /     │                                 │
│  │  (JWT verify)  │    │  Metrics      │                                 │
│  └───────────────┘    └───────────────┘                                 │
└─────────────────────────────────────────────────────────────────────────┘
         │                        │
         │ WSS (signaling)        │ WebRTC DataChannel (media)
         │                        │
┌────────┴────────────────────────┴────────────────────────────────────────┐
│                           MELLO CLIENT                                    │
│                                                                           │
│  mello-core                                                               │
│  ┌──────────────────────────┐                                             │
│  │ SfuSink (implements      │                                             │
│  │  PacketSink trait)       │                                             │
│  │                          │                                             │
│  │  - WS signaling          │                                             │
│  │  - WebRTC PeerConnection │                                             │
│  │  - DataChannel (media)   │                                             │
│  └──────────────────────────┘                                             │
└───────────────────────────────────────────────────────────────────────────┘
```

### 2.1 Technology Stack

| Component | Technology | Rationale |
|---|---|---|
| Language | Go 1.21+ | Consistent with Nakama backend; Pion ecosystem |
| WebRTC | [Pion WebRTC](https://github.com/pion/webrtc) v4 | Battle-tested Go WebRTC stack; DataChannel support |
| Signaling | WebSocket (gorilla/websocket or nhooyr/websocket) | Simple, bidirectional, TLS-capable |
| Auth | JWT (RS256) | Signed by Nakama, verified by SFU with shared public key |
| Deployment | Docker container on GCP Compute Engine | Dedicated IP, full UDP support, premium networking |

### 2.2 Key Dependency: Pion

The SFU uses Pion exclusively for WebRTC peer connections and DataChannels. Pion runs **ICE-lite** mode — the server has a public IP and does not perform ICE candidate gathering. The client initiates the ICE connection to the server's known IP.

```go
// ICE-lite configuration — server doesn't gather candidates
settingEngine := webrtc.SettingEngine{}
settingEngine.SetLite(true)
settingEngine.SetNAT1To1IPs([]string{publicIP}, webrtc.ICECandidateTypeHost)
```

This eliminates ICE negotiation latency on the server side. The client's libdatachannel handles ICE normally — it sees the server as a peer with a single host candidate.

---

## 3. Session Types

The SFU handles two distinct session types with different transport models. Stream sessions use DataChannels (spec 12 packet format). Voice sessions use RTP audio tracks (§5.3).

### 3.1 Stream Session (1-to-Many)

```
Host ──DataChannel──▶ SFU ──DataChannel──▶ Viewer A
                           ──DataChannel──▶ Viewer B
                           ──DataChannel──▶ Viewer C
                           ──DataChannel──▶ ...
```

- One host publishes video + audio packets
- N viewers receive forwarded copies
- Viewers send control packets (loss reports, IDR requests) back to host via SFU
- Host sends a single stream at chosen quality; no per-viewer ABR at the SFU level in v1

### 3.2 Voice Session (Many-to-Many)

```
Member A ──RTP Track──▶ SFU ──RTP Track──▶ Member B, C, D
Member B ──RTP Track──▶ SFU ──RTP Track──▶ Member A, C, D
Member C ──RTP Track──▶ SFU ──RTP Track──▶ Member A, B, D
Member D ──RTP Track──▶ SFU ──RTP Track──▶ Member A, B, C
         (+ reliable DataChannel for control/signaling per member)
```

- Each member publishes Opus audio via an RTP track (PT 111, Opus/48000/2)
- SFU creates per-sender `sendonly` outgoing tracks and renegotiates SDP when members join/leave
- No mixing — each member receives N-1 separate RTP streams and the client mixes locally
- Muted members: client stops sending packets (SFU does not need to know about mute state)

### 3.3 Session Lifecycle

```
┌─────────┐     ┌─────────┐     ┌─────────┐     ┌─────────┐
│ Created  │────▶│ Active  │────▶│ Draining│────▶│ Closed  │
└─────────┘     └─────────┘     └─────────┘     └─────────┘
   (first         (members        (last member     (cleanup)
    member         connected)      left; 30s
    joins)                         grace period)
```

A session enters `Draining` when the last member disconnects. It stays in `Draining` for 30 seconds — if someone reconnects (e.g. brief network drop), the session resumes without creating a new one. After 30 seconds with no members, the session is garbage-collected.

For stream sessions, if the host disconnects, the session immediately transitions to `Draining` regardless of viewer count — viewers cannot sustain a session without a host.

---

## 4. Signaling Protocol (WebSocket)

### 4.1 Connection

The client connects via WebSocket to the SFU endpoint received from the `start_stream` or `voice_join` RPC:

```
wss://sfu-eu.mello.app/ws?token=<JWT>
```

The JWT is verified on connection. If invalid, the server responds with HTTP 401 and closes. If valid, the WebSocket is established and the server sends a `welcome` message.

### 4.2 Message Format

All signaling messages are JSON over WebSocket:

```json
{
  "type": "message_type",
  "seq": 42,
  "data": { ... }
}
```

`seq` is a monotonically increasing integer per direction (client→server and server→client tracked independently). Used for request/response correlation — a response carries the same `seq` as the request that triggered it.

### 4.3 Client → Server Messages

#### `join_stream` (Host)

```json
{
  "type": "join_stream",
  "seq": 1,
  "data": {
    "session_id": "str_abc123",
    "role": "host"
  }
}
```

#### `join_stream` (Viewer)

```json
{
  "type": "join_stream",
  "seq": 1,
  "data": {
    "session_id": "str_abc123",
    "role": "viewer"
  }
}
```

#### `join_voice`

```json
{
  "type": "join_voice",
  "seq": 1,
  "data": {
    "crew_id": "crew_xyz",
    "channel_id": "ch_general"
  }
}
```

#### `offer` (WebRTC SDP)

Sent after `join_*` to establish the WebRTC PeerConnection:

```json
{
  "type": "offer",
  "seq": 2,
  "data": {
    "sdp": "v=0\r\no=- ..."
  }
}
```

#### `ice_candidate`

```json
{
  "type": "ice_candidate",
  "seq": 3,
  "data": {
    "candidate": "candidate:...",
    "sdp_mid": "0",
    "sdp_mline_index": 0
  }
}
```

#### `leave`

```json
{
  "type": "leave",
  "seq": 10,
  "data": {}
}
```

Graceful disconnect. Server cleans up the peer's session membership and WebRTC resources. If the client disconnects without sending `leave` (crash, network loss), the server detects this via WebSocket close / PeerConnection ICE failure and performs the same cleanup.

### 4.4 Server → Client Messages

#### `welcome`

Sent immediately after WebSocket connection:

```json
{
  "type": "welcome",
  "seq": 1,
  "data": {
    "server_id": "sfu-eu-01",
    "region": "eu-west"
  }
}
```

#### `answer` (WebRTC SDP)

```json
{
  "type": "answer",
  "seq": 2,
  "data": {
    "sdp": "v=0\r\no=- ..."
  }
}
```

#### `ice_candidate` (Server → Client)

Same format as client→server.

#### `joined`

Confirms the client joined a session:

```json
{
  "type": "joined",
  "seq": 1,
  "data": {
    "session_type": "stream",
    "session_id": "str_abc123",
    "members": [
      { "user_id": "user_a", "role": "host" },
      { "user_id": "user_b", "role": "viewer" }
    ]
  }
}
```

#### `member_joined`

```json
{
  "type": "member_joined",
  "seq": 5,
  "data": {
    "user_id": "user_c",
    "role": "viewer"
  }
}
```

#### `member_left`

```json
{
  "type": "member_left",
  "seq": 6,
  "data": {
    "user_id": "user_c",
    "reason": "disconnect"
  }
}
```

#### `error`

```json
{
  "type": "error",
  "seq": 1,
  "data": {
    "code": "SESSION_FULL",
    "message": "Stream session has reached viewer limit"
  }
}
```

### 4.5 Connection Flow (Full Sequence)

```
Client                          SFU Server
  │                                │
  │──── WSS Connect ──────────────▶│  (JWT in query param)
  │◀─── welcome ──────────────────│
  │                                │
  │──── join_stream (host) ───────▶│  (creates or joins session)
  │◀─── joined ───────────────────│
  │                                │
  │──── offer (SDP) ──────────────▶│  (WebRTC negotiation)
  │◀─── answer (SDP) ─────────────│
  │                                │
  │◀──▶ ice_candidate ────────────▶│  (ICE-lite: usually 1 round)
  │                                │
  │════ DataChannel established ══▶│
  │                                │
  │──── [stream packets] ─────────▶│  (media flows over DataChannel)
  │                                │
```

---

## 5. Media Transport

### 5.1 DataChannel Configuration

The SFU creates DataChannels with the same configuration used in P2P (spec 12 §5.1):

```go
// DataChannel options — matches client P2P configuration
dcConfig := webrtc.DataChannelInit{
    Ordered:        boolPtr(false),
    MaxRetransmits: uint16Ptr(0),
}
```

**Unreliable, unordered.** This is non-negotiable — same reasoning as spec 12: reliable/ordered channels introduce head-of-line blocking.

Two DataChannels per PeerConnection:

| Label | Purpose |
|---|---|
| `media` | Video + audio + FEC packets (types 0x01, 0x02, 0x03) |
| `control` | Control packets (type 0x04): loss reports, IDR requests, quality changes |

Splitting media and control onto separate DataChannels prevents control messages from being delayed behind large video packets in the SCTP send queue.

### 5.2 Packet Forwarding (Stream Session)

The SFU's hot path for stream sessions:

```go
// Pseudocode — stream session forwarding loop
func (s *StreamSession) onHostMediaPacket(data []byte) {
    // No parsing, no inspection — forward the raw blob to all viewers
    viewers := s.viewers.Snapshot()  // lock-free snapshot
    for _, viewer := range viewers {
        viewer.mediaChannel.SendNonBlocking(data)
    }
}

func (s *StreamSession) onViewerControlPacket(viewerID string, data []byte) {
    // Control packets from viewers are forwarded to the host
    s.host.controlChannel.SendNonBlocking(data)
}
```

**Key design decisions:**

- `SendNonBlocking`: if a viewer's DataChannel buffer is full (slow viewer), the packet is dropped. The viewer's FEC / IDR recovery handles the loss. A slow viewer must never block packet delivery to other viewers.
- No packet inspection: the SFU does not parse the 12-byte header. It does not know whether a packet is a keyframe, FEC parity, or audio. It forwards everything.
- Exception: the SFU *does* inspect the `type` byte (offset 0) to route between `media` and `control` DataChannels. This is the only byte the SFU reads.

### 5.3 Voice Transport (RTP Tracks)

Voice audio uses **RTP audio tracks** instead of DataChannels. Each member's Opus audio is sent as a standard RTP stream (PT 111, Opus/48000/2) over the same DTLS/ICE transport as DataChannels.

**Per-sender track model:** When member B joins a session that already has member A, the SFU:
1. Creates a `TrackLocalStaticRTP` for A's audio (SSRC = random, `msid` = A's user_id).
2. Adds the track to B's PeerConnection via `AddTransceiverFromTrack` with `sendonly` direction.
3. Sends a renegotiation offer to B.
4. Does the same for B's audio → A.
5. Wires A's incoming `TrackRemote` to forward RTP packets to B's outgoing track (and vice versa).

```go
func (s *VoiceSession) WireAudioTrack(peer *Peer, track *webrtc.TrackRemote) {
    senderID := peer.UserID
    go func() {
        for {
            pkt, _, err := track.ReadRTP()
            if err != nil { return }
            s.members.ForEach(func(other *Peer) {
                if other.UserID != senderID {
                    other.SendRTP(senderID, pkt) // rewrites SSRC per-track
                }
            })
        }
    }()
}
```

The client identifies each incoming track's sender via the `msid` SDP attribute (set to the sender's user_id). This eliminates the need for application-layer sender-id framing.

**Phantom transceiver handling:** Pion's `SetHandleUndeclaredSSRCWithoutAnswer(true)` (needed because libdatachannel doesn't send `mid` RTP header extensions) can create implicit `sendrecv` transceivers that accumulate across renegotiations. `StopPhantomTransceivers()` runs before each `CreateOffer()` to stop any audio transceiver that is not an explicit outgoing track or the initial recvonly transceiver.

**RTT measurement:** The reliable DataChannel carries ping/pong messages (`{"type":"ping","ts":...}` / `{"type":"pong","ts":...}`) for voice latency estimation. The SFU echoes pings immediately; the client sends one every ~2 seconds and computes smoothed RTT.

### 5.4 Bandwidth and Buffer Management

The SFU does NOT perform bandwidth estimation or congestion control. The client handles this:

- **Stream ABR:** The host's ABR controller (spec 12 §7.2) receives loss reports from viewers (relayed through the SFU) and adjusts bitrate accordingly. In SFU mode, the host adjusts based on the **worst-reporting viewer** — since all viewers receive the same stream, the host targets the lowest common denominator.
- **Voice:** Opus at 128kbps stereo is well within any reasonable connection. No ABR needed for voice.

Server-side buffer limits per peer:

```go
const (
    // Maximum DataChannel send buffer before dropping packets
    maxSendBufferBytes = 2 * 1024 * 1024  // 2 MB

    // If a peer's buffer exceeds this for > 5 seconds, force disconnect
    bufferTimeoutSec = 5
)
```

A viewer whose buffer stays full for >5 seconds is force-disconnected with a `peer_slow` error. This prevents a single bad connection from accumulating unbounded memory on the server.

---

## 6. Authentication and Security

### 6.1 JWT Token

Nakama generates a short-lived JWT when the client calls `start_stream` or `voice_join` (SFU mode):

```go
// Nakama side — token generation
type SFUTokenClaims struct {
    jwt.RegisteredClaims
    UserID    string `json:"uid"`
    SessionID string `json:"sid"`     // stream session ID or voice session key
    Type      string `json:"type"`    // "stream" or "voice"
    Role      string `json:"role"`    // "host" or "viewer" (stream) / "member" (voice)
    CrewID    string `json:"crew_id"`
    ChannelID string `json:"ch_id,omitempty"`  // voice sessions only
    Region    string `json:"region"`  // target SFU region
}

// Token lifetime: 5 minutes (client must connect within this window)
// After connection, the WebSocket session persists regardless of token expiry
```

The JWT is signed with RS256 using a private key held by Nakama. The SFU verifies with the corresponding public key. The SFU never contacts Nakama directly — token verification is self-contained.

### 6.2 Authorization Enforcement

The SFU enforces these rules on `join_*`:

| Check | Rule |
|---|---|
| Token `type` matches join message | `join_stream` requires `type: "stream"`, `join_voice` requires `type: "voice"` |
| Token `role` matches claimed role | A viewer token cannot join as host |
| Token `session_id` matches | Must match the session being joined |
| Stream host uniqueness | Only one host per stream session |
| Token not expired | Checked on WebSocket connect; not rechecked after |

### 6.3 Encryption

All media flows over DTLS-encrypted WebRTC DataChannels. The SFU terminates DTLS — it decrypts incoming packets and re-encrypts for each outbound peer. This is inherent to the WebRTC forwarding model and cannot be avoided without end-to-end encryption (E2EE), which is deferred.

**Future consideration:** E2EE using [Insertable Streams / SFrame](https://www.w3.org/TR/webrtc-encoded-transform/) would allow the SFU to forward encrypted payloads without decryption. This requires client-side key exchange and is a post-v1 feature.

---

## 7. Nakama Integration

### 7.1 Updated `start_stream` RPC

The existing `start_stream` RPC (spec 12 §9.1) gains region selection:

```go
// nakama/data/modules/streaming.go — updated

func StartStreamRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    // ... existing validation ...

    if hasPremium {
        region := selectSFURegion(userRegion)  // "eu-west" or "us-east"
        endpoint := sfuEndpoints[region]       // "wss://sfu-eu.mello.app/ws" etc.
        sessionID := generateSessionID()
        token := signSFUToken(SFUTokenClaims{
            UserID:    userID,
            SessionID: sessionID,
            Type:      "stream",
            Role:      "host",
            CrewID:    crewID,
            Region:    region,
        })

        // Register session in Nakama storage so viewers can look it up
        storeStreamSession(ctx, nk, sessionID, StreamSessionMeta{
            CrewID:      crewID,
            HostUserID:  userID,
            SFURegion:   region,
            SFUEndpoint: endpoint,
            CreatedAt:   time.Now().UTC(),
        })

        resp = StartStreamResponse{
            SessionID:   sessionID,
            Mode:        "sfu",
            SFUEndpoint: endpoint,
            SFUToken:    token,
        }
    }
    // ... P2P fallback unchanged ...
}
```

### 7.2 New `watch_stream` RPC (SFU Mode)

When a viewer wants to watch an SFU-mode stream, they need their own token:

```go
// nakama/data/modules/streaming.go — new RPC

type WatchStreamRequest struct {
    SessionID string `json:"session_id"`
}

type WatchStreamResponse struct {
    SFUEndpoint string `json:"sfu_endpoint"`
    SFUToken    string `json:"sfu_token"`
}

func WatchStreamRPC(ctx context.Context, ...) (string, error) {
    // 1. Look up session metadata from storage
    meta := loadStreamSession(ctx, nk, req.SessionID)
    if meta == nil {
        return "", errors.New("STREAM_NOT_FOUND")
    }

    // 2. Verify viewer is in the same crew
    if !isCrewMember(ctx, nk, callerID, meta.CrewID) {
        return "", errors.New("NOT_CREW_MEMBER")
    }

    // 3. Generate viewer token for the same SFU region
    token := signSFUToken(SFUTokenClaims{
        UserID:    callerID,
        SessionID: req.SessionID,
        Type:      "stream",
        Role:      "viewer",
        CrewID:    meta.CrewID,
        Region:    meta.SFURegion,
    })

    return marshal(WatchStreamResponse{
        SFUEndpoint: meta.SFUEndpoint,
        SFUToken:    token,
    }), nil
}
```

### 7.3 Voice Mode (Crew-Level Setting)

Voice mode is a **crew-level setting**, not a per-channel runtime decision. A crew is either P2P or SFU for all voice, always. There is **no live P2P↔SFU transition** — the mode is determined at `voice_join` time based on the crew's entitlement and never changes mid-session.

| Crew Type | Voice Mode | Channel Capacity | Notes |
|---|---|---|---|
| Free (official network) | P2P | 6 per channel | Hard cap, no upgrade path |
| Premium (official network) | SFU | 50 per channel | All voice routes through Mello's SFU |
| Self-hosted (no SFU subscription) | P2P | 6 per channel | Hard cap |
| Self-hosted (with SFU subscription) | SFU | 50 per channel | Routes through Mello's hosted SFU; see §7.3.2 |

**The SFU is never distributed.** Self-hosters do not run their own SFU. They buy *usage* of Mello's hosted SFU infrastructure as a paid service. Their self-hosted Nakama instance is configured with Mello's SFU endpoints, and voice/stream traffic routes through Mello's servers. The SFU binary, source code, and admin dashboard are 100% proprietary and never leave Mello's infrastructure.

The "waste" of routing small channels through the SFU is negligible — voice is 128 kbps Opus per member, and the SFU's forwarding overhead for a 2-person call is effectively zero. The benefit is enormous: zero transition code, zero edge cases, zero mid-call interruptions.

#### 7.3.1 Updated `voice_join` RPC

```go
// nakama/data/modules/voice_state.go — updated voice_join

func VoiceJoinRPC(ctx context.Context, ...) (string, error) {
    // ... existing validation ...

    crewVoiceMode := getCrewVoiceMode(ctx, nk, crewID)

    switch crewVoiceMode {
    case "sfu":
        // Premium crew or self-hosted with SFU subscription — route through Mello's SFU
        memberCount := len(room.Members)
        if memberCount >= 50 {
            return "", errors.New("CHANNEL_FULL")
        }

        region := selectSFURegion(userRegion)
        endpoint := sfuEndpoints[region]
        voiceSessionKey := fmt.Sprintf("voice:%s:%s", crewID, channelID)

        token := signSFUToken(SFUTokenClaims{
            UserID:    userID,
            SessionID: voiceSessionKey,
            Type:      "voice",
            Role:      "member",
            CrewID:    crewID,
            ChannelID: channelID,
            Region:    region,
        })

        return marshal(VoiceJoinResponse{
            Mode:        "sfu",
            SFUEndpoint: endpoint,
            SFUToken:    token,
            Members:     room.MemberList(),
        }), nil

    default: // "p2p"
        // Free / unlicensed crew — P2P mesh, 6-member cap
        memberCount := len(room.Members)
        if memberCount >= 6 {
            return "", errors.New("CHANNEL_FULL")
        }
        return existingP2PJoinFlow(ctx, nk, userID, crewID, channelID)
    }
}

func getCrewVoiceMode(ctx context.Context, nk runtime.NakamaModule, crewID string) string {
    // Check crew entitlement:
    // 1. Official network: premium crew → "sfu", free crew → "p2p"
    // 2. Self-hosted: active SFU subscription → "sfu", otherwise → "p2p"
    // In both cases, "sfu" means routing through Mello's hosted SFU infrastructure
    if hasPremiumCrew(ctx, nk, crewID) {
        return "sfu"
    }
    return "p2p"
}
```

#### 7.3.2 Self-Hosted SFU Subscription

Self-hosters purchase SFU access as a subscription. Their Nakama instance routes voice and stream traffic through Mello's hosted SFU infrastructure. They never receive the SFU binary.

**Activation flow:**

1. Self-hoster purchases SFU subscription via Mello's website
2. They receive an **SFU API key** tied to their server identity
3. They add the API key and Mello's SFU endpoints to their Nakama config:

```env
MELLO_SFU_API_KEY=msk_live_abc123...
MELLO_SFU_ENDPOINTS=sfu-eu.mello.app,sfu-us.mello.app
```

4. Their Nakama's `getCrewVoiceMode()` checks the API key validity and returns `"sfu"` for all crews
5. Clients on that server get routed to Mello's SFU with tokens signed using the shared API key
6. If already in active voice sessions when the key is added, a server restart or `voice_mode_changed` notification triggers reconnection through the SFU

**Key points:**
- The SFU validates the API key in the JWT claims, so unauthorized servers cannot use Mello's SFU
- Usage is metered and billed per the subscription (bandwidth or peer-minutes, TBD)
- If the subscription lapses, `getCrewVoiceMode()` falls back to `"p2p"` and channels cap at 6

### 7.4 Region Selection

```go
// nakama/data/modules/sfu_routing.go

var sfuEndpoints = map[string]string{
    "eu-west": "wss://sfu-eu.mello.app/ws",  // GCP europe-west3 (Frankfurt)
    "us-east": "wss://sfu-us.mello.app/ws",  // GCP us-east4 (Virginia)
}

func selectSFURegion(userRegion string) string {
    // Simple geo-mapping for beta
    // userRegion is derived from the client's IP geolocation (MaxMind GeoLite2)
    switch {
    case isEuropean(userRegion):
        return "eu-west"
    default:
        return "us-east"
    }
}
```

For stream sessions, the **host's region** determines the SFU. Viewers connect to the same SFU as the host — cross-region relay between SFU instances is a post-v1 feature.

For voice sessions, the **crew creator's region** determines the SFU. This is a simplification for beta; optimal would be selecting the region that minimises aggregate latency across all members.

---

## 8. Client Integration (SfuSink)

### 8.1 SfuSink Implementation

Replaces the stub from spec 12 §9.2:

```rust
// mello-core/src/stream/sink_sfu.rs

use crate::stream::{PacketSink, StreamPacket, StreamError};
use crate::transport::sfu_connection::SfuConnection;
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
}

#[async_trait]
impl PacketSink for SfuSink {
    async fn send_video(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        self.connection.send_media(packet.as_bytes()).await
    }

    async fn send_audio(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        self.connection.send_media(packet.as_bytes()).await
    }

    async fn send_control(&self, packet: &StreamPacket) -> Result<(), StreamError> {
        self.connection.send_control(packet.as_bytes()).await
    }

    async fn on_viewer_joined(&self, viewer_id: &str) {
        // In SFU mode, the server handles viewer management.
        // The host receives a member_joined signaling message.
        // Keyframe is triggered when the SFU notifies us, not here.
        log::debug!("SFU: viewer joined notification for {}", viewer_id);
    }

    async fn on_viewer_left(&self, viewer_id: &str) {
        log::debug!("SFU: viewer left notification for {}", viewer_id);
    }
}
```

### 8.2 SfuConnection

Manages the WebSocket signaling + WebRTC PeerConnection + DataChannels:

```rust
// mello-core/src/transport/sfu_connection.rs

pub struct SfuConnection {
    ws: tokio::sync::Mutex<WebSocketStream>,
    peer_connection: Arc<PeerConnection>,  // libdatachannel wrapper
    media_channel: Arc<DataChannel>,
    control_channel: Arc<DataChannel>,
    event_tx: mpsc::Sender<SfuEvent>,
}

pub enum SfuEvent {
    MemberJoined { user_id: String, role: String },
    MemberLeft { user_id: String },
    MediaPacket { data: Vec<u8> },       // received from SFU (viewer side)
    ControlPacket { data: Vec<u8> },     // received from SFU (host side: loss reports)
    Disconnected { reason: String },
}

impl SfuConnection {
    pub async fn connect(endpoint: &str, token: &str) -> Result<Self, StreamError> {
        // 1. WebSocket connect with token
        let url = format!("{}?token={}", endpoint, token);
        let ws = connect_ws(&url).await?;

        // 2. Wait for welcome message
        let welcome = ws.recv_json::<SignalingMessage>().await?;
        assert_eq!(welcome.msg_type, "welcome");

        // 3. Create PeerConnection (libdatachannel)
        let pc = PeerConnection::new()?;

        // 4. Create DataChannels
        let media_dc = pc.create_data_channel("media", DataChannelConfig {
            ordered: false,
            max_retransmits: Some(0),
        })?;
        let control_dc = pc.create_data_channel("control", DataChannelConfig {
            ordered: true,  // control messages are reliable
            max_retransmits: None,
        })?;

        // 5. Create and send SDP offer
        let offer = pc.create_offer().await?;
        pc.set_local_description(&offer).await?;
        ws.send_json(&SignalingMessage::offer(&offer)).await?;

        // 6. Receive SDP answer
        let answer = ws.recv_json::<SignalingMessage>().await?;
        pc.set_remote_description(&answer.sdp()).await?;

        // 7. ICE candidate exchange (typically 1 round with ICE-lite server)
        // Spawn background task for ongoing ICE candidates
        // ...

        Ok(Self { ws, peer_connection: pc, media_channel: media_dc, control_channel: control_dc, event_tx })
    }

    pub async fn send_media(&self, data: &[u8]) -> Result<(), StreamError> {
        self.media_channel.send_unreliable(data)
            .map_err(|e| StreamError::SfuSendFailed(e.to_string()))
    }

    pub async fn send_control(&self, data: &[u8]) -> Result<(), StreamError> {
        self.control_channel.send(data)
            .map_err(|e| StreamError::SfuSendFailed(e.to_string()))
    }

    /// Join a session (called after connect)
    pub async fn join_stream(&self, session_id: &str, role: &str) -> Result<SessionInfo, StreamError> {
        let msg = SignalingMessage::join_stream(session_id, role);
        self.ws.lock().await.send_json(&msg).await?;
        let resp = self.ws.lock().await.recv_json::<SignalingMessage>().await?;
        // Parse joined response
        Ok(resp.into_session_info()?)
    }

    pub async fn join_voice(&self, crew_id: &str, channel_id: &str) -> Result<SessionInfo, StreamError> {
        let msg = SignalingMessage::join_voice(crew_id, channel_id);
        self.ws.lock().await.send_json(&msg).await?;
        let resp = self.ws.lock().await.recv_json::<SignalingMessage>().await?;
        Ok(resp.into_session_info()?)
    }
}
```

### 8.3 Voice Manager SFU Integration

The `VoiceManager` (spec 13 §6.4) gains SFU awareness. Voice mode is determined at join time based on the `voice_join` RPC response — the VoiceManager is always in one mode for a given crew, never transitions between modes mid-session.

```rust
// mello-core/src/voice/manager.rs — updated

pub struct VoiceManager {
    libmello: *mut MelloContext,
    event_tx: mpsc::Sender<Event>,

    connected_crew: Option<CrewId>,
    connected_channel: Option<ChannelId>,

    // Voice connection mode — set at join time, never changes mid-session
    mode: VoiceMode,
    peers: HashMap<MemberId, PeerHandle>,         // P2P mode only
    sfu_connection: Option<Arc<SfuConnection>>,    // SFU mode only
}

enum VoiceMode {
    Disconnected,
    P2P,
    SFU,
}

impl VoiceManager {
    pub async fn join_channel(
        &mut self,
        nakama: &NakamaClient,
        crew_id: &str,
        channel_id: &str,
    ) -> Result<(), Error> {
        // If already in a channel, leave first
        if self.connected_channel.is_some() {
            self.leave_current(nakama).await?;
        }

        // RPC: voice_join — server decides P2P or SFU based on crew entitlement
        let resp = nakama.rpc("voice_join", &VoiceJoinRequest {
            crew_id: crew_id.to_string(),
            channel_id: channel_id.to_string(),
        }).await?;

        self.connected_crew = Some(crew_id.to_string());
        self.connected_channel = Some(channel_id.to_string());

        match resp.mode.as_str() {
            "sfu" => {
                // Connect to SFU — all voice packets routed through server
                let conn = SfuConnection::connect(
                    &resp.sfu_endpoint,
                    &resp.sfu_token,
                ).await?;
                conn.join_voice(crew_id, channel_id).await?;

                self.sfu_connection = Some(Arc::new(conn));
                self.mode = VoiceMode::SFU;

                self.event_tx.send(Event::VoiceConnected {
                    crew_id: crew_id.to_string(),
                    channel_id: channel_id.to_string(),
                }).await.ok();
            }
            _ => {
                // P2P mesh — existing behaviour from spec 13
                self.mode = VoiceMode::P2P;

                for member in &resp.members {
                    self.connect_peer(nakama, &member.user_id).await?;
                }

                self.event_tx.send(Event::VoiceConnected {
                    crew_id: crew_id.to_string(),
                    channel_id: channel_id.to_string(),
                }).await.ok();
            }
        }

        Ok(())
    }

    pub async fn leave_current(&mut self, nakama: &NakamaClient) -> Result<(), Error> {
        match self.mode {
            VoiceMode::SFU => {
                if let Some(conn) = self.sfu_connection.take() {
                    conn.leave().await.ok();
                }
            }
            VoiceMode::P2P => {
                for (_, peer) in self.peers.drain() {
                    peer.close().await;
                }
            }
            VoiceMode::Disconnected => {}
        }
        self.mode = VoiceMode::Disconnected;
        self.connected_crew = None;
        self.connected_channel = None;
        Ok(())
    }
}
```

---

## 9. SFU Server Implementation

### 9.1 Server Entry Point

```go
// cmd/sfu/main.go

package main

import (
    "flag"
    "log"
    "os"
    "os/signal"
    "syscall"

    "github.com/mollohq/mello-sfu/internal/server"
)

func main() {
    addr := flag.String("addr", ":8443", "WebSocket listen address")
    publicIP := flag.String("public-ip", "", "Public IP for ICE-lite (required)")
    certFile := flag.String("cert", "", "TLS certificate file")
    keyFile := flag.String("key", "", "TLS private key file")
    jwtPubKey := flag.String("jwt-pub-key", "", "Path to JWT RS256 public key (PEM)")
    region := flag.String("region", "eu-west", "SFU region identifier")
    flag.Parse()

    cfg := server.Config{
        Addr:      *addr,
        PublicIP:  *publicIP,
        CertFile:  *certFile,
        KeyFile:   *keyFile,
        JWTPubKey: *jwtPubKey,
        Region:    *region,
    }

    srv, err := server.New(cfg)
    if err != nil {
        log.Fatalf("Failed to create server: %v", err)
    }

    go srv.Run()

    // Graceful shutdown
    sig := make(chan os.Signal, 1)
    signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM)
    <-sig
    log.Println("Shutting down...")
    srv.Shutdown()
}
```

### 9.2 Core Server Types

```go
// internal/server/server.go

package server

type Server struct {
    config      Config
    sessions    *SessionStore
    auth        *JWTVerifier
    webrtcAPI   *webrtc.API
}

func New(cfg Config) (*Server, error) {
    // Create Pion SettingEngine with ICE-lite
    se := webrtc.SettingEngine{}
    se.SetLite(true)
    se.SetNAT1To1IPs([]string{cfg.PublicIP}, webrtc.ICECandidateTypeHost)

    api := webrtc.NewAPI(webrtc.WithSettingEngine(se))

    auth, err := NewJWTVerifier(cfg.JWTPubKey)
    if err != nil {
        return nil, err
    }

    return &Server{
        config:   cfg,
        sessions: NewSessionStore(),
        auth:     auth,
        webrtcAPI: api,
    }, nil
}
```

### 9.3 Session Store

```go
// internal/server/session_store.go

package server

import (
    "sync"
    "time"
)

type SessionStore struct {
    mu       sync.RWMutex
    sessions map[string]Session  // sessionID → Session
}

type Session interface {
    ID() string
    Type() string  // "stream" or "voice"
    AddPeer(peer *Peer) error
    RemovePeer(userID string)
    MemberCount() int
    CreatedAt() time.Time
}

type StreamSession struct {
    id        string
    crewID    string
    host      *Peer
    viewers   *PeerSet    // lock-free concurrent set
    createdAt time.Time
}

type VoiceSession struct {
    id        string
    crewID    string
    channelID string
    members   *PeerSet
    createdAt time.Time
}

func (s *SessionStore) GetOrCreate(id string, factory func() Session) Session {
    s.mu.Lock()
    defer s.mu.Unlock()
    if sess, ok := s.sessions[id]; ok {
        return sess
    }
    sess := factory()
    s.sessions[id] = sess
    return sess
}
```

### 9.4 Peer Type

```go
// internal/server/peer.go

package server

type Peer struct {
    userID          string
    role            string   // "host", "viewer", "member"
    ws              *websocket.Conn
    pc              *webrtc.PeerConnection
    mediaChannel    *webrtc.DataChannel
    controlChannel  *webrtc.DataChannel
    session         Session
    sendBuffer      *RingBuffer  // bounded send buffer
}

func (p *Peer) SendMedia(data []byte) {
    if p.mediaChannel == nil || p.mediaChannel.ReadyState() != webrtc.DataChannelStateOpen {
        return
    }
    // Non-blocking send — drop if buffer full
    if p.sendBuffer.Available() < len(data) {
        // Buffer full — drop packet (viewer will use FEC/IDR recovery)
        return
    }
    p.mediaChannel.Send(data)
}

func (p *Peer) SendControl(data []byte) {
    if p.controlChannel == nil || p.controlChannel.ReadyState() != webrtc.DataChannelStateOpen {
        return
    }
    p.controlChannel.Send(data)
}
```

### 9.5 Stream Session Forwarding

```go
// internal/server/stream_session.go

func (s *StreamSession) AddPeer(peer *Peer) error {
    switch peer.role {
    case "host":
        if s.host != nil {
            return ErrHostAlreadyConnected
        }
        s.host = peer
        // Register media callback — forward all host media to viewers
        peer.mediaChannel.OnMessage(func(msg webrtc.DataChannelMessage) {
            s.viewers.ForEach(func(viewer *Peer) {
                viewer.SendMedia(msg.Data)
            })
        })
        // Register control callback — host control messages are ignored by SFU
        // (they are host→viewer, already handled by the DataChannel routing)
        return nil

    case "viewer":
        s.viewers.Add(peer)
        // Register control callback — viewer control packets go to host
        peer.controlChannel.OnMessage(func(msg webrtc.DataChannelMessage) {
            if s.host != nil {
                s.host.SendControl(msg.Data)
            }
        })
        // Notify host that a new viewer joined (triggers keyframe)
        if s.host != nil {
            s.host.sendSignaling("member_joined", map[string]string{
                "user_id": peer.userID,
                "role":    "viewer",
            })
        }
        return nil

    default:
        return ErrInvalidRole
    }
}
```

### 9.6 Voice Session Forwarding

```go
// internal/server/voice_session.go

func (s *VoiceSession) AddPeer(peer *Peer) error {
    // Notify existing members
    s.members.ForEach(func(existing *Peer) {
        existing.sendSignaling("member_joined", map[string]string{
            "user_id": peer.userID,
            "role":    "member",
        })
    })

    s.members.Add(peer)

    // Register media callback — forward audio to all OTHER members
    peer.mediaChannel.OnMessage(func(msg webrtc.DataChannelMessage) {
        s.members.ForEach(func(other *Peer) {
            if other.userID != peer.userID {
                other.SendMedia(msg.Data)
            }
        })
    })

    return nil
}
```

### 9.7 PeerSet (Lock-Free Concurrent Set)

```go
// internal/server/peer_set.go

package server

import (
    "sync/atomic"
    "unsafe"
)

// PeerSet is a copy-on-write set optimised for frequent reads (forwarding loop)
// and infrequent writes (member join/leave).
type PeerSet struct {
    peers atomic.Pointer[[]Peer]
}

func (ps *PeerSet) Snapshot() []*Peer {
    p := ps.peers.Load()
    if p == nil {
        return nil
    }
    return *p
}

func (ps *PeerSet) ForEach(fn func(*Peer)) {
    snapshot := ps.Snapshot()
    for i := range snapshot {
        fn(&snapshot[i])
    }
}

func (ps *PeerSet) Add(peer *Peer) {
    // Copy-on-write: create new slice with the added peer
    for {
        old := ps.peers.Load()
        var newSlice []*Peer
        if old != nil {
            newSlice = make([]*Peer, len(*old)+1)
            copy(newSlice, *old)
            newSlice[len(*old)] = peer
        } else {
            newSlice = []*Peer{peer}
        }
        if ps.peers.CompareAndSwap(old, &newSlice) {
            return
        }
    }
}

// Remove uses the same copy-on-write pattern
func (ps *PeerSet) Remove(userID string) { ... }
```

The copy-on-write pattern ensures the forwarding loop (hot path) never takes a lock. Writes (join/leave) are rare relative to packet forwarding and can afford the allocation.

---

## 10. Deployment

### 10.1 Infrastructure

```
                    ┌──────────────────────┐
                    │      Cloudflare      │
                    │   (DNS + TLS edge)   │
                    └──────┬───────┬───────┘
                           │       │
              ┌────────────┘       └────────────┐
              ▼                                  ▼
    ┌──────────────────┐              ┌──────────────────┐
    │  sfu-eu.mello.app│              │  sfu-us.mello.app│
    │                  │              │                  │
    │  GCP Compute     │              │  GCP Compute     │
    │  Engine          │              │  Engine          │
    │  europe-west3    │              │  us-east4        │
    │  (Frankfurt)     │              │  (Virginia)      │
    │                  │              │                  │
    │  e2-medium       │              │  e2-medium       │
    │  2 vCPU / 4GB    │              │  2 vCPU / 4GB    │
    │                  │              │                  │
    │  ┌────────────┐  │              │  ┌────────────┐  │
    │  │  coturn     │  │              │  │  coturn     │  │
    │  │  3478 UDP   │  │              │  │  3478 UDP   │  │
    │  │  3478 TCP   │  │              │  │  3478 TCP   │  │
    │  │  5349 TLS   │  │              │  │  5349 TLS   │  │
    │  └────────────┘  │              │  └────────────┘  │
    │  ┌────────────┐  │              │  ┌────────────┐  │
    │  │  mello-sfu  │  │              │  │  mello-sfu  │  │
    │  │  8443 WSS   │  │              │  │  8443 WSS   │  │
    │  │  10000-10100│  │              │  │  10000-10100│  │
    │  │  UDP/WebRTC │  │              │  │  UDP/WebRTC │  │
    │  └────────────┘  │              │  └────────────┘  │
    └──────────────────┘              └──────────────────┘
```

### 10.2 TURN Server (coturn) Co-Location

Each SFU VM also runs [coturn](https://github.com/coturn/coturn) as the TURN relay for P2P connections. Coturn is deployed first (before the SFU is built) so that P2P voice and streaming have reliable NAT traversal from day one.

**Why co-locate:**
- Both services are stateless UDP packet routers with identical networking needs (public IP, open UDP ports)
- Zero extra cost, ~100 MB additional RAM
- At beta scale, the workloads don't compete for bandwidth
- If TURN traffic grows to compete with SFU traffic, split to a dedicated VM (config change in Nakama, not a code change)

**TURN details:**

| Setting | Value |
|---|---|
| Software | coturn 4.6+ |
| Auth | `use-auth-secret` (HMAC-SHA1 time-limited credentials) |
| Realm | `mello.app` |
| UDP port | 3478 |
| TCP port | 3478 |
| TLS port | 5349 |
| Relay port range | 49152-65535 |
| Per-user quota | 12 allocations |
| Total quota | 1200 allocations |

**Credential flow:** The client calls the `get_ice_servers` Nakama RPC (spec 04 §6.3), which generates time-limited HMAC-SHA1 credentials using the shared `TURN_SECRET`. The same secret is configured in both coturn and Nakama. Credentials expire after 24 hours.

**Deployment:** coturn is installed via the VM startup script (`deploy-instance.sh`) and runs as a systemd service. The TURN secret is stored in GCP instance metadata and read at startup. See `deploy-instance.sh` for the full provisioning script.

### 10.2 Why GCP

GCP Compute Engine gives the SFU everything it needs:

- **Dedicated public IP** with full control over firewall rules (UDP port range)
- **Low-latency networking** — GCP's premium tier uses Google's backbone between regions
- **Already in the billing stack** — no new vendor relationship
- **Clear upgrade path** — start with Compute Engine VMs, move to GKE if horizontal scaling demands it

The SFU and Nakama (on Render.com) never communicate directly — auth is self-contained JWT verification. Split hosting is clean.

### 10.3 GCP Setup

#### VM Configuration

```
Machine type:   e2-medium (2 vCPU, 4 GB RAM)
OS:             Container-Optimized OS (cos-stable)
Disk:           10 GB SSD (minimal — SFU is stateless)
Network:        Premium tier
IP:             Static external IPv4
```

Estimated cost: ~$25/month per region ($50/month total for EU + US).

#### Firewall Rules

These rules are created by `deploy-instance.sh` and cover both coturn and the SFU:

```
# TURN relay (UDP) + SFU WebRTC media
gcloud compute firewall-rules create mello-sfu-turn-udp \
    --allow udp:3478,udp:10000-10100,udp:49152-65535 \
    --target-tags mello-sfu \
    --description "Mello: TURN relay (UDP) + SFU WebRTC media"

# TURN (TCP/TLS) + SFU WebSocket signaling
gcloud compute firewall-rules create mello-sfu-turn-tcp \
    --allow tcp:3478,tcp:5349,tcp:8443 \
    --target-tags mello-sfu \
    --description "Mello: TURN (TCP/TLS) + SFU WebSocket signaling"

# Health check (GCP load balancer probes)
gcloud compute firewall-rules create mello-sfu-health \
    --allow tcp:8080 \
    --source-ranges 130.211.0.0/22,35.191.0.0/16 \
    --target-tags mello-sfu \
    --description "Mello: GCP health check probes"
```

#### Container Deployment

The VM runs the Docker image directly via Container-Optimized OS:

```yaml
# container-manifest.yaml (passed to VM metadata)
spec:
  containers:
    - name: mello-sfu
      image: europe-west3-docker.pkg.dev/mello-prod/sfu/mello-sfu:latest
      ports:
        - containerPort: 8443
          protocol: TCP
        - containerPort: 10000-10100
          protocol: UDP
      env:
        - name: SFU_ADDR
          value: ":8443"
        - name: SFU_PUBLIC_IP
          value: "<STATIC_IP>"
        - name: SFU_REGION
          value: "eu-west"
        - name: SFU_JWT_PUB_KEY
          value: "/certs/jwt_pub.pem"
        - name: SFU_MAX_SESSIONS
          value: "500"
        - name: SFU_LOG_LEVEL
          value: "info"
      volumeMounts:
        - name: certs
          mountPath: /certs
          readOnly: true
  volumes:
    - name: certs
      hostPath:
        path: /etc/sfu-certs
```

Images are pushed to GCP Artifact Registry. Deploy updates by pulling the new image and restarting the container.

### 10.4 Docker Image

```dockerfile
# Dockerfile
FROM golang:1.21-alpine AS builder
WORKDIR /build
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o /sfu ./cmd/sfu

FROM alpine:3.19
RUN apk add --no-cache ca-certificates
COPY --from=builder /sfu /usr/local/bin/sfu

EXPOSE 8443/tcp
EXPOSE 10000-10100/udp

ENTRYPOINT ["sfu"]
```

### 10.5 Configuration (Environment Variables)

```
# Server
SFU_ADDR=:8443
SFU_PUBLIC_IP=<static external IP>
SFU_CERT_FILE=/certs/cert.pem
SFU_KEY_FILE=/certs/key.pem
SFU_JWT_PUB_KEY=/certs/jwt_pub.pem
SFU_REGION=eu-west
SFU_MAX_SESSIONS=500
SFU_MAX_PEERS_PER_SESSION=100
SFU_LOG_LEVEL=info

# Admin dashboard
SFU_ADMIN_PASSWORD=<strong random password>
SFU_ADMIN_IPS=155.4.130.18,10.0.0.0/8

# Aggregate view (sibling SFU instances)
SFU_PEERS=sfu-eu.mello.app:8080,sfu-us.mello.app:8080
```

### 10.6 TLS

Two options for TLS termination:

**Option A: Cloudflare proxy (recommended for beta)**
- Cloudflare terminates TLS for `sfu-eu.mello.app`
- Cloudflare → SFU connection uses origin certificate
- Pro: Free, automatic cert renewal, DDoS protection
- Con: Adds ~1-5ms latency (Cloudflare edge hop) — acceptable for signaling (WebSocket), not in the media path (WebRTC is direct UDP)

**Option B: Let's Encrypt on the VM**
- `certbot` runs on the VM and provisions certs directly
- Pro: No intermediary for signaling
- Con: Manual renewal management

Note: WebRTC media (UDP DataChannels) uses DTLS, which is negotiated during the WebRTC handshake and does NOT go through Cloudflare. Only the WebSocket signaling channel is proxied. Media latency is unaffected by the TLS approach.

---

## 11. Capacity and Limits

### 11.1 Per-Session Limits

| Session Type | Limit | Value | Notes |
|---|---|---|---|
| Stream | Max viewers | 100 | Configurable; 100 is generous for beta |
| Voice | Max members | 50 | Configurable; keeps forwarding fan-out manageable |

### 11.2 Per-Server Limits

| Metric | Target | Notes |
|---|---|---|
| Concurrent sessions | 500 | Mix of stream and voice |
| Concurrent peers | 5,000 | Across all sessions |
| Bandwidth per server | 4 Gbps | GCP e2-medium network cap; sufficient for beta |
| Memory per peer | ~2 KB | DataChannel buffers + session metadata |

### 11.3 Bandwidth Math

A single 1080p60 stream at 12 Mbps with 50 viewers:
- Host → SFU: 12 Mbps
- SFU → 50 viewers: 50 × 12 = 600 Mbps
- Total: 612 Mbps for one stream

On a GCP `e2-medium` VM (up to 4 Gbps egress), this comfortably handles several concurrent streams. For beta with mostly smaller audiences, a single VM per region is sufficient. Scaling is horizontal — add more SFU instances behind a session-aware router.

---

## 12. Observability and Admin Dashboard

Each SFU instance serves a built-in admin dashboard and exposes structured logging, live session data, and an aggregate view of all SFU instances. No external monitoring infrastructure is required for beta.

### 12.1 Architecture

```
                    ┌──────────────────────────────────────────────┐
                    │           SFU Server (:8080)                  │
                    │                                              │
                    │  /health              Public, no auth        │
                    │  /admin               Dashboard (HTML/JS)    │
                    │  /admin/api/overview   Server stats JSON     │
                    │  /admin/api/sessions   Active sessions JSON  │
                    │  /admin/api/session/:id Session detail JSON  │
                    │  /admin/api/logs       Log ring buffer JSON  │
                    │  /admin/api/peers      Aggregate view JSON   │
                    │                                              │
                    │  All /admin/* routes require:                │
                    │    1. Client IP in allowlist                  │
                    │    2. Basic Auth password                     │
                    └──────────────────────────────────────────────┘
```

### 12.2 Authentication

Admin routes use two layers of protection:

```go
// internal/server/admin_auth.go

type AdminAuth struct {
    password    string    // from SFU_ADMIN_PASSWORD env var
    allowedIPs  []net.IP  // from SFU_ADMIN_IPS env var (comma-separated)
}

func (a *AdminAuth) Middleware(next http.Handler) http.Handler {
    return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
        // 1. IP allowlist check
        clientIP := extractClientIP(r)
        if !a.isAllowed(clientIP) {
            http.Error(w, "Forbidden", http.StatusForbidden)
            return
        }

        // 2. Basic Auth password check
        _, password, ok := r.BasicAuth()
        if !ok || subtle.ConstantTimeCompare([]byte(password), []byte(a.password)) != 1 {
            w.Header().Set("WWW-Authenticate", `Basic realm="mello-sfu"`)
            http.Error(w, "Unauthorized", http.StatusUnauthorized)
            return
        }

        next.ServeHTTP(w, r)
    })
}
```

**Configuration:**

```
SFU_ADMIN_PASSWORD=<strong random password>
SFU_ADMIN_IPS=155.4.130.18,10.0.0.0/8    # your home IP + GCP internal range
```

If `SFU_ADMIN_IPS` is empty, IP allowlisting is disabled (password-only). This is useful for initial setup when you don't know your IP yet.

### 12.3 Health Endpoint (Public)

Unchanged from previous design, no auth required. Used by GCP health checks and the aggregate view:

```
GET /health

200 OK
{
  "status": "ok",
  "server_id": "sfu-eu-01",
  "region": "eu-west",
  "version": "0.1.0",
  "uptime_seconds": 86400,
  "sessions": {
    "stream_active": 3,
    "stream_draining": 0,
    "voice_active": 8,
    "voice_draining": 1
  },
  "peers": {
    "total": 47,
    "hosts": 3,
    "viewers": 22,
    "voice_members": 22
  },
  "bandwidth_mbps": {
    "inbound": 42.3,
    "outbound": 487.1
  },
  "system": {
    "cpu_percent": 12.4,
    "memory_mb": 186,
    "goroutines": 312
  }
}
```

### 12.4 Admin API Endpoints

All endpoints return JSON. The dashboard UI consumes these.

#### `GET /admin/api/overview`

Server overview with time-series snapshots for sparkline charts:

```json
{
  "server": {
    "id": "sfu-eu-01",
    "region": "eu-west",
    "version": "0.1.0",
    "uptime_seconds": 86400,
    "started_at": "2026-03-22T10:15:27Z"
  },
  "now": {
    "sessions": 11,
    "peers": 47,
    "bandwidth_in_mbps": 42.3,
    "bandwidth_out_mbps": 487.1,
    "packets_forwarded_sec": 14302,
    "cpu_percent": 12.4,
    "memory_mb": 186
  },
  "history": [
    { "ts": "2026-03-22T14:20:00Z", "peers": 44, "bw_out": 412.0 },
    { "ts": "2026-03-22T14:20:10Z", "peers": 45, "bw_out": 433.2 },
    { "ts": "2026-03-22T14:20:20Z", "peers": 47, "bw_out": 487.1 }
  ],
  "totals_since_start": {
    "sessions_created": 142,
    "peers_connected": 891,
    "peers_force_disconnected": 3,
    "bytes_forwarded": 48291048576,
    "packets_forwarded": 12948201
  }
}
```

`history` contains the last 360 snapshots at 10-second intervals (1 hour of data). Kept in a fixed-size ring buffer in memory.

#### `GET /admin/api/sessions`

List all active sessions:

```json
{
  "sessions": [
    {
      "id": "str_abc123",
      "type": "stream",
      "state": "active",
      "crew_id": "crew_xyz",
      "created_at": "2026-03-22T14:10:00Z",
      "duration_seconds": 630,
      "host": {
        "user_id": "user_a",
        "connected_at": "2026-03-22T14:10:01Z",
        "packets_received": 48201,
        "bytes_received": 142948576
      },
      "viewer_count": 4,
      "bandwidth_out_mbps": 48.2,
      "packets_forwarded": 192804
    },
    {
      "id": "voice:crew_xyz:ch_general",
      "type": "voice",
      "state": "active",
      "crew_id": "crew_xyz",
      "channel_id": "ch_general",
      "created_at": "2026-03-22T13:45:00Z",
      "duration_seconds": 2130,
      "member_count": 5,
      "bandwidth_out_mbps": 3.2,
      "packets_forwarded": 64201
    }
  ]
}
```

#### `GET /admin/api/session/:id`

Drill into a single session:

```json
{
  "id": "str_abc123",
  "type": "stream",
  "state": "active",
  "crew_id": "crew_xyz",
  "created_at": "2026-03-22T14:10:00Z",
  "duration_seconds": 630,
  "peers": [
    {
      "user_id": "user_a",
      "role": "host",
      "connected_at": "2026-03-22T14:10:01Z",
      "packets_sent": 48201,
      "packets_received": 12,
      "bytes_sent": 142948576,
      "buffer_usage_percent": 0,
      "ice_state": "connected",
      "remote_ip": "155.4.130.18"
    },
    {
      "user_id": "user_b",
      "role": "viewer",
      "connected_at": "2026-03-22T14:10:15Z",
      "packets_sent": 12,
      "packets_received": 47800,
      "bytes_received": 141800000,
      "buffer_usage_percent": 2,
      "packets_dropped": 0,
      "ice_state": "connected",
      "remote_ip": "203.0.113.42"
    }
  ],
  "recent_events": [
    { "ts": "2026-03-22T14:20:30Z", "event": "peer_joined", "user_id": "user_d", "role": "viewer" },
    { "ts": "2026-03-22T14:18:12Z", "event": "peer_left", "user_id": "user_c", "reason": "disconnect" }
  ]
}
```

#### `GET /admin/api/logs?level=warn&session=str_abc&limit=100`

Query the in-memory log ring buffer:

```json
{
  "entries": [
    {
      "ts": "2026-03-22T14:20:51Z",
      "level": "warn",
      "event": "buffer_full",
      "session": "str_abc123",
      "user": "user_c",
      "data": { "dropped": 47, "duration_ms": 1200 }
    },
    {
      "ts": "2026-03-22T14:20:50Z",
      "level": "info",
      "event": "peer_joined",
      "session": "str_abc123",
      "user": "user_d",
      "data": { "role": "viewer", "viewers": 4 }
    }
  ],
  "total": 847,
  "filtered": 2
}
```

**Query parameters:**

| Param | Description | Default |
|---|---|---|
| `level` | Minimum level: `debug`, `info`, `warn`, `error` | `info` |
| `session` | Filter by session ID | (all) |
| `user` | Filter by user ID | (all) |
| `event` | Filter by event type | (all) |
| `limit` | Max entries to return | 100 |
| `before` | Entries before this ISO timestamp | (latest) |
| `after` | Entries at or after this ISO timestamp | (earliest) |
| `days` | Shorthand for `after` — entries from the last N days (converted to `after` server-side) | (all) |

#### `GET /admin/api/peers`

Aggregate view across all SFU instances. This SFU polls its siblings:

```json
{
  "instances": [
    {
      "server_id": "sfu-eu-01",
      "region": "eu-west",
      "status": "ok",
      "url": "https://sfu-eu.mello.app:8080",
      "sessions": 11,
      "peers": 47,
      "bandwidth_out_mbps": 487.1,
      "uptime_seconds": 86400,
      "self": true
    },
    {
      "server_id": "sfu-us-01",
      "region": "us-east",
      "status": "ok",
      "url": "https://sfu-us.mello.app:8080",
      "sessions": 6,
      "peers": 23,
      "bandwidth_out_mbps": 201.4,
      "uptime_seconds": 72100,
      "self": false
    }
  ],
  "totals": {
    "instances": 2,
    "sessions": 17,
    "peers": 70,
    "bandwidth_out_mbps": 688.5
  }
}
```

Populated by polling each sibling's `/health` endpoint. Sibling list from `SFU_PEERS` env var.

### 12.5 Structured Logging

All log output is JSON to stdout. GCP Cloud Logging indexes it automatically.

```go
// internal/server/logger.go

type StructuredLogger struct {
    ring    *LogRingBuffer  // in-memory, 10,000 entries
    output  io.Writer       // os.Stdout
}

type LogEntry struct {
    Timestamp string                 `json:"ts"`
    Level     string                 `json:"level"`
    Event     string                 `json:"event"`
    SessionID string                 `json:"session,omitempty"`
    UserID    string                 `json:"user,omitempty"`
    Data      map[string]interface{} `json:"data,omitempty"`
}

func (l *StructuredLogger) Log(level, event string, fields ...Field) {
    entry := LogEntry{
        Timestamp: time.Now().UTC().Format(time.RFC3339),
        Level:     level,
        Event:     event,
    }
    for _, f := range fields {
        f.Apply(&entry)
    }

    // Write to stdout (GCP Cloud Logging picks this up)
    json.NewEncoder(l.output).Encode(entry)

    // Also push to ring buffer (admin dashboard reads this)
    l.ring.Push(entry)
}
```

#### Log Ring Buffer

```go
// internal/server/log_ring.go

const LogRingSize = 10_000

type LogRingBuffer struct {
    mu      sync.RWMutex
    entries [LogRingSize]LogEntry
    head    int
    count   int
}

func (r *LogRingBuffer) Push(entry LogEntry) {
    r.mu.Lock()
    defer r.mu.Unlock()
    r.entries[r.head] = entry
    r.head = (r.head + 1) % LogRingSize
    if r.count < LogRingSize {
        r.count++
    }
}

func (r *LogRingBuffer) Query(filter LogFilter) []LogEntry {
    r.mu.RLock()
    defer r.mu.RUnlock()
    // Iterate backwards from head, apply filter, return up to limit
    ...
}
```

Memory: 10,000 entries at ~500 bytes average = ~5 MB. Negligible.

#### What Gets Logged

| Event | Level | Fields | When |
|---|---|---|---|
| `server_started` | info | region, version, public_ip | Startup |
| `server_stopping` | info | uptime, sessions_served | Shutdown |
| `session_created` | info | session, type, crew_id | First peer joins |
| `session_closed` | info | session, duration, packets_fwd | Last peer leaves + drain |
| `peer_joined` | info | session, user, role, count | Peer connects |
| `peer_left` | info | session, user, reason, count | Peer disconnects |
| `buffer_full` | warn | session, user, dropped, duration_ms | Send buffer overflow |
| `peer_force_disconnect` | warn | session, user, reason | Slow peer kicked |
| `session_draining` | info | session, reason | Session enters drain state |
| `auth_failed` | warn | remote_ip, reason | JWT validation failure |
| `webrtc_failed` | error | session, user, error | PeerConnection error |
| `datachannel_failed` | error | session, user, channel, error | DataChannel error |
| `stats` | debug | session, packets_fwd, bw_out, peers | Every 10s per session |

### 12.6 Metrics Collection (Internal)

The server collects metrics in memory for the dashboard. No Prometheus, no external time-series DB.

```go
// internal/server/metrics.go

type Metrics struct {
    // Counters (monotonic since start)
    SessionsCreated      atomic.Int64
    PeersConnected       atomic.Int64
    PeersForceDisconnected atomic.Int64
    PacketsForwarded     atomic.Int64
    BytesForwarded       atomic.Int64

    // Gauges (current value)
    ActiveSessions       atomic.Int32
    ActivePeers          atomic.Int32

    // Rate tracking (for bandwidth calculation)
    bandwidthIn          *RateCounter  // bytes/sec, 1-second buckets
    bandwidthOut         *RateCounter

    // History (for sparklines on dashboard)
    history              *MetricsRing  // 360 snapshots, 10s interval = 1 hour
}

type MetricsSnapshot struct {
    Timestamp    time.Time `json:"ts"`
    Peers        int       `json:"peers"`
    Sessions     int       `json:"sessions"`
    BandwidthOut float64   `json:"bw_out"`
    PacketsFwd   int64     `json:"pkts_fwd"`
    CPU          float64   `json:"cpu"`
    MemoryMB     float64   `json:"mem"`
}
```

A background goroutine takes a snapshot every 10 seconds and pushes it to the ring.

### 12.7 Admin Dashboard UI

The dashboard is a single HTML file with embedded CSS and JavaScript, served from `/admin`. No build step, no npm, no webpack. Just Go's `embed` directive.

```go
//go:embed admin/dashboard.html
var dashboardHTML []byte

func (s *Server) handleAdmin(w http.ResponseWriter, r *http.Request) {
    w.Header().Set("Content-Type", "text/html; charset=utf-8")
    w.Write(dashboardHTML)
}
```

#### Layout

The dashboard has three tabs:

**Tab 1: Overview**
- Server info card (ID, region, version, uptime)
- Live counters (sessions, peers, bandwidth in/out, packets/sec)
- Sparkline charts (peers over time, bandwidth over time, last 1 hour)
- Totals since start (sessions served, peers connected, data forwarded)

**Tab 2: Sessions**
- Table of all active sessions
- Columns: ID, type, crew, members, duration, bandwidth, packets
- Click a row to expand the peer-level detail (buffer health, packets, IP, ICE state)
- Color-coded: green = healthy, yellow = buffer pressure, red = peer being kicked

**Tab 3: Logs**
- Level filter (debug/info/warn/error toggle buttons)
- Session ID and user ID filter inputs
- Export buttons: "Export 1d", "Export 3d", "Export 7d" — fetches logs for that window at debug level (limit 10,000) and downloads as a `.tsv` file
- Auto-scrolling log stream (polls `/admin/api/logs` every 2 seconds)
- Each log entry shows full local date+time (e.g. `2026-03-22 14:20:51`), user_id extracted from `data.user` (shown inline before session ID), level badge, event, and data

**Aggregate Banner (top of page)**
- Polls `/admin/api/peers` every 30 seconds
- Shows all SFU instances in a horizontal bar: region, status dot (green/red), peers, bandwidth
- Clicking an instance opens its dashboard in a new tab

#### Visual Style

Dark theme matching Mello's aesthetic:

| Element | Value |
|---|---|
| Background | `#0a0a0f` (near-black with slight blue) |
| Surface | `#14141f` (cards, table rows) |
| Surface hover | `#1e1e2e` |
| Border | `#2a2a3a` |
| Text primary | `#e0e0e8` |
| Text secondary | `#888898` |
| Accent | `#ff2d55` (mello hot pink-red) |
| Success | `#22c55e` |
| Warning | `#eab308` |
| Error | `#ef4444` |
| Font | `"Inter", system-ui, sans-serif` |
| Mono | `"JetBrains Mono", "Fira Code", monospace` |

Auto-refresh interval: overview and sessions every 5 seconds, logs every 2 seconds. All via `fetch()` to the admin API endpoints.

### 12.8 Aggregate View (Multi-Instance)

Each SFU instance knows about its siblings via an env var:

```
SFU_PEERS=sfu-eu.mello.app:8080,sfu-us.mello.app:8080
```

The aggregate view works by polling each sibling's `/health` endpoint (public, no auth required). The dashboard's aggregate banner and the `/admin/api/peers` endpoint both use this data.

```go
// internal/server/peers.go

type PeerPoller struct {
    peers    []string            // from SFU_PEERS
    statuses map[string]*PeerStatus
    interval time.Duration       // 30 seconds
}

type PeerStatus struct {
    ServerID     string
    Region       string
    Status       string    // "ok" or "unreachable"
    Sessions     int
    Peers        int
    BandwidthOut float64
    LastSeen     time.Time
    Latency      time.Duration  // health check round-trip
}
```

If a sibling is unreachable for 3 consecutive polls (90 seconds), its status changes to `"unreachable"` and it shows as a red dot on the dashboard. No alerting, no auto-scaling, just visibility.

### 12.9 Configuration Summary

```
# Admin dashboard
SFU_ADMIN_PASSWORD=<strong random password>       # Required for /admin access
SFU_ADMIN_IPS=155.4.130.18,10.0.0.0/8          # Optional: IP allowlist (empty = password only)

# Peer discovery (for aggregate view)
SFU_PEERS=sfu-eu.mello.app:8080,sfu-us.mello.app:8080

# Logging
SFU_LOG_LEVEL=info                               # debug, info, warn, error
```

---

## 13. Performance Targets

| Metric | Target | Notes |
|---|---|---|
| Forwarding latency (SFU added) | <1ms | Receive → send; pure memory copy |
| WebRTC setup time | <500ms | ICE-lite eliminates gathering delay |
| Peer join → first packet | <1s | Including signaling + WebRTC setup |
| Packet drop rate (SFU-induced) | <0.01% | Only from buffer overflow on slow peers |
| Server memory (1000 peers) | <500 MB | ~2 KB per peer + Go runtime |

---

## 14. Error Codes

| Code | Name | Description |
|---|---|---|
| `INVALID_TOKEN` | Authentication failed | JWT expired, malformed, or invalid signature |
| `SESSION_NOT_FOUND` | Session not found | Session ID doesn't exist on this server |
| `SESSION_FULL` | Session at capacity | Viewer/member limit reached |
| `HOST_ALREADY_CONNECTED` | Host already connected | Stream session already has a host |
| `INVALID_ROLE` | Invalid role | Token role doesn't match join request |
| `PEER_SLOW` | Peer too slow | Send buffer overflow for >5 seconds; force disconnect |
| `SESSION_ENDED` | Session ended | Host disconnected (stream) or session drained |

---

## 15. Testing Checklist

### Stream Sessions
- [ ] Host connects, starts streaming → SFU receives packets
- [ ] Viewer connects → receives forwarded video + audio packets
- [ ] Multiple viewers → each receives independent copy
- [ ] Viewer loss report → relayed to host (host ABR adjusts)
- [ ] Viewer IDR request → relayed to host (host sends keyframe)
- [ ] Slow viewer → packets dropped, does not affect other viewers
- [ ] Slow viewer >5s → force disconnected
- [ ] Host disconnects → all viewers receive session_ended
- [ ] Viewer disconnects → host receives member_left, other viewers unaffected
- [ ] 100 concurrent viewers → forwarding performance within targets

### Voice Sessions
- [ ] Two members connect → each hears the other
- [ ] Member audio not echoed back to sender
- [ ] 10+ members → all hear each other (N-1 forwarding)
- [ ] Member disconnects → removed from forwarding, others notified
- [ ] Free crew → voice_join returns P2P mode, 6-member cap enforced
- [ ] Premium crew → voice_join returns SFU mode, even for 2 members
- [ ] Premium crew → 50 members in one channel works
- [ ] Self-hosted with SFU subscription → voice routes through Mello's SFU
- [ ] Self-hosted without subscription → P2P only, 6-member cap
- [ ] Lapsed SFU subscription → falls back to P2P

### Auth & Security
- [ ] Expired JWT → connection rejected
- [ ] Viewer token used as host → rejected
- [ ] Wrong session_id in token → rejected
- [ ] No token → connection rejected (HTTP 401)

### TURN Server (coturn)
- [ ] coturn starts and listens on 3478 (UDP/TCP) and 5349 (TLS)
- [ ] HMAC-SHA1 credentials from Nakama `get_ice_servers` RPC are accepted by coturn
- [ ] P2P voice connects through TURN relay when direct connection fails
- [ ] P2P streaming connects through TURN relay when direct connection fails
- [ ] Trickle ICE test (webrtc.github.io) shows relay candidates
- [ ] Relay port range stays within 49152-65535
- [ ] Private IP ranges blocked (no SSRF via relay)

### Deployment
- [ ] Docker image builds and runs
- [ ] Health endpoint returns accurate stats
- [ ] Graceful shutdown drains active sessions (30s timeout)
- [ ] GCP firewall rules allow TURN (3478, 5349), WSS (8443), and UDP (10000-10100, 49152-65535)
- [ ] EU and US instances independently operational
- [ ] Cloudflare DNS + TLS proxy working for signaling
- [ ] WebRTC media bypasses Cloudflare (direct UDP to VM IP)

### Reconnection
- [ ] Client WebSocket drops → client reconnects with new token → rejoins session
- [ ] Brief network interruption (<30s) → session persists in Draining state → client reconnects
- [ ] SFU restart → all clients detect disconnect → reconnect flow
- [ ] Voice SFU auto-reconnect: exponential backoff (2s, 4s, 8s, 16s, 32s), max 5 attempts, gives up and emits `VoiceStateChanged { in_call: false }` on failure

### Admin Dashboard & Observability
- [ ] `/health` returns valid JSON with session/peer counts, no auth required
- [ ] `/admin` blocked without password → returns 401
- [ ] `/admin` blocked from non-allowlisted IP → returns 403
- [ ] `/admin` accessible with correct password + IP → dashboard loads
- [ ] Dashboard overview tab shows live counters, sparklines update every 5s
- [ ] Dashboard sessions tab lists active sessions with member counts
- [ ] Session drill-down shows per-peer stats (packets, buffer, ICE state)
- [ ] Dashboard logs tab shows filtered log stream, auto-scrolls
- [ ] Log filters work: level, session ID, user ID, event type
- [ ] Aggregate banner shows sibling SFU instances via `/admin/api/peers`
- [ ] Unreachable sibling shows red status dot after 90s
- [ ] Structured JSON logs appear in GCP Cloud Logging with correct fields
- [ ] Log ring buffer retains 10,000 entries, oldest evicted correctly

---

## 16. File Structure

```
mello-sfu/                            # Private repository
├── cmd/
│   └── sfu/
│       └── main.go                   # Entry point
├── internal/
│   ├── server/
│   │   ├── server.go                 # Core server, WebSocket + HTTP handler
│   │   ├── config.go                 # Configuration
│   │   ├── session_store.go          # Session lifecycle management
│   │   ├── stream_session.go         # 1-to-many stream forwarding
│   │   ├── voice_session.go          # Many-to-many voice forwarding
│   │   ├── peer.go                   # Peer connection wrapper
│   │   ├── peer_set.go              # Lock-free concurrent peer set
│   │   ├── signaling.go             # WebSocket message types and handling
│   │   ├── health.go                # /health endpoint (public)
│   │   ├── admin.go                 # /admin/* routes, API handlers
│   │   ├── admin_auth.go            # IP allowlist + Basic Auth middleware
│   │   └── peers.go                 # Aggregate view, sibling polling
│   ├── auth/
│   │   └── jwt.go                    # JWT RS256 verification
│   ├── logging/
│   │   ├── logger.go                # Structured JSON logger
│   │   └── ring.go                  # 10,000-entry log ring buffer
│   └── metrics/
│       └── metrics.go                # In-memory counters, gauges, history ring
├── admin/
│   └── dashboard.html               # Single-file admin UI (embedded via go:embed)
├── deploy/
│   └── deploy-instance.sh           # GCP VM provisioning + coturn install
├── Dockerfile
├── docker-compose.yml                # Local dev (SFU + test clients)
├── go.mod
├── go.sum
└── README.md
```

### Client-side changes (in public mello repo)

```
mello-core/src/
  transport/
    sfu_connection.rs                 # NEW: WebSocket signaling + WebRTC to SFU
  stream/
    sink_sfu.rs                       # UPDATED: Replace stub with real implementation
  voice/
    manager.rs                        # UPDATED: SFU mode branch in join_channel()

backend/nakama/data/modules/
  streaming.go                        # UPDATED: watch_stream RPC, region selection
  voice_state.go                      # UPDATED: crew-level voice mode in voice_join
  sfu_routing.go                      # NEW: Region selection, SFU endpoint config
  sfu_auth.go                         # NEW: JWT signing for SFU tokens
```

---

## 17. Implementation Order

### Phase 0: TURN Server + GCP Infra (Now)
1. Run `deploy-instance.sh eu` to provision GCP VM in europe-west3 with coturn
2. Update Nakama backend `.env` with `TURN_SECRET` and `TURN_HOST`
3. Verify TURN works: Trickle ICE test with credentials from `get_ice_servers` RPC
4. Run `deploy-instance.sh us` for the US region
5. Test P2P voice/streaming with TURN relay available

This gives you working NAT traversal immediately, and the VMs are ready for the SFU when it's built.

### Phase 1: Stream Fan-out + Logging Foundation (Week 1-2)
1. [x] Scaffold Go project with Pion WebRTC
2. [x] **Implement structured JSON logger + log ring buffer (10,000 entries)**
3. [x] **Implement in-memory metrics (counters, gauges, history ring)**
4. [x] Implement WebSocket signaling server
5. [x] Implement stream session (host + viewer forwarding)
6. [x] Implement `/health` endpoint
7. [ ] Implement `SfuSink` in mello-core (replace stub)
8. [ ] Update `start_stream` / `watch_stream` RPCs in Nakama
9. [ ] Deploy SFU binary to existing GCP VMs (alongside coturn)
10. [ ] End-to-end test: host streams → SFU → viewer receives

### Phase 2: Voice Relay (Week 3)
1. [x] Implement voice session (many-to-many forwarding)
2. [ ] Update `voice_join` RPC with crew-level mode check
3. [ ] Update VoiceManager in mello-core (SFU branch in join_channel)
4. [ ] End-to-end test: premium crew, voice through SFU

### Phase 3: Admin Dashboard + Production Readiness (Week 4)
1. [x] **Implement admin API endpoints (overview, sessions, session detail, logs, peers)**
2. [x] **Implement admin auth middleware (IP allowlist + Basic Auth)**
3. [x] **Build dashboard HTML/JS (dark theme, three tabs, embedded via go:embed)**
4. [x] **Implement sibling polling for aggregate view**
5. [ ] Set up GCP Artifact Registry for container images
6. [ ] Configure Nakama routing (region selection)
7. [ ] JWT key pair setup between Nakama and SFU
8. [ ] Integration test across regions

### Phase 4: Hardening (Week 5)
1. [x] Slow peer detection and force-disconnect
2. [x] Session draining and cleanup
3. [ ] Load testing (target: 100 viewers on a single stream)
4. [ ] Reconnection flows
5. [ ] Cloudflare DNS + TLS for SFU signaling endpoints

---

*This spec covers the SFU server. For client-side streaming, see [12-STREAMING.md](./12-STREAMING.md). For voice channels, see [13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md). For the video capture/encode/decode pipeline, see [14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md).*
