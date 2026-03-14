# MELLO Presence & Crew State Implementation

> **Component:** Presence, Crew State, Real-Time Updates  
> **Version:** 0.3  
> **Status:** Implemented

---

## 1. Overview

Mello's sidebar shows live activity for all crews a user belongs to. This requires a tiered presence system that balances real-time accuracy with efficiency.

```
┌─────────────────────────────────────────────────────────────────────────┐
│                                                                         │
│   ┌─────────────────────┐                                               │
│   │    USER PRESENCE    │  Per-user: status, activity, last_seen        │
│   └──────────┬──────────┘                                               │
│              │                                                          │
│              ▼                                                          │
│   ┌─────────────────────┐                                               │
│   │  CREW STATE MANAGER │  Per-crew: aggregates presence + activity     │
│   └──────────┬──────────┘                                               │
│              │                                                          │
│              ▼                                                          │
│   ┌─────────────────────┐                                               │
│   │    PUSH MANAGER     │  Per-user: what to push, when                 │
│   └──────────┬──────────┘                                               │
│              │                                                          │
│         ┌────┴────┐                                                     │
│         ▼         ▼                                                     │
│   ┌───────────┐ ┌───────────┐                                           │
│   │  ACTIVE   │ │  SIDEBAR  │                                           │
│   │  CREW     │ │  CREWS    │                                           │
│   │           │ │           │                                           │
│   │ Real-time │ │ Batched + │                                           │
│   │ all data  │ │ priority  │                                           │
│   └───────────┘ └───────────┘                                           │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

**Key concepts:**

| Concept | Description |
|---------|-------------|
| **Active Crew** | The crew user is currently viewing. Gets all updates in real-time. |
| **Sidebar Crews** | Other crews in the sidebar. Get batched summaries + priority events. |
| **Idle Crews** | Crews with no online members. Minimal updates until activity. |
| **Priority Events** | Stream start, voice join, mentions, DMs — always immediate. |

**Update cadence:**

| Data | Active Crew | Sidebar Crew |
|------|-------------|--------------|
| Online count | Instant | 30s batch |
| Voice members | Instant | Instant (join/leave only) |
| Speaking indicator | Instant | — |
| Stream status | Instant | Instant |
| Stream thumbnail | Instant | 30s batch |
| All messages | Instant | — |
| Last 2 messages | Instant | 10s throttle |
| Member presence | Instant | — |
| Mentions/DMs | Instant | Instant |

---

## 2. Data Models

### 2.1 User Presence

Stored in Nakama storage: `presence/{user_id}`

```json
{
  "user_id": "user_abc",
  "status": "online",
  "last_seen": "2026-03-08T14:15:00Z",
  "activity": {
    "type": "in_voice",
    "crew_id": "crew_xyz",
    "channel_id": "ch_abc12345",
    "channel_name": "General"
  },
  "updated_at": "2026-03-08T14:16:00Z"
}
```

**Status values:** `online`, `idle`, `dnd`, `offline`

**Activity types:**

| Type | Fields | Description |
|------|--------|-------------|
| `none` | — | Just online, not doing anything |
| `in_voice` | `crew_id`, `channel_id`, `channel_name` | In a voice channel |
| `streaming` | `crew_id`, `stream_id`, `stream_title` | Streaming |
| `watching` | `crew_id`, `stream_id`, `streamer_id` | Watching someone |

### 2.2 Crew State

Stored in Nakama storage: `crew_state/{crew_id}`

Server computes and caches this aggregate. Voice state is represented as a
`voice_channels` array — one entry per channel defined for the crew. Whether
voice is "active" for the crew is derived from whether any channel has members
(no top-level `voice.active` boolean).

```json
{
  "crew_id": "crew_xyz",
  "name": "Neon Syndicate",
  
  "counts": {
    "online": 4,
    "total": 20
  },
  
  "voice_channels": [
    {
      "id": "ch_abc12345",
      "name": "General",
      "is_default": true,
      "sort_order": 0,
      "active": true,
      "members": [
        { "user_id": "user_a", "username": "vex_r", "speaking": true },
        { "user_id": "user_b", "username": "lune", "speaking": false }
      ]
    },
    {
      "id": "ch_def67890",
      "name": "Strategy",
      "is_default": false,
      "sort_order": 1,
      "active": false,
      "members": []
    }
  ],
  
  "voice": {
    "active": true,
    "members": [...]
  },
  
  "stream": {
    "active": true,
    "stream_id": "stream_123",
    "streamer_id": "user_c",
    "streamer_username": "k0ji_tech",
    "title": "PROJECT AVALON",
    "viewer_count": 3,
    "thumbnail_url": "https://...",
    "thumbnail_updated_at": "2026-03-08T14:15:30Z"
  },
  
  "recent_messages": [
    {
      "message_id": "msg_1",
      "user_id": "user_a",
      "username": "vex_r",
      "preview": "yo who has the stash coordinat...",
      "timestamp": "2026-03-08T14:15:00Z"
    },
    {
      "message_id": "msg_2",
      "user_id": "user_b",
      "username": "lune",
      "preview": "check the drop box, i pinned it",
      "timestamp": "2026-03-08T14:14:30Z"
    }
  ],
  
  "updated_at": "2026-03-08T14:16:00Z"
}
```

> **Note:** The legacy flat `voice` object is still included for backward
> compatibility but should be considered deprecated. New clients should read
> `voice_channels` exclusively.

### 2.3 User Subscription State

In-memory on server (per-session, keyed by `sessionID`):

```go
type UserSubscription struct {
    UserID       string
    SessionID    string
    ActiveCrew   string
    SidebarCrews map[string]bool // set of crew IDs
}
```

A reverse index (`crewSubscribers: map[crewID → set[sessionID]`) enables fast
lookup of all sessions subscribed to a given crew.

### 2.4 Voice State

In-memory, real-time. Rooms are keyed by **channel ID** (not crew ID), since a
crew has multiple voice channels:

```go
voiceRooms: map[channelID → *VoiceRoom]
```

```go
type VoiceMemberState struct {
    UserID   string `json:"user_id"`
    Username string `json:"username"`
    Speaking bool   `json:"speaking"`
    Muted    bool   `json:"muted"`
    Deafened bool   `json:"deafened"`
}

type VoiceRoom struct {
    ChannelID string                       `json:"channel_id"`
    CrewID    string                       `json:"crew_id"`
    Members   map[string]*VoiceMemberState // keyed by user_id
}
```

**Reverse maps:**

| Map | Type | Purpose |
|-----|------|---------|
| `voiceUserChannel` | `map[userID → channelID]` | Which channel a user is in (fast leave/cleanup) |
| `voiceChannelCrew` | `map[channelID → crewID]` | Which crew a channel belongs to (resolve crew on leave) |

Each map has its own `sync.RWMutex` for concurrent access.

### 2.5 Stream Thumbnail

Stored in object storage or cache:

```
thumbnails/{stream_id}/latest.jpg
thumbnails/{stream_id}/{timestamp}.jpg  (historical, optional)
```

Metadata in Nakama storage: `stream_meta/{stream_id}`

```json
{
  "stream_id": "stream_123",
  "crew_id": "crew_xyz",
  "streamer_id": "user_c",
  "title": "PROJECT AVALON",
  "started_at": "2026-03-08T14:00:00Z",
  "thumbnail_url": "https://storage.mello.app/thumbnails/stream_123/latest.jpg",
  "thumbnail_updated_at": "2026-03-08T14:15:30Z",
  "viewer_ids": ["user_a", "user_b", "user_d"]
}
```

---

## 3. Backend Modules

### 3.1 File Structure

All Go modules live in a single flat package (required by Nakama runtime plugin loader):

```
backend/nakama/data/modules/
├── main.go              # InitModule, HealthCheckRPC, globals
├── presence.go          # UserPresence types, storage, RPCs, session hooks
├── crew_state.go        # CrewState aggregation, cache, RPCs, chat hook
├── push.go              # PushManager, subscriptions, batcher, throttle
├── voice_state.go       # In-memory voice rooms, RPCs, cleanup
├── voice_channels.go    # Voice channel definitions, CRUD RPCs, storage
├── streaming.go         # Stream start/stop, thumbnail upload RPC
├── crews.go             # create_crew RPC, group hooks
└── ice.go               # ICE server RPC
```

### 3.2 Module Registration

```go
// main.go — flat package, all symbols at package level

func InitModule(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, initializer runtime.Initializer) error {

    // Session hooks
    initializer.RegisterEventSessionStart(OnSessionStart)
    initializer.RegisterEventSessionEnd(OnSessionEnd)

    // Chat hooks (generic AfterRt — no typed AfterChannelMessageSend in runtime v1.31)
    initializer.RegisterAfterRt("ChannelMessageSend", OnChatMessage)

    // Group (crew) hooks
    initializer.RegisterAfterJoinGroup(AfterJoinCrew)
    initializer.RegisterAfterLeaveGroup(AfterLeaveCrew)

    // Auth hooks
    initializer.RegisterAfterAuthenticateEmail(AfterAuthenticateEmail)

    // RPCs — presence
    initializer.RegisterRpc("presence_update", PresenceUpdateRPC)
    initializer.RegisterRpc("presence_get", PresenceGetRPC)

    // RPCs — crew state
    initializer.RegisterRpc("crew_state_get", CrewStateGetRPC)
    initializer.RegisterRpc("crew_state_get_sidebar", CrewStateGetSidebarRPC)

    // RPCs — subscriptions (push)
    initializer.RegisterRpc("set_active_crew", SetActiveCrewRPC)
    initializer.RegisterRpc("subscribe_sidebar", SubscribeSidebarRPC)

    // RPCs — voice
    initializer.RegisterRpc("voice_join", VoiceJoinRPC)
    initializer.RegisterRpc("voice_leave", VoiceLeaveRPC)
    initializer.RegisterRpc("voice_speaking", VoiceSpeakingRPC)

    // RPCs — voice channels (CRUD)
    initializer.RegisterRpc("channel_create", ChannelCreateRPC)
    initializer.RegisterRpc("channel_rename", ChannelRenameRPC)
    initializer.RegisterRpc("channel_delete", ChannelDeleteRPC)
    initializer.RegisterRpc("channel_reorder", ChannelReorderRPC)

    // RPCs — streaming
    initializer.RegisterRpc("start_stream", StartStreamRPC)
    initializer.RegisterRpc("stop_stream", StopStreamRPC)
    initializer.RegisterRpc("stream_thumbnail_upload", StreamThumbnailUploadRPC)
    initializer.RegisterRpc("get_ice_servers", GetIceServersRPC)

    // RPCs — crews
    initializer.RegisterRpc("create_crew", CreateCrewRPC)

    // Background goroutines
    go StartSidebarBatchLoop(nk, logger, 30*time.Second)
    go StartMessageThrottleLoop(nk, logger, 10*time.Second)

    return nil
}
```

---

## 4. RPC Specifications

### 4.1 Presence RPCs

#### `presence_update`

Update current user's presence.

**Request:**
```json
{
  "status": "online",
  "activity": {
    "type": "streaming",
    "crew_id": "crew_xyz",
    "stream_id": "stream_123",
    "stream_title": "PROJECT AVALON"
  }
}
```

**Response:**
```json
{
  "success": true
}
```

#### `presence_get`

Get presence for specific users.

**Request:**
```json
{
  "user_ids": ["user_a", "user_b", "user_c"]
}
```

**Response:**
```json
{
  "presences": {
    "user_a": {
      "status": "online",
      "activity": { "type": "in_voice", "crew_id": "crew_xyz", "channel_id": "ch_abc12345", "channel_name": "General" }
    },
    "user_b": {
      "status": "online",
      "activity": { "type": "none" }
    },
    "user_c": {
      "status": "offline",
      "last_seen": "2026-03-08T12:00:00Z"
    }
  }
}
```

### 4.2 Crew State RPCs

#### `crew_state_get`

Get full state for a single crew (used for active crew).

**Request:**
```json
{
  "crew_id": "crew_xyz"
}
```

**Response:**
```json
{
  "crew_id": "crew_xyz",
  "name": "The Vanguard",
  "counts": {
    "online": 8,
    "total": 12
  },
  "members": [
    {
      "user_id": "user_a",
      "username": "k0ji_tech",
      "avatar": "...",
      "presence": {
        "status": "online",
        "activity": { "type": "streaming", "stream_id": "..." }
      }
    }
  ],
  "voice_channels": [
    {
      "id": "ch_abc12345",
      "name": "General",
      "is_default": true,
      "sort_order": 0,
      "active": true,
      "members": [
        { "user_id": "user_a", "username": "k0ji_tech", "speaking": false },
        { "user_id": "user_b", "username": "ash_22", "speaking": false }
      ]
    },
    {
      "id": "ch_def67890",
      "name": "Strategy",
      "is_default": false,
      "sort_order": 1,
      "active": false,
      "members": []
    }
  ],
  "voice": {
    "active": true,
    "members": [...]
  },
  "stream": {
    "active": true,
    "stream_id": "stream_123",
    "streamer_id": "user_a",
    "streamer_username": "k0ji_tech",
    "title": "PROJECT AVALON",
    "viewer_count": 3,
    "thumbnail_url": "https://..."
  },
  "recent_messages": [
    { "username": "ash_22", "preview": "status check?", "timestamp": "..." },
    { "username": "m1ra", "preview": "All systems clear...", "timestamp": "..." }
  ]
}
```

> **Note:** `voice` (legacy flat object) is kept for backward compatibility.
> New clients should use `voice_channels`.

#### `crew_state_get_sidebar`

Get summary state for multiple crews (sidebar view).

**Request:**
```json
{
  "crew_ids": ["crew_abc", "crew_def", "crew_123"]
}
```

**Response:**
```json
{
  "crews": [
    {
      "crew_id": "crew_abc",
      "name": "Neon Syndicate",
      "counts": { "online": 4, "total": 20 },
      "voice_channels": [
        {
          "id": "ch_abc12345",
          "name": "General",
          "is_default": true,
          "members": [
            { "user_id": "...", "username": "vex_r" },
            { "user_id": "...", "username": "lune" }
          ]
        },
        {
          "id": "ch_def67890",
          "name": "Strategy",
          "is_default": false,
          "members": []
        }
      ],
      "stream": null,
      "recent_messages": [
        { "username": "vex_r", "preview": "yo who has the stash...", "timestamp": "..." },
        { "username": "lune", "preview": "check the drop box...", "timestamp": "..." }
      ],
      "unread": { "count": 5, "mentions": 0 }
    },
    {
      "crew_id": "crew_def",
      "name": "Deep Space",
      "counts": { "online": 2, "total": 6 },
      "voice_channels": [],
      "stream": {
        "active": true,
        "streamer_username": "nova_9",
        "title": "Exploring sector 7",
        "thumbnail_url": "https://..."
      },
      "recent_messages": [...],
      "unread": { "count": 0, "mentions": 0 }
    },
    {
      "crew_id": "crew_123",
      "name": "Ghost Recon",
      "counts": { "online": 0, "total": 8 },
      "idle": true
    }
  ]
}
```

> **Note:** Sidebar voice channel members do **not** include `speaking` state
> (same rule as before, now per-channel). Only active crew gets speaking.

### 4.3 Subscription RPCs

#### `set_active_crew`

Tell server which crew is currently focused.

**Request:**
```json
{
  "crew_id": "crew_xyz"
}
```

**Response:**
```json
{
  "success": true,
  "state": { ... }  // Full crew state returned immediately
}
```

#### `subscribe_sidebar`

Subscribe to sidebar updates for crews.

**Request:**
```json
{
  "crew_ids": ["crew_abc", "crew_def"]
}
```

**Response:**
```json
{
  "success": true,
  "crews": [ ... ]  // Initial sidebar state
}
```

### 4.4 Voice RPCs

#### `voice_join`

Join a voice channel. If the user is already in another channel, they are
automatically removed from the old channel first (implicit move).

If `channel_id` is omitted, the server picks the crew's default channel.

**Request:**
```json
{
  "crew_id": "crew_xyz",
  "channel_id": "ch_abc12345"
}
```

**Response:**
```json
{
  "success": true,
  "channel_id": "ch_abc12345",
  "voice_state": {
    "channel_id": "ch_abc12345",
    "active": true,
    "members": [
      { "user_id": "user_a", "username": "k0ji_tech", "speaking": false }
    ]
  }
}
```

#### `voice_leave`

Leave voice channel. No `channel_id` needed — server resolves it from the
`voiceUserChannel` reverse map.

**Request:**
```json
{
  "crew_id": "crew_xyz"
}
```

#### `voice_speaking`

Update speaking status (called frequently from client). No `channel_id`
needed — the server resolves the user's current channel from the
`voiceUserChannel` reverse map.

**Request:**
```json
{
  "crew_id": "crew_xyz",
  "speaking": true
}
```

### 4.5 Stream RPCs

#### `stream_thumbnail_upload`

Upload a stream thumbnail (called by streamer every 30s).

**Request:**
```json
{
  "stream_id": "stream_123",
  "thumbnail_base64": "..."
}
```

**Response:**
```json
{
  "success": true,
  "thumbnail_url": "https://storage.mello.app/thumbnails/stream_123/latest.jpg"
}
```

---

## 5. Client ↔ Server Communication

### 5.1 Client → Server (HTTP RPCs)

All client-to-server actions use Nakama HTTP RPCs (`POST /v2/rpc/{id}`), not raw
WebSocket messages. The mello-core `NakamaClient` has a generic `rpc()` helper
and thin typed wrappers for each call. See Section 4 for payloads.

| Action | RPC ID | Called when |
|--------|--------|------------|
| Update presence | `presence_update` | Login, logout, activity change |
| Get presence | `presence_get` | On demand |
| Set active crew | `set_active_crew` | Crew focused in UI |
| Subscribe sidebar | `subscribe_sidebar` | After crews loaded |
| Join voice | `voice_join` | User clicks channel / auto-join |
| Leave voice | `voice_leave` | User leaves voice or crew |
| Speaking update | `voice_speaking` | VAD state changes |
| Create channel | `channel_create` | Admin creates voice channel |
| Rename channel | `channel_rename` | Admin renames voice channel |
| Delete channel | `channel_delete` | Admin deletes voice channel |
| Reorder channels | `channel_reorder` | Admin reorders voice channels |

### 5.2 Server → Client (Nakama Notifications)

The server pushes updates to clients via Nakama notifications over the existing
WebSocket connection. Each notification carries a numeric `code` and a JSON
`content` string. The client WS reader dispatches on `code`.

```go
// Notification codes (push.go)
const (
    NotifyCrewState      = 110  // Full crew state
    NotifyCrewEvent      = 111  // Priority event (immediate)
    NotifySidebarUpdate  = 112  // Batched sidebar summary
    NotifyPresenceChange = 113  // Member presence changed
    NotifyVoiceUpdate    = 114  // Voice state changed
    NotifyMessagePreview = 115  // Throttled message preview
)
```

#### Code 110 — `crew_state`

Full crew state (sent on focus / request via `set_active_crew` RPC response, or
pushed when significant change occurs). Includes `voice_channels` array.

```json
{
  "crew_id": "crew_xyz",
  "counts": { "online": 8, "total": 12 },
  "members": [...],
  "voice_channels": [
    { "id": "ch_abc12345", "name": "General", "is_default": true, "members": [...] },
    { "id": "ch_def67890", "name": "Strategy", "is_default": false, "members": [] }
  ],
  "voice": {...},
  "stream": {...},
  "recent_messages": [...]
}
```

#### Code 111 — `crew_event`

Priority event (immediate to all subscribers).

```json
{
  "crew_id": "crew_xyz",
  "event": "stream_started",
  "data": {
    "stream_id": "stream_123",
    "streamer_id": "user_a",
    "streamer_username": "k0ji_tech",
    "title": "PROJECT AVALON"
  }
}
```

Event types:
- `stream_started`
- `stream_ended`
- `voice_joined` — includes `channel_id` and `channel_name` in `data`
- `voice_left` — includes `channel_id` and `channel_name` in `data`
- `channel_created` — a new voice channel was added
- `channel_renamed` — a voice channel was renamed
- `channel_deleted` — a voice channel was removed
- `mention` (always immediate)
- `dm_received` (always immediate)

**Example — voice_joined:**
```json
{
  "crew_id": "crew_xyz",
  "event": "voice_joined",
  "data": {
    "user_id": "user_a",
    "username": "k0ji_tech",
    "channel_id": "ch_abc12345",
    "channel_name": "General"
  }
}
```

#### Code 112 — `sidebar_update`

Batched sidebar update (every 30s). Includes `voice_channels` per crew (members
without speaking state).

```json
{
  "type": "sidebar_update",
  "crews": [
    {
      "crew_id": "crew_abc",
      "counts": { "online": 4, "total": 20 },
      "voice_channels": [
        {
          "id": "ch_abc12345",
          "name": "General",
          "is_default": true,
          "members": [
            { "user_id": "...", "username": "vex_r" },
            { "user_id": "...", "username": "lune" }
          ]
        }
      ],
      "stream": {
        "active": true,
        "thumbnail_url": "https://...",
        "thumbnail_updated_at": "..."
      }
    }
  ]
}
```

#### Code 113 — `presence_change`

Member presence changed (active crew only).

```json
{
  "crew_id": "crew_xyz",
  "user_id": "user_a",
  "presence": {
    "status": "online",
    "activity": { "type": "streaming", ... }
  }
}
```

#### Code 114 — `voice_update`

Voice state changed. Pushed to active crew subscribers and includes per-channel
data with speaking state. Also includes a legacy flat `members` list for
backward compatibility.

```json
{
  "type": "voice_update",
  "crew_id": "crew_xyz",
  "voice_channels": [
    {
      "id": "ch_abc12345",
      "name": "General",
      "is_default": true,
      "members": [
        { "user_id": "user_a", "username": "vex_r", "speaking": true },
        { "user_id": "user_b", "username": "lune", "speaking": false }
      ]
    },
    {
      "id": "ch_def67890",
      "name": "Strategy",
      "is_default": false,
      "members": []
    }
  ],
  "members": [
    { "user_id": "user_a", "username": "vex_r", "speaking": true },
    { "user_id": "user_b", "username": "lune", "speaking": false }
  ]
}
```

For active crew: includes speaking state per channel.
For sidebar: only join/leave events via crew_event (code 111), no voice_update push.

#### Code 115 — `message_preview`

Recent messages updated (throttled 10s for sidebar).

```json
{
  "crew_id": "crew_abc",
  "messages": [
    { "username": "vex_r", "preview": "new message here...", "timestamp": "..." },
    { "username": "lune", "preview": "previous message...", "timestamp": "..." }
  ]
}
```

---

## 6. Push Logic

All push logic lives in `push.go` as free functions operating on package-level
state (flat package requirement). Two background goroutines are started from
`InitModule`: the sidebar batcher (30s) and the message throttle flusher (10s).

### 6.1 Priority Event Handling

```go
// push.go

var PriorityEvents = map[string]bool{
    "stream_started":   true,
    "stream_ended":     true,
    "voice_joined":     true,
    "voice_left":       true,
    "channel_created":  true,
    "channel_renamed":  true,
    "channel_deleted":  true,
    "mention":          true,
    "dm_received":      true,
}

func PushCrewEvent(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule,
    crewID, event string, data map[string]interface{}) {
    // Send NotifyCrewEvent (code 111) to all subscribers of crewID
    for _, sub := range getSubscribersForCrew(crewID) {
        nk.NotificationSend(ctx, sub.UserID, "crew_event", content, NotifyCrewEvent, "", false)
    }
}
```

### 6.2 Batched Sidebar Updates

```go
// push.go — background goroutine started by InitModule

func StartSidebarBatchLoop(nk runtime.NakamaModule, logger runtime.Logger, interval time.Duration) {
    ticker := time.NewTicker(interval)
    for range ticker.C {
        FlushSidebarBatch(context.Background(), logger, nk)
    }
}

func FlushSidebarBatch(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule) {
    // For each session subscription, build sidebar state for their crews
    // and send NotifySidebarUpdate (code 112)
}
```

### 6.3 Message Throttling

```go
// push.go — throttled message previews (10s per crew)

var msgThrottle struct {
    mu       sync.Mutex
    lastPush map[string]time.Time      // crewID → last push time
    pending  map[string]*MessagePreview // crewID → latest pending message
}

func QueueMessagePreview(ctx context.Context, logger runtime.Logger, nk runtime.NakamaModule,
    crewID string, msg *MessagePreview) {
    // If ≥10s since last push: push immediately
    // Otherwise: store as pending, flushed by StartMessageThrottleLoop
}

func StartMessageThrottleLoop(nk runtime.NakamaModule, logger runtime.Logger, interval time.Duration) {
    ticker := time.NewTicker(interval)
    for range ticker.C {
        FlushThrottledMessages(context.Background(), logger, nk)
    }
}
```

---

## 7. Stream Thumbnails

> **TODO:** Client-side thumbnail capture (section 7.2) is deferred until the
> streaming feature is implemented. The server-side `stream_thumbnail_upload` RPC
> is ready. When streaming lands, implement `StreamManager::start_thumbnail_loop`
> in `mello-core/src/stream/` — capture frame from libmello encoder, resize to
> 320×180, JPEG encode, base64, upload via RPC every 30s.

### 7.1 Capture Flow

```
┌─────────────────────────────────────────────────────────────────────────┐
│                                                                         │
│   STREAMER CLIENT                        SERVER                         │
│                                                                         │
│   Every 30 seconds while streaming:                                     │
│                                                                         │
│   1. Capture current frame                                              │
│   2. Resize to 320x180                                                  │
│   3. Encode as JPEG (quality 70)                                        │
│   4. Base64 encode                ─────────────────────▶  5. Decode     │
│                                                           6. Store      │
│                                                           7. Update URL │
│                                                           8. Notify     │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### 7.2 Client Implementation

```rust
// mello-core/src/stream/thumbnail.rs

impl StreamManager {
    pub async fn start_thumbnail_loop(&self, stream_id: &str) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            
            if !self.is_streaming() {
                break;
            }
            
            // Capture frame from encoder
            if let Some(frame) = self.capture_thumbnail_frame() {
                // Resize and encode
                let thumbnail = resize_and_encode_jpeg(&frame, 320, 180, 70);
                let base64 = base64::encode(&thumbnail);
                
                // Upload
                if let Err(e) = self.upload_thumbnail(stream_id, &base64).await {
                    log::warn!("Thumbnail upload failed: {}", e);
                }
            }
        }
    }
}
```

### 7.3 Server Storage

```go
// stream/thumbnail.go

func ThumbnailUploadRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    userID, _ := ctx.Value(runtime.RUNTIME_CTX_USER_ID).(string)
    
    var req struct {
        StreamID        string `json:"stream_id"`
        ThumbnailBase64 string `json:"thumbnail_base64"`
    }
    json.Unmarshal([]byte(payload), &req)
    
    // Verify user owns this stream
    streamMeta, err := getStreamMeta(ctx, nk, req.StreamID)
    if err != nil || streamMeta.StreamerID != userID {
        return "", runtime.NewError("unauthorized", 7)
    }
    
    // Decode base64
    thumbnailBytes, err := base64.StdEncoding.DecodeString(req.ThumbnailBase64)
    if err != nil {
        return "", runtime.NewError("invalid thumbnail", 3)
    }
    
    // Store (implementation depends on storage backend)
    thumbnailURL, err := storeThumbnail(req.StreamID, thumbnailBytes)
    if err != nil {
        return "", err
    }
    
    // Update stream metadata
    streamMeta.ThumbnailURL = thumbnailURL
    streamMeta.ThumbnailUpdatedAt = time.Now()
    saveStreamMeta(ctx, nk, streamMeta)
    
    // Update crew state
    crewStateManager.OnThumbnailUpdated(streamMeta.CrewID, req.StreamID, thumbnailURL)
    
    return `{"success": true, "thumbnail_url": "` + thumbnailURL + `"}`, nil
}
```

### 7.4 Thumbnail in Sidebar Updates

Thumbnails are included in the 30s batched sidebar update:

```json
{
  "type": "sidebar_update",
  "crews": [
    {
      "crew_id": "crew_abc",
      "stream": {
        "active": true,
        "streamer_username": "k0ji_tech",
        "title": "PROJECT AVALON",
        "thumbnail_url": "https://storage.mello.app/thumbnails/stream_123/latest.jpg?t=1709912345",
        "viewer_count": 3
      }
    }
  ]
}
```

**Note:** Append timestamp query param to bust cache on client.

---

## 8. Client Integration

### 8.1 Rust Types

Types live in two flat modules: `mello-core/src/presence.rs` and
`mello-core/src/crew_state.rs`. All fields use `#[serde(default)]` liberally
so partial payloads from the server don't break deserialization.

```rust
// mello-core/src/presence.rs

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PresenceStatus { Online, Idle, Dnd, Offline }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Activity {
    None,
    InVoice { crew_id: String, channel_id: Option<String>, channel_name: Option<String> },
    Streaming { crew_id: String, stream_id: String, stream_title: String },
    Watching { crew_id: String, stream_id: String, streamer_id: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserPresence {
    pub user_id: String,
    pub status: PresenceStatus,
    pub last_seen: Option<String>,
    pub activity: Option<Activity>,
    pub updated_at: Option<String>,
}
```

```rust
// mello-core/src/crew_state.rs — full/sidebar state + push payloads

pub struct CrewState {
    pub crew_id: String,
    pub name: String,
    pub counts: CrewCounts,
    pub members: Option<Vec<CrewMember>>,  // Only for active crew
    pub voice: VoiceState,                 // Legacy — prefer voice_channels
    pub voice_channels: Vec<VoiceChannelState>,
    pub stream: Option<StreamState>,
    pub recent_messages: Vec<MessagePreview>,
    pub updated_at: Option<String>,
}

pub struct CrewCounts { pub online: u32, pub total: u32 }

pub struct CrewMember {
    pub user_id: String,
    pub username: String,
    pub avatar: Option<String>,
    pub presence: Option<UserPresence>,
}

pub struct VoiceState { pub active: bool, pub members: Vec<VoiceMember> }
pub struct VoiceMember { pub user_id: String, pub username: String, pub speaking: Option<bool> }

pub struct VoiceChannelState {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub members: Vec<VoiceMember>,
}

pub struct StreamState {
    pub active: bool,
    pub stream_id: Option<String>,
    pub streamer_id: Option<String>,
    pub streamer_username: Option<String>,
    pub title: Option<String>,
    pub viewer_count: u32,
    pub thumbnail_url: Option<String>,
}

pub struct MessagePreview { pub message_id: Option<String>, pub user_id: Option<String>, pub username: String, pub preview: String, pub timestamp: String }

// Sidebar (lighter)
pub struct CrewSidebarState {
    pub crew_id: String, pub name: String, pub counts: CrewCounts,
    pub voice: Option<VoiceState>,  // Legacy — prefer voice_channels
    pub voice_channels: Vec<VoiceChannelState>,
    pub stream: Option<StreamState>, pub recent_messages: Vec<MessagePreview>, pub idle: bool,
}

// Push payloads parsed from Nakama notification content
pub struct CrewEvent    { pub crew_id: String, pub event: String, pub data: serde_json::Value }
pub struct PresenceChange { pub crew_id: String, pub user_id: String, pub presence: PresenceInfo }
pub struct VoiceUpdate  { pub crew_id: String, pub channel_id: String, pub members: Vec<VoiceMember> }
pub struct MessagePreviewUpdate { pub crew_id: String, pub messages: Vec<MessagePreview> }
pub struct SidebarUpdate { pub crews: Vec<CrewSidebarState> }
```

### 8.2 Client Architecture

There is **no separate `CrewStateManager` object**. Instead, `mello-core`'s
`Client` (in `client.rs`) calls Nakama RPCs directly and emits `Event` variants
to the UI thread. Notification payloads from the WS reader are parsed and
dispatched as events in the same way.

**Commands** (UI → core):

```rust
// mello-core/src/command.rs
Command::UpdatePresence { status, activity }
Command::SetActiveCrew { crew_id }
Command::SubscribeSidebar { crew_ids }
Command::JoinVoice { channel_id }
Command::CreateVoiceChannel { name }
Command::RenameVoiceChannel { channel_id, name }
Command::DeleteVoiceChannel { channel_id }
```

**Events** (core → UI):

```rust
// mello-core/src/events.rs
Event::CrewStateLoaded { state: CrewState }
Event::SidebarUpdated { crews: Vec<CrewSidebarState> }
Event::CrewEventReceived { event: CrewEvent }
Event::PresenceChanged { change: PresenceChange }
Event::VoiceUpdated { crew_id, channel_id, members: Vec<VoiceMember> }
Event::VoiceChannelsUpdated { crew_id, channels: Vec<VoiceChannelState> }
Event::VoiceChannelCreated { crew_id, channel: VoiceChannelState }
Event::VoiceChannelRenamed { crew_id, channel_id, name }
Event::VoiceChannelDeleted { crew_id, channel_id }
Event::MessagePreviewUpdated { crew_id, messages: Vec<MessagePreview> }
```

### 8.3 Connection Lifecycle

Implemented in `mello-core/src/client.rs`:

```rust
// Called after successful auth + WS connect (handle_device_auth, handle_login, handle_restore)
async fn on_connected(&self) {
    // Set online presence via RPC
    self.nakama.presence_update(&PresenceStatus::Online, None).await;
}

// load_crews() — called after login/restore — subscribes sidebar for all crews
async fn load_crews(&self) {
    let crews = self.nakama.list_user_groups().await?;
    let crew_ids: Vec<String> = crews.iter().map(|c| c.id.clone()).collect();
    self.handle_subscribe_sidebar(&crew_ids).await;  // RPC + emit SidebarUpdated
    self.event_tx.send(Event::CrewsLoaded { crews });
}

// handle_select_crew() — joins channel + calls set_active_crew RPC → emits CrewStateLoaded
async fn handle_select_crew(&mut self, crew_id: &str) {
    self.nakama.join_crew_channel(crew_id).await;
    match self.nakama.set_active_crew(crew_id).await {
        Ok(state) => self.event_tx.send(Event::CrewStateLoaded { state }),
        Err(e)    => log::warn!("set_active_crew RPC failed: {}", e),
    }
    // ... also loads message history and follows users
}

// handle_logout() — sets offline presence before clearing session
async fn handle_logout(&mut self) {
    self.nakama.presence_update(&PresenceStatus::Offline, None).await;
    self.nakama.voice_leave(crew_id).await;
    // ... clears session, leaves channel
}
```

---

## 9. Event Flows

### 9.1 User Joins Voice

```
User A clicks a voice channel ("Strategy") in crew_xyz
    │
    ▼
Client: RPC voice_join(crew_xyz, ch_def67890)
    │
    ▼
Server:
    0. If user is already in a different channel → implicit leave
       (remove from old room, push voice_left event, update presence)
    1. Resolve channel — if channel_id omitted, pick default channel
    2. Check capacity (max 6 per channel)
    3. Add user to voiceRooms[ch_def67890]
    4. Update reverse maps: voiceUserChannel[user_a] = ch_def67890
    5. Update user presence: activity = { in_voice, channel_id, channel_name }
    6. Push to crew_xyz active subscribers:
       { type: "voice_update", voice_channels: [...] }
    7. Push to crew_xyz sidebar subscribers:
       { type: "crew_event", event: "voice_joined",
         data: { user_id: "A", channel_id: "ch_def67890", channel_name: "Strategy" } }
```

### 9.2 User Starts Streaming

```
User A starts stream in crew_xyz
    │
    ▼
Client: RPC stream_start(crew_xyz, title)
    │
    ▼
Server:
    1. Create stream record
    2. Update user presence: activity = streaming
    3. Update crew state: stream = { active: true, ... }
    4. Push to ALL crew_xyz subscribers (priority event):
       { type: "crew_event", event: "stream_started", data: { ... } }
    │
    ▼
Client (streamer): Start thumbnail capture loop (every 30s)
```

### 9.3 New Chat Message

```
User A sends message in crew_xyz
    │
    ▼
Server (after message stored):
    1. Update crew state: recent_messages = [new, prev]
    2. If crew_xyz is someone's active crew:
       → Push full message immediately
    3. If crew_xyz is someone's sidebar crew:
       → Check throttle (10s)
       → If ok: push message_preview
       → If throttled: queue for next window
```

### 9.4 Sidebar Batch Tick (Every 30s)

```
Timer fires
    │
    ▼
For each user:
    For each sidebar crew:
        │
        ▼
        Compute summary:
        - online_count
        - voice member count
        - stream thumbnail (if active)
        │
        ▼
        Queue in batcher
    │
    ▼
Flush batcher:
    Send sidebar_update to each user with their crews
```

---

## 10. Storage & State

| Location | Key | Kind | Purpose |
|----------|-----|------|---------|
| Nakama storage `presence` | `{user_id}` | Persistent | User presence state |
| Nakama storage `stream_meta` | `{stream_id}` | Persistent | Active stream metadata |
| Nakama storage `voice_channels` | `{crew_id}` | Persistent | Voice channel definitions per crew |
| In-memory `crewStateCache` | `{crew_id}` | Cache | Aggregated crew state (rebuilt on demand) |
| In-memory `voiceRooms` | `{channel_id}` | Runtime | Voice room membership per channel |
| In-memory `voiceUserChannel` | `{user_id}` | Runtime | Reverse: user → channel |
| In-memory `voiceChannelCrew` | `{channel_id}` | Runtime | Reverse: channel → crew |
| In-memory `subscriptions` | `{session_id}` | Runtime | Per-session push subscriptions |
| In-memory `crewSubscribers` | `{crew_id}` | Runtime | Reverse index: crew → sessions |

> **Note:** In-memory state is lost on Nakama restart. Presence is restored via
> session hooks; voice rooms start empty (users rejoin). This is acceptable for
> the current single-node deployment.

---

## 12. Testing

### 12.1 Unit Tests

- Presence status transitions
- Crew state aggregation
- Batch timing logic
- Throttle logic
- Priority event detection

### 12.2 Integration Tests

- User connect → presence online → crew counts update
- User joins voice → all subscribers notified
- Stream start → priority event to all
- Message send → throttled preview to sidebar
- Thumbnail upload → appears in sidebar update

### 12.3 Load Tests

- 100 users, 10 crews each
- Measure: event latency, server memory, bandwidth
- Target: <100ms for priority events, <35s for batched

### 12.4 Manual Test Cases

- [ ] Connect → sidebar shows correct online counts
- [ ] Switch active crew → old crew moves to sidebar mode
- [ ] Someone starts streaming → sidebar shows immediately
- [ ] Someone joins voice → sidebar shows immediately
- [ ] New messages → sidebar shows with ~10s delay
- [ ] Online count changes → sidebar shows with ~30s delay
- [ ] Thumbnail updates → visible in sidebar on next batch
- [ ] Disconnect → last_seen set, others see offline
- [ ] Reconnect → state restored correctly

---

*Implementation spec for presence and crew state. See [00-ARCHITECTURE.md](./00-ARCHITECTURE.md) for system context.*
