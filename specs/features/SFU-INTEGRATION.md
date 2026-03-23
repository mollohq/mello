# SFU Integration Specification

> **Component:** mello-core (Rust) · Backend/Nakama (Go)  
> **Status:** Ready to implement  
> **Depends on:** [01-SFU.md](./01-SFU.md) (SFU server, already built), [12-STREAMING.md](./12-STREAMING.md), [13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md)

---

## 1. Overview

This spec covers the integration work needed to connect the existing SFU server to the client and backend. The SFU itself is built and deployed. This spec implements the client-side `SfuSink`, the Nakama RPCs that route traffic to the SFU, and the JWT signing that authenticates clients with the SFU.

### What exists today

| Component | Status |
|---|---|
| SFU server (Go, Pion WebRTC) | Built, Phase 1-3 complete |
| `PacketSink` trait in mello-core | Exists ([12-STREAMING.md §8.1](./12-STREAMING.md)) |
| `P2PFanoutSink` | Exists, working |
| `SfuSink` | Stub that returns `Err(SfuNotImplemented)` |
| `start_stream` RPC | Exists, P2P-only |
| `voice_join` RPC | Exists, P2P-only, keyed by channel_id ([13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md)) |
| TURN server (coturn) | Deployed on GCP, working |

### What this spec adds

| Component | Change |
|---|---|
| `SfuSink` | Replace stub with real WebSocket + WebRTC connection to SFU |
| `SfuConnection` | New: manages WS signaling + PeerConnection + DataChannels to SFU |
| `start_stream` RPC | Add SFU mode branch (premium crews get SFU endpoint + JWT) |
| `watch_stream` RPC | New: viewers request their own SFU token |
| `voice_join` RPC | Add crew-level mode check, return SFU endpoint + JWT when applicable |
| `VoiceManager` | Add SFU branch in `join_channel()` |
| JWT signing | New Nakama module: sign RS256 tokens for SFU auth |
| SFU routing | New Nakama module: region selection |

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

### 2.3 `sfu_entitlement.go` - Premium Crew Check

For beta, this is a simple flag in crew metadata. No credits system, no Stripe integration yet. When that ships, this function gets a real implementation.

```go
// backend/nakama/data/modules/sfu_entitlement.go

package main

import (
    "context"
    "encoding/json"

    "github.com/heroiclabs/nakama-common/runtime"
)

// hasPremiumCrew checks if a crew has SFU access.
// Beta: checks a flag in crew metadata.
// Production: checks credits/subscription system.
func hasPremiumCrew(ctx context.Context, nk runtime.NakamaModule, crewID string) bool {
    // Load crew group metadata
    groups, err := nk.GroupsGetId(ctx, []string{crewID})
    if err != nil || len(groups) == 0 {
        return false
    }

    var meta CrewMetadata
    if err := json.Unmarshal([]byte(groups[0].GetMetadata()), &meta); err != nil {
        return false
    }

    return meta.SFUEnabled
}

// Add to CrewMetadata (in crews.go):
// SFUEnabled bool `json:"sfu_enabled"`
//
// For beta testing, set this manually via Nakama console:
//   Group → Edit Metadata → add "sfu_enabled": true
```

---

## 3. Backend: Modified Files

### 3.1 `streaming.go` - Updated `start_stream`, New `watch_stream`

**Changes to existing `StartStreamRPC`:**

The current function only handles P2P. Add an SFU branch before the P2P path. The response struct gains new fields.

```go
// backend/nakama/data/modules/streaming.go

// Updated response type
type StartStreamResponse struct {
    SessionID string `json:"session_id"`
    Mode      string `json:"mode"` // "p2p" or "sfu"

    // P2P fields
    MaxViewers int `json:"max_viewers,omitempty"`

    // SFU fields
    SFUEndpoint string `json:"sfu_endpoint,omitempty"`
    SFUToken    string `json:"sfu_token,omitempty"`
}

func StartStreamRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    userID := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)

    var req StartStreamRequest
    if err := json.Unmarshal([]byte(payload), &req); err != nil {
        return "", runtime.NewError("invalid request", 3)
    }

    // Existing validation: verify crew membership
    if !isCrewMember(ctx, nk, userID, req.CrewID) {
        return "", runtime.NewError("not a crew member", 7)
    }

    sessionID := generateSessionID()
    var resp StartStreamResponse

    // SFU path: premium crews get server-relayed streaming
    if sfuAuthEnabled() && hasPremiumCrew(ctx, nk, req.CrewID) {
        region := selectSFURegion("") // TODO: pass user region
        endpoint := sfuEndpointForRegion(region)

        token, err := signSFUToken(SFUTokenClaims{
            UserID:    userID,
            SessionID: sessionID,
            Type:      "stream",
            Role:      "host",
            CrewID:    req.CrewID,
            Region:    region,
        })
        if err != nil {
            logger.Error("Failed to sign SFU token: %v", err)
            // Fall through to P2P
        } else {
            // Store session so viewers can look it up
            storeStreamSession(ctx, nk, sessionID, StreamSessionMeta{
                CrewID:      req.CrewID,
                HostUserID:  userID,
                Mode:        "sfu",
                SFURegion:   region,
                SFUEndpoint: endpoint,
            })

            resp = StartStreamResponse{
                SessionID:   sessionID,
                Mode:        "sfu",
                SFUEndpoint: endpoint,
                SFUToken:    token,
            }

            logger.Info("Stream started (SFU): session=%s crew=%s host=%s region=%s", sessionID, req.CrewID, userID, region)
            respJSON, _ := json.Marshal(resp)
            return string(respJSON), nil
        }
    }

    // P2P path (free crews or SFU auth not configured)
    storeStreamSession(ctx, nk, sessionID, StreamSessionMeta{
        CrewID:     req.CrewID,
        HostUserID: userID,
        Mode:       "p2p",
    })

    resp = StartStreamResponse{
        SessionID:  sessionID,
        Mode:       "p2p",
        MaxViewers: 5,
    }

    logger.Info("Stream started (P2P): session=%s crew=%s host=%s", sessionID, req.CrewID, userID)

    // Existing: broadcast "X is streaming" to crew
    broadcastStreamStart(ctx, nk, userID, req.CrewID, sessionID)

    respJSON, _ := json.Marshal(resp)
    return string(respJSON), nil
}

// --- Session storage helpers ---

type StreamSessionMeta struct {
    CrewID      string `json:"crew_id"`
    HostUserID  string `json:"host_user_id"`
    Mode        string `json:"mode"` // "p2p" or "sfu"
    SFURegion   string `json:"sfu_region,omitempty"`
    SFUEndpoint string `json:"sfu_endpoint,omitempty"`
}

func storeStreamSession(ctx context.Context, nk runtime.NakamaModule, sessionID string, meta StreamSessionMeta) {
    data, _ := json.Marshal(meta)
    nk.StorageWrite(ctx, []*runtime.StorageWrite{{
        Collection:      "stream_sessions",
        Key:             sessionID,
        UserID:          "",
        Value:           string(data),
        PermissionRead:  1, // owner-read (server counts as owner for system-owned)
        PermissionWrite: 0, // server-only write
    }})
}

func loadStreamSession(ctx context.Context, nk runtime.NakamaModule, sessionID string) *StreamSessionMeta {
    records, err := nk.StorageRead(ctx, []*runtime.StorageRead{{
        Collection: "stream_sessions",
        Key:        sessionID,
        UserID:     "",
    }})
    if err != nil || len(records) == 0 {
        return nil
    }
    var meta StreamSessionMeta
    if err := json.Unmarshal([]byte(records[0].Value), &meta); err != nil {
        return nil
    }
    return &meta
}

func generateSessionID() string {
    // "str_" prefix + 12 random alphanumeric chars
    return "str_" + randomAlphanumeric(12)
}
```

**New `WatchStreamRPC`:**

```go
// backend/nakama/data/modules/streaming.go (continued)

type WatchStreamRequest struct {
    SessionID string `json:"session_id"`
}

type WatchStreamResponse struct {
    Mode        string `json:"mode"` // "p2p" or "sfu"
    SFUEndpoint string `json:"sfu_endpoint,omitempty"`
    SFUToken    string `json:"sfu_token,omitempty"`
}

func WatchStreamRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    userID := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)

    var req WatchStreamRequest
    if err := json.Unmarshal([]byte(payload), &req); err != nil {
        return "", runtime.NewError("invalid request", 3)
    }

    meta := loadStreamSession(ctx, nk, req.SessionID)
    if meta == nil {
        return "", runtime.NewError("stream not found", 5)
    }

    // Verify viewer is in the same crew
    if !isCrewMember(ctx, nk, userID, meta.CrewID) {
        return "", runtime.NewError("not a crew member", 7)
    }

    if meta.Mode == "sfu" {
        token, err := signSFUToken(SFUTokenClaims{
            UserID:    userID,
            SessionID: req.SessionID,
            Type:      "stream",
            Role:      "viewer",
            CrewID:    meta.CrewID,
            Region:    meta.SFURegion,
        })
        if err != nil {
            return "", runtime.NewError("token signing failed", 13)
        }

        resp := WatchStreamResponse{
            Mode:        "sfu",
            SFUEndpoint: meta.SFUEndpoint,
            SFUToken:    token,
        }
        respJSON, _ := json.Marshal(resp)
        return string(respJSON), nil
    }

    // P2P mode: viewer connects directly to host via signaling
    resp := WatchStreamResponse{Mode: "p2p"}
    respJSON, _ := json.Marshal(resp)
    return string(respJSON), nil
}
```

**Register both RPCs in `main.go`:**

```go
// In InitModule()
initializer.RegisterRpc("start_stream", StartStreamRPC)
initializer.RegisterRpc("watch_stream", WatchStreamRPC)
```

### 3.2 `voice_state.go` - Updated `voice_join`

The existing `VoiceJoinRPC` only handles P2P. Add a crew-level mode check at the top.

**Changes to existing `VoiceJoinRPC`:**

```go
// backend/nakama/data/modules/voice_state.go

// Updated response type
type VoiceJoinResponse struct {
    Mode        string             `json:"mode"` // "p2p" or "sfu"
    Members     []VoiceMemberState `json:"members"`

    // SFU fields (mode == "sfu")
    SFUEndpoint string `json:"sfu_endpoint,omitempty"`
    SFUToken    string `json:"sfu_token,omitempty"`
}

func VoiceJoinRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    userID := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)

    var req VoiceJoinRequest
    if err := json.Unmarshal([]byte(payload), &req); err != nil {
        return "", runtime.NewError("invalid request", 3)
    }

    // Existing validation: crew membership, channel exists, user not already in channel, etc.
    // ... (unchanged) ...

    room := getOrCreateVoiceRoom(req.ChannelID, req.CrewID)

    // --- NEW: Crew-level voice mode check ---
    if sfuAuthEnabled() && hasPremiumCrew(ctx, nk, req.CrewID) {
        // SFU mode: route through Mello's SFU
        if len(room.Members) >= 50 {
            return "", runtime.NewError("channel full", 8)
        }

        region := selectSFURegion("")
        endpoint := sfuEndpointForRegion(region)
        voiceSessionKey := fmt.Sprintf("voice:%s:%s", req.CrewID, req.ChannelID)

        token, err := signSFUToken(SFUTokenClaims{
            UserID:    userID,
            SessionID: voiceSessionKey,
            Type:      "voice",
            Role:      "member",
            CrewID:    req.CrewID,
            ChannelID: req.ChannelID,
            Region:    region,
        })
        if err != nil {
            logger.Error("Failed to sign SFU token for voice: %v", err)
            // Fall through to P2P
        } else {
            // Add user to room (existing logic)
            addMemberToRoom(room, userID)
            broadcastVoiceUpdate(ctx, nk, req.CrewID, req.ChannelID, room)

            resp := VoiceJoinResponse{
                Mode:        "sfu",
                Members:     room.MemberList(),
                SFUEndpoint: endpoint,
                SFUToken:    token,
            }
            logger.Info("Voice join (SFU): user=%s crew=%s channel=%s region=%s", userID, req.CrewID, req.ChannelID, region)
            respJSON, _ := json.Marshal(resp)
            return string(respJSON), nil
        }
    }

    // P2P mode: existing behaviour
    if len(room.Members) >= 6 {
        return "", runtime.NewError("channel full", 8)
    }

    // Existing P2P join logic (add member, broadcast, return member list)
    addMemberToRoom(room, userID)
    broadcastVoiceUpdate(ctx, nk, req.CrewID, req.ChannelID, room)

    resp := VoiceJoinResponse{
        Mode:    "p2p",
        Members: room.MemberList(),
    }
    logger.Info("Voice join (P2P): user=%s crew=%s channel=%s", userID, req.CrewID, req.ChannelID)
    respJSON, _ := json.Marshal(resp)
    return string(respJSON), nil
}
```

### 3.3 `crews.go` - Updated `CrewMetadata`

Add the `SFUEnabled` field:

```go
// In CrewMetadata struct (crews.go)
type CrewMetadata struct {
    MaxMembers    int    `json:"max_members"`
    InviteOnly    bool   `json:"invite_only"`
    CreatedBy     string `json:"created_by"`
    ChannelManage string `json:"channel_manage"`
    SFUEnabled    bool   `json:"sfu_enabled"` // NEW: enables SFU for this crew
}
```

For beta testing, set `sfu_enabled: true` manually on test crews via the Nakama admin console (Group -> Edit Metadata).

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

## 4. Client: New Files

### 4.1 `sfu_connection.rs` - WebSocket + WebRTC to SFU

This is the transport layer between the client and the SFU server. It manages:
- WebSocket connection for signaling (JSON messages)
- WebRTC PeerConnection via libdatachannel
- Two DataChannels: `media` (unreliable) and `control` (reliable)
- Background tasks for ICE candidates and signaling messages

```rust
// mello-core/src/transport/sfu_connection.rs

use crate::stream::StreamError;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

pub struct SfuConnection {
    ws: Mutex<WebSocketStream>,               // tungstenite or tokio-tungstenite
    peer_connection: Arc<PeerConnection>,       // libdatachannel via mello-core-sys
    media_channel: Arc<DataChannel>,            // unreliable, unordered
    control_channel: Arc<DataChannel>,          // reliable, ordered
    event_tx: mpsc::Sender<SfuEvent>,
    event_rx: Mutex<mpsc::Receiver<SfuEvent>>,
    server_id: String,
    region: String,
}

#[derive(Debug)]
pub enum SfuEvent {
    MemberJoined { user_id: String, role: String },
    MemberLeft { user_id: String, reason: String },
    MediaPacket { data: Vec<u8> },
    ControlPacket { data: Vec<u8> },
    Disconnected { reason: String },
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

## 5. Client: Modified Files

### 5.1 `stream/host.rs` - Updated `start_stream`

The existing `start_stream` function already handles the `mode` field. The only change: when mode is `"sfu"`, the `SfuSink::new()` now works instead of returning `SfuNotImplemented`.

```rust
// mello-core/src/stream/host.rs - NO CHANGES NEEDED
// The existing code from spec 12 §9.2 already handles this:

let sink: Arc<dyn PacketSink> = match resp.mode.as_str() {
    "p2p"  => Arc::new(P2PFanoutSink::new()),
    "sfu"  => Arc::new(SfuSink::new(&resp.sfu_endpoint, &resp.sfu_token).await?),
    other  => return Err(StreamError::UnknownMode(other.to_string())),
};
```

Verify this code exists and that it passes `sfu_endpoint` and `sfu_token` from the RPC response. The `StartStreamResponse` deserialization needs to include the new fields:

```rust
// mello-core/src/nakama/types.rs (or wherever RPC response types live)

#[derive(Debug, Deserialize)]
pub struct StartStreamResponse {
    pub session_id: String,
    pub mode: String,

    // P2P fields
    #[serde(default)]
    pub max_viewers: Option<i32>,

    // SFU fields
    #[serde(default)]
    pub sfu_endpoint: Option<String>,
    #[serde(default)]
    pub sfu_token: Option<String>,
}
```

### 5.2 `stream/viewer.rs` - SFU Viewer Path

The viewer needs to call `watch_stream` to get its own SFU token, then connect.

```rust
// mello-core/src/stream/viewer.rs

pub async fn watch_stream(
    nakama: &NakamaClient,
    session_id: &str,
) -> Result<StreamViewSession, StreamError> {
    let resp: WatchStreamResponse = nakama.rpc("watch_stream", &WatchStreamRequest {
        session_id: session_id.to_string(),
    }).await?;

    match resp.mode.as_str() {
        "sfu" => {
            let conn = SfuConnection::connect(
                &resp.sfu_endpoint.unwrap(),
                &resp.sfu_token.unwrap(),
            ).await?;

            conn.join_stream(session_id, "viewer").await?;

            // Spawn task to receive media packets from SFU and feed to decoder
            let conn = Arc::new(conn);
            let conn_clone = conn.clone();
            tokio::spawn(async move {
                loop {
                    match conn_clone.recv_event().await {
                        Some(SfuEvent::MediaPacket { data }) => {
                            // Feed to StreamManager/decoder (same as P2P receive path)
                            // The packet format is identical: 12-byte header + payload
                        }
                        Some(SfuEvent::ControlPacket { data }) => {
                            // Handle control messages from host (via SFU)
                        }
                        Some(SfuEvent::Disconnected { reason }) => {
                            log::warn!("SFU disconnected: {}", reason);
                            break;
                        }
                        _ => {}
                    }
                }
            });

            Ok(StreamViewSession::Sfu { connection: conn })
        }
        _ => {
            // Existing P2P viewer path
            Ok(StreamViewSession::P2P { /* existing */ })
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct WatchStreamResponse {
    pub mode: String,
    pub sfu_endpoint: Option<String>,
    pub sfu_token: Option<String>,
}
```

### 5.3 `voice/manager.rs` - SFU Branch in `join_channel`

The `VoiceManager` already tracks `connected_crew`, `connected_channel`, and `peers`. Add `mode` and `sfu_connection` fields, and branch in `join_channel()` based on the RPC response.

**Fields to add:**

```rust
pub struct VoiceManager {
    // ... existing fields ...

    mode: VoiceMode,                               // NEW
    sfu_connection: Option<Arc<SfuConnection>>,     // NEW
}

enum VoiceMode {
    Disconnected,
    P2P,
    SFU,
}
```

**Updated `join_channel`:**

The `voice_join` RPC response now has a `mode` field. Branch on it:

```rust
pub async fn join_channel(&mut self, nakama: &NakamaClient, crew_id: &str, channel_id: &str) -> Result<(), Error> {
    if self.connected_channel.is_some() {
        self.leave_current(nakama).await?;
    }

    let resp: VoiceJoinResponse = nakama.rpc("voice_join", &VoiceJoinRequest {
        crew_id: crew_id.to_string(),
        channel_id: channel_id.to_string(),
    }).await?;

    self.connected_crew = Some(crew_id.to_string());
    self.connected_channel = Some(channel_id.to_string());

    match resp.mode.as_str() {
        "sfu" => {
            let conn = SfuConnection::connect(
                &resp.sfu_endpoint.unwrap(),
                &resp.sfu_token.unwrap(),
            ).await?;
            conn.join_voice(crew_id, channel_id).await?;

            // Spawn voice receive loop: SFU sends us other members' audio
            let conn = Arc::new(conn);
            let conn_clone = conn.clone();
            let libmello = self.libmello;
            tokio::spawn(async move {
                loop {
                    match conn_clone.recv_event().await {
                        Some(SfuEvent::MediaPacket { data }) => {
                            // Feed audio packet to libmello for decode + playback
                            // Same Opus packet format as P2P
                            unsafe { mello_voice_feed_packet(libmello, data.as_ptr(), data.len() as i32); }
                        }
                        Some(SfuEvent::Disconnected { reason }) => {
                            log::warn!("Voice SFU disconnected: {}", reason);
                            break;
                        }
                        _ => {}
                    }
                }
            });

            // Voice send loop: capture mic audio, send via SFU connection
            // Reuses the existing WASAPI capture -> Opus encode -> send pipeline.
            // Only difference: packets go to conn.send_media() instead of per-peer DataChannels.
            self.start_voice_send_loop_sfu(conn.clone());

            self.sfu_connection = Some(conn);
            self.mode = VoiceMode::SFU;
        }
        _ => {
            // Existing P2P path (unchanged from spec 13)
            self.mode = VoiceMode::P2P;
            for member in &resp.members {
                self.connect_peer(nakama, &member.user_id).await?;
            }
        }
    }

    self.event_tx.send(Event::VoiceConnected {
        crew_id: crew_id.to_string(),
        channel_id: channel_id.to_string(),
    }).await.ok();

    Ok(())
}
```

**Updated `leave_current`:**

```rust
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

    // Existing: call voice_leave RPC
    if let Some(crew_id) = &self.connected_crew {
        nakama.rpc("voice_leave", &VoiceLeaveRequest {
            crew_id: crew_id.clone(),
        }).await.ok();
    }

    self.mode = VoiceMode::Disconnected;
    self.connected_crew = None;
    self.connected_channel = None;
    Ok(())
}
```

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
  sfu_entitlement.go       # hasPremiumCrew() check

mello-core/src/
  transport/sfu_connection.rs   # WebSocket + WebRTC connection to SFU
```

### Modified files

```
backend/nakama/data/modules/
  main.go                  # Register watch_stream, init SFU auth
  streaming.go             # Add SFU branch to start_stream, add watch_stream RPC
  voice_state.go           # Add SFU branch to voice_join
  crews.go                 # Add SFUEnabled to CrewMetadata

mello-core/src/
  stream/sink_sfu.rs       # Replace stub with real SfuSink
  stream/viewer.rs         # Add SFU viewer path (watch_stream RPC)
  stream/mod.rs            # New StreamError variants
  voice/manager.rs         # Add VoiceMode, SFU branch in join_channel
  transport/mod.rs         # Export sfu_connection
  nakama/types.rs          # Add sfu_endpoint, sfu_token to response types
```

---

## 7. Testing Checklist

### Backend RPCs
- [ ] `start_stream` on free crew -> returns `mode: "p2p"`
- [ ] `start_stream` on SFU-enabled crew -> returns `mode: "sfu"` with endpoint + token
- [ ] `start_stream` with no `SFU_JWT_PRIVATE_KEY` -> falls back to P2P
- [ ] `watch_stream` with valid session -> returns viewer token
- [ ] `watch_stream` with wrong crew -> returns `NOT_CREW_MEMBER`
- [ ] `watch_stream` for P2P session -> returns `mode: "p2p"`
- [ ] `voice_join` on free crew -> returns `mode: "p2p"`, cap at 6
- [ ] `voice_join` on SFU crew -> returns `mode: "sfu"` with endpoint + token, cap at 50
- [ ] JWT token is valid RS256, SFU can verify it

### Client SfuSink
- [ ] `SfuSink::new()` connects to SFU WebSocket
- [ ] WebRTC PeerConnection established (ICE-lite)
- [ ] Both DataChannels open (media + control)
- [ ] `send_video()` sends via media DataChannel
- [ ] `send_audio()` sends via media DataChannel
- [ ] `send_control()` sends via control DataChannel
- [ ] Viewer receives MediaPacket events from SFU

### Client VoiceManager
- [ ] P2P join still works (unchanged)
- [ ] SFU join: connects to SFU, joins voice session
- [ ] SFU voice: outgoing audio sent via SfuConnection
- [ ] SFU voice: incoming audio from SFU fed to libmello
- [ ] Leave: SFU connection closed, server notified

### End-to-End
- [ ] Host streams via SFU, viewer receives video
- [ ] Multiple viewers receive the same stream
- [ ] Viewer loss reports reach host (ABR adjusts)
- [ ] Voice works through SFU (2+ members)
- [ ] Disconnection handled cleanly (no orphaned sessions)
