# MELLO Backend Specification

> **Component:** Backend Infrastructure (Nakama)  
> **Platform:** Heroic Labs Nakama  
> **Status:** Beta Scope  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

Mello's backend is built on Nakama, an open-source game server. Nakama handles authentication, presence, groups (crews), real-time chat, and P2P signaling. This keeps backend complexity minimal while providing battle-tested infrastructure.

**Key Responsibilities:**
- User authentication (email, Discord OAuth)
- Presence tracking (online/idle/offline)
- Groups (Crews) with membership management
- Real-time chat (persistent, per-crew)
- P2P signaling (ICE candidate exchange)
- TURN relay configuration

---

## 2. Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           BACKEND STACK                                 │
│                                                                         │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                         CLIENTS                                   │  │
│  │                                                                   │  │
│  │    Windows │ macOS │ Linux │ iOS │ Android                        │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                │                                        │
│                     WebSocket (WSS) + REST (HTTPS)                      │
│                                │                                        │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                      LOAD BALANCER                                │  │
│  │                     (nginx / Cloudflare)                          │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                │                                        │
│         ┌──────────────────────┼──────────────────────┐                 │
│         │                      │                      │                 │
│         ▼                      ▼                      ▼                 │
│  ┌─────────────┐       ┌─────────────┐       ┌─────────────┐           │
│  │   Nakama    │       │   Nakama    │       │   Nakama    │           │
│  │  Instance 1 │◀─────▶│  Instance 2 │◀─────▶│  Instance 3 │           │
│  └─────────────┘       └─────────────┘       └─────────────┘           │
│         │                      │                      │                 │
│         └──────────────────────┼──────────────────────┘                 │
│                                │                                        │
│                                ▼                                        │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                     COCKROACHDB CLUSTER                           │  │
│  │                    (or PostgreSQL for dev)                        │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────┐
│                         P2P INFRASTRUCTURE                              │
│                                                                         │
│  ┌─────────────────────────┐       ┌─────────────────────────────────┐  │
│  │      STUN SERVERS       │       │         TURN SERVERS            │  │
│  │                         │       │                                 │  │
│  │  stun.l.google.com:19302│       │  turn.mello.app:3478            │  │
│  │  stun1.l.google.com:... │       │  (coturn, ~10% of connections)  │  │
│  └─────────────────────────┘       └─────────────────────────────────┘  │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Project Structure

```
backend/
├── docker-compose.yml              # Local development
├── docker-compose.prod.yml         # Production template
├── .env.example                    # Environment variables template
│
├── nakama/
│   ├── Dockerfile                  # Custom Nakama image (if needed)
│   └── data/
│       └── modules/                # Custom server code
│           ├── main.go             # Entry point (Go runtime)
│           ├── auth.go             # Custom auth hooks
│           ├── crews.go            # Crew management logic
│           ├── signaling.go        # P2P signaling helpers
│           └── presence.go         # Presence customization
│
├── migrations/                     # Database migrations
│   └── 001_initial.sql
│
├── config/
│   ├── nakama.yml                  # Nakama configuration
│   └── turn.conf                   # TURN server config
│
└── scripts/
    ├── setup-dev.sh                # Development setup
    ├── deploy.sh                   # Production deployment
    └── backup.sh                   # Database backup
```

---

## 4. Nakama Configuration

### 4.1 Development Config

```yaml
# config/nakama.yml

name: mello-dev

# Server
socket:
  server_key: "mello_dev_server_key_change_in_prod"
  port: 7350
  
# Dashboard
console:
  port: 7351
  username: "admin"
  password: "mello_admin_dev"  # Change in production

# Database
database:
  address:
    - "postgres:5432"

# Logging
logger:
  level: "DEBUG"
  
# Session
session:
  token_expiry_sec: 86400        # 24 hours
  refresh_token_expiry_sec: 604800  # 7 days

# Social
social:
  discord:
    client_id: "${DISCORD_CLIENT_ID}"
    client_secret: "${DISCORD_CLIENT_SECRET}"

# Runtime
runtime:
  path: "/nakama/data/modules"
  http_key: "${NAKAMA_HTTP_KEY}"
```

### 4.2 Docker Compose (Development)

```yaml
# docker-compose.yml

version: '3.8'

services:
  postgres:
    image: postgres:15
    environment:
      POSTGRES_DB: nakama
      POSTGRES_USER: nakama
      POSTGRES_PASSWORD: localdev
    volumes:
      - postgres_data:/var/lib/postgresql/data
    ports:
      - "5432:5432"
    healthcheck:
      test: ["CMD", "pg_isready", "-U", "nakama"]
      interval: 5s
      timeout: 5s
      retries: 5

  nakama:
    image: heroiclabs/nakama:3.21.0
    depends_on:
      postgres:
        condition: service_healthy
    environment:
      - DISCORD_CLIENT_ID=${DISCORD_CLIENT_ID}
      - DISCORD_CLIENT_SECRET=${DISCORD_CLIENT_SECRET}
      - NAKAMA_HTTP_KEY=${NAKAMA_HTTP_KEY:-mello_http_key_dev}
    entrypoint:
      - /bin/sh
      - -c
      - |
        /nakama/nakama migrate up --database.address postgres:localdev@postgres:5432/nakama &&
        exec /nakama/nakama --config /nakama/data/config.yml
    volumes:
      - ./nakama/data:/nakama/data
      - ./config/nakama.yml:/nakama/data/config.yml:ro
    ports:
      - "7350:7350"   # gRPC API
      - "7351:7351"   # Admin console
      - "7352:7352"   # HTTP API
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:7350/"]
      interval: 10s
      timeout: 5s
      retries: 5

  # Local TURN server for testing P2P relay
  coturn:
    image: coturn/coturn:4.6
    network_mode: host
    volumes:
      - ./config/turn.conf:/etc/turnserver.conf:ro

volumes:
  postgres_data:
```

---

## 5. Data Models

### 5.1 User (Nakama built-in + metadata)

```go
// User metadata stored in Nakama
type UserMetadata struct {
    Tag           string `json:"tag"`            // e.g., "#001"
    AvatarURL     string `json:"avatar_url"`
    DisplayName   string `json:"display_name"`
    CreatedAt     int64  `json:"created_at"`
}
```

### 5.2 Crew (Nakama Group + metadata)

```go
// Crew = Nakama Group with custom metadata
type CrewMetadata struct {
    MaxMembers    int    `json:"max_members"`    // Default: 6
    InviteOnly    bool   `json:"invite_only"`
    CreatedBy     string `json:"created_by"`     // User ID
    StreamEnabled bool   `json:"stream_enabled"` // Can members stream?
}

// Nakama Group fields used:
// - id: Unique crew ID
// - name: Crew display name
// - description: Crew description
// - open: Is crew joinable without invite
// - max_count: Maximum members
// - metadata: CrewMetadata JSON
```

### 5.3 Presence Status

```go
// Presence status sent via Nakama presence
type PresenceStatus struct {
    Status      string `json:"status"`       // "online", "idle", "dnd", "offline"
    StreamingTo string `json:"streaming_to"` // Crew ID if streaming, empty otherwise
    WatchingID  string `json:"watching_id"`  // Host user ID if watching, empty otherwise
}
```

### 5.4 Signaling Messages

```go
// P2P signaling messages sent via Nakama channel messages
type SignalMessage struct {
    Type      string `json:"type"`       // "offer", "answer", "ice"
    From      string `json:"from"`       // Sender user ID
    To        string `json:"to"`         // Target user ID
    SessionID string `json:"session_id"` // Unique session identifier
    
    // For offer/answer
    SDP string `json:"sdp,omitempty"`
    
    // For ICE candidates
    Candidate     string `json:"candidate,omitempty"`
    SDPMid        string `json:"sdp_mid,omitempty"`
    SDPMLineIndex int    `json:"sdp_mline_index,omitempty"`
}
```

---

## 6. Server Runtime (Go)

### 6.1 Main Entry Point

```go
// nakama/data/modules/main.go

package main

import (
    "context"
    "database/sql"
    
    "github.com/heroiclabs/nakama-common/runtime"
)

func InitModule(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, initializer runtime.Initializer) error {
    logger.Info("Mello backend initializing...")
    
    // Register authentication hooks
    if err := initializer.RegisterAfterAuthenticateEmail(AfterAuthenticateEmail); err != nil {
        return err
    }
    if err := initializer.RegisterAfterAuthenticateCustom(AfterAuthenticateDiscord); err != nil {
        return err
    }
    
    // Register group (crew) hooks
    if err := initializer.RegisterAfterJoinGroup(AfterJoinCrew); err != nil {
        return err
    }
    if err := initializer.RegisterAfterLeaveGroup(AfterLeaveCrew); err != nil {
        return err
    }
    
    // Register RPC functions
    if err := initializer.RegisterRpc("create_crew", CreateCrewRPC); err != nil {
        return err
    }
    if err := initializer.RegisterRpc("get_ice_servers", GetIceServersRPC); err != nil {
        return err
    }
    if err := initializer.RegisterRpc("start_stream", StartStreamRPC); err != nil {
        return err
    }
    if err := initializer.RegisterRpc("stop_stream", StopStreamRPC); err != nil {
        return err
    }
    
    // Register match handler for signaling
    if err := initializer.RegisterMatch("signaling", NewSignalingMatch); err != nil {
        return err
    }
    
    logger.Info("Mello backend initialized successfully")
    return nil
}
```

### 6.2 Crew Management

```go
// nakama/data/modules/crews.go

package main

import (
    "context"
    "database/sql"
    "encoding/json"
    
    "github.com/heroiclabs/nakama-common/api"
    "github.com/heroiclabs/nakama-common/runtime"
)

const (
    MaxCrewMembers = 6
    MaxCrewsPerUser = 10
)

type CreateCrewRequest struct {
    Name        string `json:"name"`
    Description string `json:"description,omitempty"`
    InviteOnly  bool   `json:"invite_only"`
}

type CreateCrewResponse struct {
    CrewID string `json:"crew_id"`
}

func CreateCrewRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
    if !ok {
        return "", runtime.NewError("authentication required", 16) // UNAUTHENTICATED
    }
    
    var req CreateCrewRequest
    if err := json.Unmarshal([]byte(payload), &req); err != nil {
        return "", runtime.NewError("invalid request", 3) // INVALID_ARGUMENT
    }
    
    // Validate name
    if len(req.Name) < 2 || len(req.Name) > 32 {
        return "", runtime.NewError("name must be 2-32 characters", 3)
    }
    
    // Check user's crew count
    groups, _, err := nk.UserGroupsList(ctx, userID, 100, nil, "")
    if err != nil {
        return "", runtime.NewError("failed to check user groups", 13) // INTERNAL
    }
    if len(groups) >= MaxCrewsPerUser {
        return "", runtime.NewError("maximum crews reached", 9) // FAILED_PRECONDITION
    }
    
    // Create crew metadata
    metadata := CrewMetadata{
        MaxMembers:    MaxCrewMembers,
        InviteOnly:    req.InviteOnly,
        CreatedBy:     userID,
        StreamEnabled: true,
    }
    metadataJSON, _ := json.Marshal(metadata)
    
    // Create Nakama group
    group, err := nk.GroupCreate(ctx, 
        userID,                    // Creator
        req.Name,                  // Name
        userID,                    // Creator ID as unique name suffix
        "en",                      // Language
        req.Description,           // Description
        "",                        // Avatar URL
        !req.InviteOnly,           // Open (opposite of invite only)
        string(metadataJSON),      // Metadata
        MaxCrewMembers,            // Max count
    )
    if err != nil {
        return "", runtime.NewError("failed to create crew", 13)
    }
    
    resp := CreateCrewResponse{CrewID: group.Id}
    respJSON, _ := json.Marshal(resp)
    return string(respJSON), nil
}

// Called after a user joins a crew
func AfterJoinCrew(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, out *api.JoinGroup, in *api.JoinGroup) error {
    userID := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
    crewID := in.GroupId
    
    logger.Info("User %s joined crew %s", userID, crewID)
    
    // Broadcast presence update to crew channel
    // ... (handled by client via Nakama's built-in presence)
    
    return nil
}

// Called after a user leaves a crew
func AfterLeaveCrew(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, out *api.LeaveGroup, in *api.LeaveGroup) error {
    userID := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
    crewID := in.GroupId
    
    logger.Info("User %s left crew %s", userID, crewID)
    
    return nil
}
```

### 6.3 ICE Server Configuration

```go
// nakama/data/modules/signaling.go

package main

import (
    "context"
    "database/sql"
    "encoding/json"
    "os"
    "time"
    
    "github.com/heroiclabs/nakama-common/runtime"
)

type IceServer struct {
    URLs       []string `json:"urls"`
    Username   string   `json:"username,omitempty"`
    Credential string   `json:"credential,omitempty"`
}

type GetIceServersResponse struct {
    IceServers []IceServer `json:"ice_servers"`
    TTL        int         `json:"ttl"` // Seconds until credentials expire
}

func GetIceServersRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    userID, ok := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
    if !ok {
        return "", runtime.NewError("authentication required", 16)
    }
    
    // STUN servers (free, no auth needed)
    stunServers := IceServer{
        URLs: []string{
            "stun:stun.l.google.com:19302",
            "stun:stun1.l.google.com:19302",
        },
    }
    
    // TURN servers (require time-limited credentials)
    turnSecret := os.Getenv("TURN_SECRET")
    turnHost := os.Getenv("TURN_HOST")
    
    // Generate time-limited TURN credentials
    // Username format: timestamp:userID
    // Credential: HMAC-SHA1(secret, username)
    timestamp := time.Now().Add(24 * time.Hour).Unix()
    username := fmt.Sprintf("%d:%s", timestamp, userID)
    credential := generateTurnCredential(turnSecret, username)
    
    turnServer := IceServer{
        URLs: []string{
            fmt.Sprintf("turn:%s:3478?transport=udp", turnHost),
            fmt.Sprintf("turn:%s:3478?transport=tcp", turnHost),
            fmt.Sprintf("turns:%s:5349?transport=tcp", turnHost),
        },
        Username:   username,
        Credential: credential,
    }
    
    resp := GetIceServersResponse{
        IceServers: []IceServer{stunServers, turnServer},
        TTL:        86400, // 24 hours
    }
    
    respJSON, _ := json.Marshal(resp)
    return string(respJSON), nil
}

func generateTurnCredential(secret, username string) string {
    mac := hmac.New(sha1.New, []byte(secret))
    mac.Write([]byte(username))
    return base64.StdEncoding.EncodeToString(mac.Sum(nil))
}
```

### 6.4 Stream Announcements

```go
// nakama/data/modules/streaming.go

package main

import (
    "context"
    "database/sql"
    "encoding/json"
    
    "github.com/heroiclabs/nakama-common/runtime"
)

type StartStreamRequest struct {
    CrewID string `json:"crew_id"`
    Title  string `json:"title,omitempty"`
}

type StreamAnnouncement struct {
    Type     string `json:"type"` // "stream_start" or "stream_end"
    HostID   string `json:"host_id"`
    HostName string `json:"host_name"`
    Title    string `json:"title"`
}

func StartStreamRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    userID := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
    
    var req StartStreamRequest
    if err := json.Unmarshal([]byte(payload), &req); err != nil {
        return "", runtime.NewError("invalid request", 3)
    }
    
    // Verify user is member of crew
    membership, err := nk.GroupUsersList(ctx, req.CrewID, 100, nil, "")
    if err != nil {
        return "", runtime.NewError("crew not found", 5) // NOT_FOUND
    }
    
    isMember := false
    for _, m := range membership {
        if m.User.Id == userID {
            isMember = true
            break
        }
    }
    if !isMember {
        return "", runtime.NewError("not a crew member", 7) // PERMISSION_DENIED
    }
    
    // Get user info
    users, err := nk.UsersGetId(ctx, []string{userID}, nil)
    if err != nil || len(users) == 0 {
        return "", runtime.NewError("user not found", 13)
    }
    user := users[0]
    
    // Broadcast stream start to crew channel
    announcement := StreamAnnouncement{
        Type:     "stream_start",
        HostID:   userID,
        HostName: user.DisplayName,
        Title:    req.Title,
    }
    announcementJSON, _ := json.Marshal(announcement)
    
    // Send to crew channel
    channelID := fmt.Sprintf("crew.%s", req.CrewID)
    nk.ChannelMessageSend(ctx, 
        channelID,
        string(announcementJSON),
        userID,
        user.Username,
        false, // Not persistent (real-time only)
    )
    
    return "{}", nil
}

func StopStreamRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    userID := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
    
    var req StartStreamRequest // Same structure
    if err := json.Unmarshal([]byte(payload), &req); err != nil {
        return "", runtime.NewError("invalid request", 3)
    }
    
    // Get user info
    users, err := nk.UsersGetId(ctx, []string{userID}, nil)
    if err != nil || len(users) == 0 {
        return "", runtime.NewError("user not found", 13)
    }
    user := users[0]
    
    // Broadcast stream end
    announcement := StreamAnnouncement{
        Type:     "stream_end",
        HostID:   userID,
        HostName: user.DisplayName,
    }
    announcementJSON, _ := json.Marshal(announcement)
    
    channelID := fmt.Sprintf("crew.%s", req.CrewID)
    nk.ChannelMessageSend(ctx, channelID, string(announcementJSON), userID, user.Username, false)
    
    return "{}", nil
}
```

---

## 7. Client-Server Communication

### 7.1 Authentication Flow

```
┌────────┐                    ┌────────┐                    ┌────────┐
│ Client │                    │ Nakama │                    │Discord │
└───┬────┘                    └───┬────┘                    └───┬────┘
    │                             │                             │
    │ 1. Open Discord OAuth URL   │                             │
    │────────────────────────────▶│                             │
    │                             │                             │
    │                             │ 2. Redirect to Discord      │
    │◀────────────────────────────│────────────────────────────▶│
    │                             │                             │
    │ 3. User authorizes          │                             │
    │──────────────────────────────────────────────────────────▶│
    │                             │                             │
    │ 4. Callback with code       │                             │
    │◀──────────────────────────────────────────────────────────│
    │                             │                             │
    │ 5. AuthenticateCustom       │                             │
    │   (provider: "discord",     │                             │
    │    token: code)             │                             │
    │────────────────────────────▶│                             │
    │                             │                             │
    │                             │ 6. Exchange code for token  │
    │                             │────────────────────────────▶│
    │                             │                             │
    │                             │ 7. Get user info            │
    │                             │◀────────────────────────────│
    │                             │                             │
    │ 8. Session token            │                             │
    │◀────────────────────────────│                             │
    │                             │                             │
```

### 7.2 Real-Time Communication

```
┌────────┐                              ┌────────┐
│ Client │                              │ Nakama │
└───┬────┘                              └───┬────┘
    │                                       │
    │ 1. Connect WebSocket                  │
    │──────────────────────────────────────▶│
    │                                       │
    │ 2. Join crew channel                  │
    │   channel_join("crew.{crew_id}")      │
    │──────────────────────────────────────▶│
    │                                       │
    │ 3. Receive presence updates           │
    │◀──────────────────────────────────────│
    │                                       │
    │ 4. Send chat message                  │
    │   channel_message_send(...)           │
    │──────────────────────────────────────▶│
    │                                       │
    │ 5. Receive chat messages (broadcast)  │
    │◀──────────────────────────────────────│
    │                                       │
    │ 6. Send signaling message             │
    │   channel_message_send(signal)        │
    │──────────────────────────────────────▶│
    │                                       │
    │ 7. Receive signaling (to specific)    │
    │◀──────────────────────────────────────│
    │                                       │
```

### 7.3 P2P Signaling Flow

```
┌─────────┐              ┌────────┐              ┌─────────┐
│  Peer A │              │ Nakama │              │  Peer B │
└────┬────┘              └───┬────┘              └────┬────┘
     │                       │                       │
     │ 1. Create offer       │                       │
     │ (local SDP)           │                       │
     │                       │                       │
     │ 2. Send offer via     │                       │
     │    channel message    │                       │
     │──────────────────────▶│                       │
     │                       │                       │
     │                       │ 3. Forward to Peer B  │
     │                       │──────────────────────▶│
     │                       │                       │
     │                       │                       │ 4. Create answer
     │                       │                       │    (local SDP)
     │                       │                       │
     │                       │ 5. Send answer        │
     │                       │◀──────────────────────│
     │                       │                       │
     │ 6. Forward to Peer A  │                       │
     │◀──────────────────────│                       │
     │                       │                       │
     │ 7. Exchange ICE candidates (both directions) │
     │◀─────────────────────▶│◀─────────────────────▶│
     │                       │                       │
     │                       │                       │
     │═══════════════════════════════════════════════│
     │              P2P Connection Established       │
     │═══════════════════════════════════════════════│
     │                       │                       │
```

---

## 8. TURN Server Configuration

### 8.1 Coturn Config

```conf
# config/turn.conf

# Network
listening-port=3478
tls-listening-port=5349
listening-ip=0.0.0.0
external-ip=YOUR_PUBLIC_IP

# Relay
min-port=49152
max-port=65535
relay-ip=YOUR_PUBLIC_IP

# Authentication
lt-cred-mech
use-auth-secret
static-auth-secret=YOUR_TURN_SECRET

# TLS (production)
cert=/etc/ssl/certs/turn.pem
pkey=/etc/ssl/private/turn.key

# Logging
log-file=/var/log/turnserver.log
verbose

# Security
no-multicast-peers
denied-peer-ip=10.0.0.0-10.255.255.255
denied-peer-ip=192.168.0.0-192.168.255.255
denied-peer-ip=172.16.0.0-172.31.255.255
denied-peer-ip=127.0.0.0-127.255.255.255

# Realm
realm=mello.app

# Quotas (per user)
user-quota=12
total-quota=1200
```

---

## 9. Scaling Considerations

### 9.1 Beta (Up to 10,000 users)

```
┌─────────────────────────────────────────┐
│          SINGLE REGION SETUP            │
│                                         │
│  1x Nakama (4 vCPU, 8GB RAM)            │
│  1x PostgreSQL (2 vCPU, 4GB RAM)        │
│  1x TURN (2 vCPU, 4GB RAM, 1Gbps)       │
│                                         │
│  Estimated cost: ~$150-300/mo           │
└─────────────────────────────────────────┘
```

### 9.2 Growth (100,000+ users)

```
┌─────────────────────────────────────────────────────────────────────┐
│                    MULTI-REGION SETUP                               │
│                                                                     │
│  ┌─────────────────────┐    ┌─────────────────────┐                │
│  │     US-EAST         │    │     EU-WEST         │                │
│  │                     │    │                     │                │
│  │  3x Nakama (HA)     │    │  3x Nakama (HA)     │                │
│  │  CockroachDB node   │◀──▶│  CockroachDB node   │                │
│  │  2x TURN            │    │  2x TURN            │                │
│  └─────────────────────┘    └─────────────────────┘                │
│              │                        │                            │
│              └────────┬───────────────┘                            │
│                       │                                            │
│               Global Load Balancer                                 │
│                                                                    │
│  Estimated cost: ~$2,000-5,000/mo                                  │
└─────────────────────────────────────────────────────────────────────┘
```

---

## 10. Monitoring & Observability

### 10.1 Metrics to Track

| Metric | Source | Alert Threshold |
|--------|--------|-----------------|
| WebSocket connections | Nakama | >90% capacity |
| Message throughput | Nakama | >10k/s per instance |
| P2P success rate | Application | <85% |
| TURN relay usage | Coturn | >30% of connections |
| TURN bandwidth | Coturn | >80% capacity |
| Database connections | PostgreSQL | >80% pool |
| API latency (p99) | Nakama | >500ms |

### 10.2 Logging

```yaml
# docker-compose.yml addition for logging

  loki:
    image: grafana/loki:2.9.0
    ports:
      - "3100:3100"
    volumes:
      - loki_data:/loki

  grafana:
    image: grafana/grafana:10.2.0
    ports:
      - "3000:3000"
    volumes:
      - grafana_data:/var/lib/grafana
    environment:
      - GF_SECURITY_ADMIN_PASSWORD=admin

volumes:
  loki_data:
  grafana_data:
```

---

## 11. Security

| Aspect | Implementation |
|--------|----------------|
| Transport | WSS (TLS 1.3) |
| Authentication | Nakama JWT tokens |
| Session | 24h expiry, refresh tokens |
| TURN credentials | Time-limited HMAC |
| Rate limiting | Nakama built-in + nginx |
| DDoS protection | Cloudflare |

---

## 12. API Reference Summary

### REST Endpoints (via Nakama)

| Method | Path | Description |
|--------|------|-------------|
| POST | `/v2/account/authenticate/email` | Email login |
| POST | `/v2/account/authenticate/custom` | Discord OAuth |
| GET | `/v2/user` | Get current user |
| GET | `/v2/group` | List user's crews |
| POST | `/v2/group` | Create crew |
| POST | `/v2/group/{id}/join` | Join crew |
| POST | `/v2/group/{id}/leave` | Leave crew |
| POST | `/v2/rpc/create_crew` | Create crew (custom) |
| POST | `/v2/rpc/get_ice_servers` | Get TURN credentials |
| POST | `/v2/rpc/start_stream` | Announce stream start |
| POST | `/v2/rpc/stop_stream` | Announce stream end |

### WebSocket Messages

| Type | Direction | Description |
|------|-----------|-------------|
| `channel_join` | Client→Server | Join crew channel |
| `channel_leave` | Client→Server | Leave crew channel |
| `channel_message_send` | Client→Server | Send chat/signal |
| `channel_message` | Server→Client | Receive chat/signal |
| `presence_event` | Server→Client | Member join/leave |
| `status_update` | Client→Server | Update presence |

---

*This spec defines the backend. For development setup, see [05-GETTING-STARTED.md](./05-GETTING-STARTED.md).*
