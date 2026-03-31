# MELLO Crew Event Ledger Specification

> **Component:** Crew Event Ledger  
> **Version:** 0.1  
> **Status:** Planned  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)  
> **Related:** [04-BACKEND.md](./04-BACKEND.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md), [13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md)

---

## 1. Overview

The crew event ledger records meaningful activity within a crew. It is the single data source that powers the catch-up card, post-game moments, and future features (activity feeds, weekly digests, year-in-review).

Events are **not** chat messages. They are structured signals derived from crew activity, stored in a rolling 7-day window per crew.

### Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Storage model | Single Nakama Storage document per crew | Avoids pagination, simple reads, Nakama has no range query |
| Rolling window | 7 days | Sufficient for catch-up; keeps document size bounded |
| Write pattern | Append + trim on every write | No background compaction jobs needed |
| Event ownership | System-owned (UserID = "") | Events belong to the crew, not any individual |
| Catch-up trigger | Client requests when `last_seen` for crew > threshold | No server-side scheduling; on-demand only |
| Catch-up threshold | 4 hours | Shorter than this, catch-up card not shown |
| AI summarization | v2 (premium, deferred) | v1 uses structured template composition |

### What This Spec Covers

- Event types and schemas
- Storage model and write path
- Catch-up generation (template-based, v1)
- RPCs and hooks
- Client integration

### What This Spec Does Not Cover

- AI-powered summarization (v2, separate spec)
- Push notifications for individual events (handled by spec 11)
- Game detection / IGDB integration (separate spec)

---

## 2. Event Types

Every event shares a common envelope:

```go
type CrewEvent struct {
    ID        string `json:"id"`         // ULID (sortable, unique)
    CrewID    string `json:"crew_id"`
    Type      string `json:"type"`       // Event type key
    ActorID   string `json:"actor_id"`   // User who triggered it (empty for system events)
    Timestamp int64  `json:"ts"`         // Unix ms
    Data      any    `json:"data"`       // Type-specific payload
    Score     int    `json:"score"`      // Priority score for catch-up ranking
}
```

### 2.1 Auto-Detected Events

These are written by server hooks with no user action required.

#### `voice_session`

Written when the last member leaves a voice channel that had 2+ participants.

```json
{
    "type": "voice_session",
    "score": 20,
    "data": {
        "channel_id": "ch_general",
        "channel_name": "General",
        "participant_ids": ["user_a", "user_b", "user_c"],
        "participant_names": ["ash", "koji", "nav"],
        "duration_min": 47,
        "peak_count": 3
    }
}
```

#### `stream_session`

Written when a stream ends.

```json
{
    "type": "stream_session",
    "score": 30,
    "data": {
        "streamer_id": "user_c",
        "streamer_name": "koji",
        "title": "PROJECT AVALON",
        "game": "Counter-Strike 2",
        "duration_min": 120,
        "peak_viewers": 4,
        "viewer_ids": ["user_a", "user_b", "user_d"]
    }
}
```

#### `game_session`

Written when a detected game session ends for a user in this crew. Requires game detection on client (spec TBD) to push session data to the server.

```json
{
    "type": "game_session",
    "score": 10,
    "data": {
        "game_name": "Counter-Strike 2",
        "game_igdb_id": 0,
        "player_ids": ["user_a", "user_b"],
        "player_names": ["ash", "koji"],
        "duration_min": 45
    }
}
```

**Note:** `game_igdb_id` is 0 until IGDB integration is implemented. The `game_name` comes from the client's process-detected game name.

#### `member_joined`

Written by the `AfterJoinGroup` hook.

```json
{
    "type": "member_joined",
    "score": 15,
    "data": {
        "username": "newplayer",
        "display_name": "NewPlayer"
    }
}
```

#### `member_left`

Written by the `AfterLeaveGroup` hook.

```json
{
    "type": "member_left",
    "score": 5,
    "data": {
        "username": "oldplayer",
        "display_name": "OldPlayer"
    }
}
```

#### `chat_activity`

Written by a periodic aggregator (every 30 min) when chat volume exceeds a threshold. Not per-message. No message content is stored.

```json
{
    "type": "chat_activity",
    "score": 5,
    "data": {
        "message_count": 47,
        "window_start": 1711400000000,
        "window_end": 1711401800000,
        "active_user_count": 4
    }
}
```

### 2.2 User-Contributed Events

#### `moment`

Written when a user shares a moment via the post-game flow or a future "share moment" action.

```json
{
    "type": "moment",
    "score": 40,
    "data": {
        "text": "40-bomb on Dust2",
        "sentiment": "highlight",
        "game_name": "Counter-Strike 2"
    }
}
```

**Sentiment values:** `win`, `loss`, `highlight`

A moment with no text (user only tapped a sentiment button) is still valid:

```json
{
    "type": "moment",
    "score": 25,
    "data": {
        "text": "",
        "sentiment": "win",
        "game_name": "Counter-Strike 2"
    }
}
```

Moments with text score higher than moments without text because they produce better catch-up summaries.

---

## 3. Storage Model

### 3.1 Nakama Storage Document

| Field | Value |
|-------|-------|
| Collection | `crew_events` |
| Key | `{crew_id}` |
| UserID | `""` (system-owned) |
| PermissionRead | `2` (public read) |
| PermissionWrite | `0` (server-only write) |

```go
type CrewEventLedger struct {
    CrewID    string      `json:"crew_id"`
    Events    []CrewEvent `json:"events"`
    UpdatedAt int64       `json:"updated_at"` // Unix ms
}
```

### 3.2 Write Path

Every event write follows this sequence:

```go
func AppendCrewEvent(ctx context.Context, nk runtime.NakamaModule, crewID string, event CrewEvent) error {
    // 1. Read current ledger
    ledger := readLedger(ctx, nk, crewID)

    // 2. Append new event
    ledger.Events = append(ledger.Events, event)

    // 3. Trim: remove events older than 7 days
    cutoff := time.Now().Add(-7 * 24 * time.Hour).UnixMilli()
    trimmed := make([]CrewEvent, 0, len(ledger.Events))
    for _, e := range ledger.Events {
        if e.Timestamp >= cutoff {
            trimmed = append(trimmed, e)
        }
    }
    ledger.Events = trimmed
    ledger.UpdatedAt = time.Now().UnixMilli()

    // 4. Write back
    return writeLedger(ctx, nk, crewID, ledger)
}
```

### 3.3 Concurrency

Nakama Storage uses optimistic concurrency via version tokens. If two writes race, one fails and retries. At expected event volumes (tens of events per day per crew), collisions are rare. Retry up to 3 times with jitter.

```go
func writeLedgerWithRetry(ctx context.Context, nk runtime.NakamaModule, crewID string, ledger CrewEventLedger, version string) error {
    for attempt := 0; attempt < 3; attempt++ {
        err := nk.StorageWrite(ctx, []*runtime.StorageWrite{{
            Collection:      "crew_events",
            Key:             crewID,
            UserID:          "",
            Value:           marshal(ledger),
            Version:         version, // Empty string = unconditional on first write
            PermissionRead:  2,
            PermissionWrite: 0,
        }})
        if err == nil {
            return nil
        }
        // Version conflict: re-read, re-append, retry
        time.Sleep(time.Duration(50*(attempt+1)) * time.Millisecond)
        freshLedger := readLedger(ctx, nk, crewID)
        freshLedger.Events = append(freshLedger.Events, ledger.Events[len(ledger.Events)-1])
        ledger = freshLedger
    }
    return fmt.Errorf("crew_events write failed after 3 retries for crew %s", crewID)
}
```

### 3.4 Document Size Budget

Worst case estimate for a very active crew over 7 days:
- 50 voice sessions * ~200 bytes = 10 KB
- 20 stream sessions * ~250 bytes = 5 KB
- 100 game sessions * ~200 bytes = 20 KB
- 50 moments * ~150 bytes = 7.5 KB
- 30 chat activity windows * ~100 bytes = 3 KB
- 10 member changes * ~100 bytes = 1 KB

**Total: ~46.5 KB** -- well within Nakama's 256 KB storage object limit.

---

## 4. Catch-Up Generation (v1, Templates)

### 4.1 Catch-Up RPC

```go
initializer.RegisterRpc("crew_catchup", CrewCatchupRPC)
```

**Request:**

```json
{
    "crew_id": "crew_xyz",
    "last_seen": 1711300000000
}
```

**Response:**

```json
{
    "crew_id": "crew_xyz",
    "catchup_text": "ash hit Immortal in Valorant, new scrim times posted, and koji streamed his first ace yesterday.",
    "event_count": 14,
    "top_events": [
        { "type": "moment", "actor_id": "user_a", "ts": 1711350000000, "data": { "text": "hit Immortal", "sentiment": "highlight", "game_name": "Valorant" } },
        { "type": "stream_session", "actor_id": "user_c", "ts": 1711340000000, "data": { "streamer_name": "koji", "title": "first ace", "duration_min": 30 } }
    ],
    "has_events": true
}
```

When there are no events or `last_seen` is recent (< 4 hours), return the quiet state:

```json
{
    "crew_id": "crew_xyz",
    "catchup_text": "All quiet, crew's been chilling. Nothing new since you left.",
    "event_count": 0,
    "top_events": [],
    "has_events": false
}
```

### 4.2 Ranking and Selection

The catch-up card shows a summary built from the top 3 events by score, deduplicated by type.

```go
func buildCatchup(events []CrewEvent, lastSeen int64) CatchupResponse {
    // 1. Filter to events after lastSeen
    recent := filterAfter(events, lastSeen)
    if len(recent) == 0 {
        return quietCatchup()
    }

    // 2. Sort by score descending, then timestamp descending
    sort.Slice(recent, func(i, j int) bool {
        if recent[i].Score != recent[j].Score {
            return recent[i].Score > recent[j].Score
        }
        return recent[i].Timestamp > recent[j].Timestamp
    })

    // 3. Pick top 3, preferring type diversity
    selected := selectDiverse(recent, 3)

    // 4. Render text from templates
    text := renderCatchupText(selected)

    return CatchupResponse{
        CatchupText: text,
        EventCount:  len(recent),
        TopEvents:   selected,
        HasEvents:   true,
    }
}
```

The `selectDiverse` function picks the highest-scored event, then skips events of the same type for subsequent picks unless no other types remain.

### 4.3 Template Rendering

Each event type has a template function that produces a natural-language fragment:

```go
var templateFuncs = map[string]func(CrewEvent) string{
    "moment": func(e CrewEvent) string {
        d := e.Data.(MomentData)
        name := lookupUsername(e.ActorID)
        if d.Text != "" {
            return fmt.Sprintf("%s: %s", name, d.Text)
        }
        switch d.Sentiment {
        case "win":
            return fmt.Sprintf("%s had a win in %s", name, d.GameName)
        case "loss":
            return fmt.Sprintf("%s took an L in %s", name, d.GameName)
        case "highlight":
            return fmt.Sprintf("%s had a moment in %s", name, d.GameName)
        }
        return ""
    },
    "voice_session": func(e CrewEvent) string {
        d := e.Data.(VoiceSessionData)
        names := joinNames(d.ParticipantNames, 3)
        return fmt.Sprintf("%s hung out in %s for %dm", names, d.ChannelName, d.DurationMin)
    },
    "stream_session": func(e CrewEvent) string {
        d := e.Data.(StreamSessionData)
        return fmt.Sprintf("%s streamed %s for %dm", d.StreamerName, d.Title, d.DurationMin)
    },
    "game_session": func(e CrewEvent) string {
        d := e.Data.(GameSessionData)
        names := joinNames(d.PlayerNames, 3)
        return fmt.Sprintf("%s played %s", names, d.GameName)
    },
    "member_joined": func(e CrewEvent) string {
        d := e.Data.(MemberJoinedData)
        return fmt.Sprintf("%s joined the crew", d.DisplayName)
    },
    "member_left": func(e CrewEvent) string {
        d := e.Data.(MemberLeftData)
        return fmt.Sprintf("%s left the crew", d.DisplayName)
    },
    "chat_activity": func(e CrewEvent) string {
        d := e.Data.(ChatActivityData)
        return fmt.Sprintf("%d messages from %d people in chat", d.MessageCount, d.ActiveUserCount)
    },
}
```

The final catch-up text joins selected fragments with commas and "and":

```go
func renderCatchupText(events []CrewEvent) string {
    fragments := make([]string, 0, len(events))
    for _, e := range events {
        if fn, ok := templateFuncs[e.Type]; ok {
            if f := fn(e); f != "" {
                fragments = append(fragments, f)
            }
        }
    }
    return joinWithAnd(fragments)
    // "ash: hit Immortal, koji streamed PROJECT AVALON for 120m, and nav joined the crew"
}

func joinWithAnd(parts []string) string {
    switch len(parts) {
    case 0:
        return ""
    case 1:
        return parts[0]
    case 2:
        return parts[0] + " and " + parts[1]
    default:
        return strings.Join(parts[:len(parts)-1], ", ") + ", and " + parts[len(parts)-1]
    }
}
```

---

## 5. Post-Game Moment RPC

Handles the post-game flow (win/loss/highlight taps from the client bottom bar).

```go
initializer.RegisterRpc("post_moment", PostMomentRPC)
```

**Request:**

```json
{
    "crew_id": "crew_xyz",
    "sentiment": "highlight",
    "text": "40-bomb on Dust2",
    "game_name": "Counter-Strike 2"
}
```

`text` is optional (empty string if user only tapped a sentiment button). `game_name` is populated by the client from the most recent detected game.

**Validation:**
- `sentiment` must be one of: `win`, `loss`, `highlight`
- `text` max length: 140 characters
- `game_name` max length: 100 characters
- User must be a member of the crew
- Rate limit: max 10 moments per user per crew per day

**Response:**

```json
{
    "success": true,
    "event_id": "01J5K3M..."
}
```

---

## 6. Hook Integration

### 6.1 Events Written by Existing Hooks

| Hook / Module | Event Written | Trigger |
|---------------|---------------|---------|
| `voice_state.go` voice_leave | `voice_session` | Last member leaves a channel that had 2+ members (track via in-memory session start time) |
| `streaming.go` stream_stop | `stream_session` | Stream ends |
| `crews.go` AfterJoinGroup | `member_joined` | User joins crew |
| `crews.go` AfterLeaveGroup | `member_left` | User leaves crew |
| `crew_state.go` OnChatMessage | `chat_activity` | Aggregated, not per-message (see 6.2) |

### 6.2 Chat Activity Aggregator

Chat activity is not logged per-message. Instead, an in-memory counter per crew tracks messages. Every 30 minutes, if the count exceeds a threshold (10 messages), a `chat_activity` event is written and the counter resets.

```go
var chatCounters = struct {
    sync.Mutex
    counts map[string]*chatWindow // crew_id -> window
}{}

type chatWindow struct {
    count      int
    userSet    map[string]bool
    windowStart int64
}

// Called from OnChatMessage hook
func incrementChatCounter(crewID, userID string) {
    chatCounters.Lock()
    defer chatCounters.Unlock()
    w, ok := chatCounters.counts[crewID]
    if !ok {
        w = &chatWindow{
            userSet:     make(map[string]bool),
            windowStart: time.Now().UnixMilli(),
        }
        chatCounters.counts[crewID] = w
    }
    w.count++
    w.userSet[userID] = true
}

// Called by a goroutine ticker every 30 minutes
func flushChatCounters(ctx context.Context, nk runtime.NakamaModule) {
    chatCounters.Lock()
    snapshot := chatCounters.counts
    chatCounters.counts = make(map[string]*chatWindow)
    chatCounters.Unlock()

    for crewID, w := range snapshot {
        if w.count < 10 {
            continue // Below threshold, discard
        }
        event := CrewEvent{
            ID:        generateULID(),
            CrewID:    crewID,
            Type:      "chat_activity",
            ActorID:   "",
            Timestamp: time.Now().UnixMilli(),
            Score:     5,
            Data: ChatActivityData{
                MessageCount:    w.count,
                WindowStart:     w.windowStart,
                WindowEnd:       time.Now().UnixMilli(),
                ActiveUserCount: len(w.userSet),
            },
        }
        AppendCrewEvent(ctx, nk, crewID, event)
    }
}
```

### 6.3 Game Session Events

Game sessions require the client to report session data. The client detects game start/stop via process scanning and pushes a `game_session_end` RPC when a game closes.

```go
initializer.RegisterRpc("game_session_end", GameSessionEndRPC)
```

**Request (from client):**

```json
{
    "crew_id": "crew_xyz",
    "game_name": "Counter-Strike 2",
    "duration_min": 45
}
```

The server enriches with co-players: any other crew members who also had an active game session for the same game overlapping in time (tracked via presence activity or a lightweight in-memory map).

---

## 7. Last-Seen Tracking

The catch-up system needs to know when a user last interacted with a crew. This is tracked per user per crew.

### 7.1 Storage

| Field | Value |
|-------|-------|
| Collection | `crew_last_seen` |
| Key | `{crew_id}` |
| UserID | `{user_id}` (user-owned) |
| PermissionRead | `1` (owner only) |
| PermissionWrite | `1` (owner only) |

```json
{
    "crew_id": "crew_xyz",
    "last_seen": 1711400000000
}
```

### 7.2 Update Triggers

`last_seen` is updated when:

- User sets this crew as active (`set_active_crew` RPC, spec 11)
- User sends a chat message in this crew
- User joins a voice channel in this crew

It is NOT updated for passive sidebar visibility. The point is to capture "the last time the user was actively engaged with this crew."

---

## 8. Backend Module

### 8.1 New File

```
backend/nakama/data/modules/
├── ...existing files...
└── crew_events.go       # Event ledger, catch-up RPC, moment RPC, chat aggregator
```

### 8.2 Registration

Add to `main.go` `InitModule`:

```go
// RPCs -- crew events
initializer.RegisterRpc("crew_catchup", CrewCatchupRPC)
initializer.RegisterRpc("post_moment", PostMomentRPC)
initializer.RegisterRpc("game_session_end", GameSessionEndRPC)

// Start chat activity flush ticker
go startChatActivityTicker(ctx, nk, 30 * time.Minute)
```

### 8.3 ULID Generation

Events use ULIDs for IDs (time-sortable, unique). Use `github.com/oklog/ulid/v2`.

```go
import "github.com/oklog/ulid/v2"

func generateULID() string {
    return ulid.Make().String()
}
```

---

## 9. Client Integration

### 9.1 Catch-Up Flow

```
User opens app
    |
    v
For each crew in sidebar:
    1. Read local last_seen cache
    2. If now - last_seen > 4 hours:
        Call crew_catchup RPC
    3. If response.has_events:
        Show catch-up card on sidebar item
    4. If !response.has_events:
        Show quiet state ("All quiet...")
    |
    v
User taps into crew (set_active_crew):
    1. Catch-up card stays visible until user sends a message or joins voice
    2. Update last_seen (server + local cache)
    3. Dismiss catch-up card
```

### 9.2 Post-Game Flow

```
Client detects game process exit
    |
    v
If user is in a crew (has active/recent crew):
    1. Morph bottom bar to post-game card
    2. Show sentiment buttons (win / loss / highlight)
    3. Wait for tap or 30s timeout
    |
    v
If user tapped a button:
    1. If highlight: show text input, wait for submit or 15s timeout
    2. Call post_moment RPC with sentiment + optional text + game_name
    3. Show confirmation ("Moment shared with crew")
    4. Fade to idle after 2s
    |
    v
If timeout (no tap):
    1. Fade post-game card to idle
    2. Call game_session_end RPC (session still logged, just no moment)
```

### 9.3 Rust Types

In `mello-core/src/crew_events.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatchupResponse {
    pub crew_id: String,
    pub catchup_text: String,
    pub event_count: u32,
    pub top_events: Vec<CatchupEvent>,
    pub has_events: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatchupEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub actor_id: String,
    pub ts: i64,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostMomentRequest {
    pub crew_id: String,
    pub sentiment: String, // "win" | "loss" | "highlight"
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub game_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameSessionEndRequest {
    pub crew_id: String,
    pub game_name: String,
    pub duration_min: u32,
}
```

---

## 10. Future Extensions (Not In Scope)

These are documented so future specs can build on the event ledger without redesigning it:

- **AI-powered catch-up (v2):** Feed `top_events` to a lightweight LLM for natural language polish. Premium/hosted feature only.
- **Weekly crew digest:** Aggregate a week's events into a summary, delivered as a notification or email.
- **Activity feed:** Render the full event list as a scrollable feed within the crew view.
- **Year in review:** Annual summary of crew activity (most played games, total voice hours, top moments).
- **Crew analytics (premium):** Charts showing activity trends, peak hours, member engagement.

---

*This spec covers the crew event ledger, catch-up card data, and post-game moment flow. For presence and crew state, see [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md). For voice channels, see [13-VOICE-CHANNELS.md](./13-VOICE-CHANNELS.md).*
