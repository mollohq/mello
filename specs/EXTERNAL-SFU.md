# SFU Integration: Backend and Client

> **Component:** Backend integration (Go, Nakama) · mello-core SFU client (Rust)
> **Version:** 0.3
> **Status:** Current. The SFU itself is specified in the public mello-sfu repository ([01-SFU.md](https://github.com/mollohq/mello-sfu/blob/main/01-SFU.md)).
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [12-STREAMING.md](./12-STREAMING.md), [13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md), [04-BACKEND.md](./04-BACKEND.md), [features/SFU-INTEGRATION.md](./features/SFU-INTEGRATION.md)

---

## 1. Overview

The SFU (Selective Forwarding Unit) is the media relay of m3llo. It is the only media transport for voice and streams. It forwards RTP and does not transcode or mix.

| Part | Where it is specified |
|---|---|
| The SFU: signaling, media plane, security, admin API, error codes | mello-sfu ([01-SFU.md](https://github.com/mollohq/mello-sfu/blob/main/01-SFU.md)) |
| Token signing, the RPCs, region selection | This document, sections 3 and 4 |
| The client side of the SFU connection | This document, section 5, and [12-STREAMING.md](./12-STREAMING.md) |
| Running an SFU | mello-sfu ([SELF-HOSTING.md](https://github.com/mollohq/mello-sfu/blob/main/SELF-HOSTING.md)) and [docs/self-hosting.md](../docs/self-hosting.md) |

| Rule | Effect |
|---|---|
| SFU only | `start_stream`, `watch_stream` and `voice_join` return `mode: "sfu"`, or the error `SFU_UNAVAILABLE`. |
| Kill switch | The crew metadata `"sfu_enabled": false` turns the SFU off for one crew. The default is on. |
| No region built in | The backend uses only the regions in `SFU_REGIONS` (section 4.4). |
| Self-contained tokens | The SFU verifies the token with a public key. It never calls Nakama. |

---

## 2. The SFU Contract

mello-sfu is the source of truth for everything the SFU does:

| Topic | mello-sfu document |
|---|---|
| Session types and lifecycle | [01-SFU.md](https://github.com/mollohq/mello-sfu/blob/main/01-SFU.md) section 4 |
| WebSocket signaling messages | [01-SFU.md](https://github.com/mollohq/mello-sfu/blob/main/01-SFU.md) section 5 |
| Media plane: H.264 RTP relay, voice receive slots, REMB and PLI | [01-SFU.md](https://github.com/mollohq/mello-sfu/blob/main/01-SFU.md) section 6 |
| Security model | [01-SFU.md](https://github.com/mollohq/mello-sfu/blob/main/01-SFU.md) section 7 |
| Error codes | [01-SFU.md](https://github.com/mollohq/mello-sfu/blob/main/01-SFU.md) section 12 |
| Admin API (JSON, HTTP Basic Auth) | [ADMIN-API.md](https://github.com/mollohq/mello-sfu/blob/main/ADMIN-API.md) |
| Stream snapshots | [STREAM-SNAPSHOTS-SFU.md](https://github.com/mollohq/mello-sfu/blob/main/STREAM-SNAPSHOTS-SFU.md) |

A change to the signaling or the media plane starts in mello-sfu. The client follows it.

---

## 3. Authentication

### 3.1 SFU Token

Nakama signs a short-lived JWT (RS256) when a client calls `start_stream`, `watch_stream` or `voice_join`. The code is `backend/nakama/data/modules/sfu_auth.go`.

```go
type SFUTokenClaims struct {
    Issuer    string `json:"iss"`      // "mello-nakama"
    ExpiresAt int64  `json:"exp"`      // 5 minutes after issue
    IssuedAt  int64  `json:"iat"`
    UserID    string `json:"uid"`
    Username  string `json:"username,omitempty"` // display name
    SessionID string `json:"sid"`      // stream session ID or voice session key
    Type      string `json:"type"`     // "stream" or "voice"
    Role      string `json:"role"`     // "host", "viewer" or "member"
    CrewID    string `json:"crew_id"`
    ChannelID string `json:"ch_id,omitempty"` // voice sessions only
    Region    string `json:"region"`   // must match the SFU's SFU_REGION
}
```

| Item | Value |
|---|---|
| Private key | `SFU_JWT_PRIVATE_KEY` on Nakama (PEM). Without it the SFU is unavailable. |
| Public key | The SFU's `SFU_JWT_PUB_KEY` (a path to the PEM file). |
| Lifetime | 5 minutes. The client must connect in this time. The WebSocket session stays open after the token expires. |

### 3.2 Authorization on the SFU

The SFU checks these rules on `join_*`:

The SFU enforces these rules on `join_*`:

| Check | Rule |
|---|---|
| Token `type` matches join message | `join_stream` requires `type: "stream"`, `join_voice` requires `type: "voice"` |
| Token `role` matches claimed role | A viewer token cannot join as host |
| Token `session_id` matches | Must match the session being joined |
| Stream host uniqueness | Only one host per stream session |
| Token not expired | Checked on WebSocket connect; not rechecked after |

### 3.3 Encryption

All media goes over DTLS-SRTP (RTP tracks) and DTLS-SCTP (DataChannels). The SFU terminates DTLS on each peer connection. End-to-end encryption (SFrame) is not planned for v1.

---

## 4. Nakama Integration

### 4.1 `start_stream` RPC

The host gets the session, the endpoint and a host token. A sketch of `backend/nakama/data/modules/streaming.go`:

```go
func StartStreamRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    // ... existing validation ...

    if !sfuAvailableForCrew(ctx, nk, crewID) {
        return "", errSFUUnavailable // "SFU_UNAVAILABLE", code 14
    }
    {
        region := selectSFURegion(req.PreferredRegion) // a region of SFU_REGIONS
        endpoint := sfuEndpointForRegion(region)
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
}
```

### 4.2 `watch_stream` RPC

A viewer gets its own token for the session of the host. A sketch:

```go
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

### 4.3 `voice_join` RPC

The SFU is the only voice transport. `voice_join` answers `mode: "sfu"` or the error `SFU_UNAVAILABLE`. A channel holds up to 50 members (`MaxVoiceChannelMembers`). A sketch of `backend/nakama/data/modules/voice_state.go`:

```go
func VoiceJoinRPC(ctx context.Context, ...) (string, error) {
    // ... existing validation ...

    // Server has a signing key, and the crew kill switch is off.
    if !sfuAvailableForCrew(ctx, nk, crewID) {
        return "", errSFUUnavailable // "SFU_UNAVAILABLE", code 14
    }

    // joinVoiceRoom checks capacity (MaxVoiceChannelMembers, 50).
    snap, err := joinVoiceRoom(ctx, logger, nk, params)
    if err != nil {
        return "", err // CHANNEL_FULL and others
    }

    endpoint, token, ok := issueVoiceSFUToken(logger, params)
    if !ok {
        voiceLeaveInternal(ctx, logger, nk, userID) // a failed join seats nobody
        return "", errSFUUnavailable
    }

    return marshal(VoiceJoinResponse{
        Mode:        "sfu",
        SFUEndpoint: endpoint,
        SFUToken:    token,
        VoiceState:  snap,
    }), nil
}

// sfuEnabledForCrew: true unless the crew metadata has "sfu_enabled": false.
func sfuEnabledForCrew(ctx context.Context, nk runtime.NakamaModule, crewID string) bool
```

### 4.4 Region Selection

The regions come from configuration (`SFU_REGIONS`, see `specs/features/SFU-INTEGRATION.md` section 2.2). Nothing is built in: the official network and a self-hoster both list their own SFUs.

```go
// nakama/data/modules/sfu_routing.go

// SFU_REGIONS="eu-west=wss://sfu-eu.example.com:8443/ws,us-east=wss://sfu-us.example.com:8443/ws"
var sfuEndpoints map[string]string // region -> endpoint, from SFU_REGIONS

// The client's choice when it names a configured region, else
// SFU_DEFAULT_REGION when that names one, else the first region.
func selectSFURegion(preferred string) string
```

| Input | Rule |
|---|---|
| `preferred_region` in `voice_join` and `start_stream` | Optional. Used when it names a configured region. An empty or unknown value falls back; it never fails the RPC. |
| `SFU_DEFAULT_REGION` (env) | The fallback when it names a configured region. |
| The first region of `SFU_REGIONS` | The last fallback. |
| No region configured | `voice_join` and `start_stream` return `SFU_UNAVAILABLE`. |

The client measures the round trip to each region's `/health` (the `get_sfu_regions` RPC lists them) at login and every 10 minutes, and sends the fastest as `preferred_region` (spec 10 section 7.6, spec 13 section 4.2). The IP geolocation fallback is not built.

For stream sessions, the **host's region** determines the SFU. Viewers connect to the same SFU as the host (`watch_stream` reads the stored session region). Cross-region relay between SFU instances is a post-v1 feature.

For voice sessions, the **first member who gets an SFU token** picks the region for the room (`VoiceRoom.SFURegion`). Every later member gets the room's region, whatever it prefers, because all members of a voice session must reach the same SFU instance. The region goes away with the room when the last member leaves.

---

## 5. Client Integration

| Part | Code |
|---|---|
| Stream host and viewer | `mello-core/src/client/streaming.rs`, `mello-core/src/stream/sink_sfu.rs` |
| SFU connection: WebSocket signaling and the peer connection | `mello-core/src/transport/sfu_connection.rs` |
| Voice join | `mello-core/src/client/sfu_voice_join.rs`, `mello-core/src/voice/` |
| Region probe | `mello-core/src/transport/sfu_region.rs` |
| Peer connection (libdatachannel) | `libmello/src/transport/peer_connection.cpp`, [03-LIBMELLO.md](./03-LIBMELLO.md) |

The client contract for streams is [12-STREAMING.md](./12-STREAMING.md) section 6. For voice it is [13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md).

The client declares `a=ssrc` on its voice audio m-line. Without it the SFU cannot bind the incoming mic to the m-line ([01-SFU.md](https://github.com/mollohq/mello-sfu/blob/main/01-SFU.md) section 6).

Voice reconnects after an SFU disconnect: the first attempt after 500 ms, then 1 s, then every 2 s. The client stops 30 s after the drop and leaves the call. The constants are in `mello-core/src/client/voice.rs` ([10-AUDIO_PIPELINE.md](./10-AUDIO_PIPELINE.md) section 7.6).

---

## 6. Configuration

| Variable (Nakama) | Use |
|---|---|
| `SFU_JWT_PRIVATE_KEY` | Signs the SFU tokens. Required. |
| `SFU_REGIONS` | `region=endpoint`, comma-separated. Example: `eu-west=wss://sfu.example.com:8443/ws`. |
| `SFU_DEFAULT_REGION` | The fallback region. Default: the first region. |
| `SFU_HEALTH_PORT` | The port of the health URLs in `get_sfu_regions`. Default 8080. |
| `SFU_ENDPOINT_EU`, `SFU_ENDPOINT_US` | Older variables for `eu-west` and `us-east`. Used only when `SFU_REGIONS` is not set. |

[features/SFU-INTEGRATION.md](./features/SFU-INTEGRATION.md) section 2.2 has the full rules. The SFU side of the configuration is in [SELF-HOSTING.md](https://github.com/mollohq/mello-sfu/blob/main/SELF-HOSTING.md).
