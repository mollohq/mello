# Stream Snapshots — Client & Backend Specification

> **Component:** Nakama backend (Go), mello-core (Rust), Client UI (Slint)
> **Version:** 1.2
> **Status:** Planned
> **Related docs:** [STREAM-SNAPSHOTS-SFU.md](./STREAM-SNAPSHOTS-SFU.md), [CLIPS.md](./CLIPS.md), [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md)

---

## 1. Purpose

Stream session cards in the crew feed display a crossfading sequence of still frames captured during the session, making the feed feel alive and giving crew members an immediate visual sense of what was played. This spec covers the backend changes to `streaming.go` and `crew_events.go`, the mello-core data model, and the `SessionPreviewCard` Slint component.

---

## 2. Backend Changes (Nakama)

### 2.1 Data Flow

```
StopStreamRPC (streaming.go)
    │
    ├── List R2 prefix: snapshots/{crew_id}/{session_id}/
    │       → sorted []string of public CDN URLs, oldest first
    │
    ├── Build StreamSessionData { ..., SnapshotURLs: urls }
    │
    └── AppendCrewEvent → crew event ledger (crew_events collection)
                              │
                              └── CrewTimelineRPC reads event ledger
                                      → TimelineEntry { data: StreamSessionData }
                                      → client receives snapshot_urls in feed card
```

The SFU writes JPEGs to R2 (`mello-snapshots` bucket) under a deterministic key structure (`snapshots/{crew_id}/{session_id}/{unix_timestamp_ms}.jpg`). `StopStreamRPC` lists that prefix to discover what was actually uploaded, sorts by timestamp, and writes the result into `StreamSessionData`. No runtime coordination between SFU and Nakama.

### 2.2 `crew_events.go` — StreamSessionData

Add `SnapshotURLs` to the existing `StreamSessionData` struct. Field names must match the existing event ledger schema (`streamer_id`, `streamer_name`, `duration_min` — see `16-CREW-EVENT-LEDGER.md` section 2.1):

```go
type StreamSessionData struct {
    // existing fields — do not rename
    StreamerID   string   `json:"streamer_id"`
    StreamerName string   `json:"streamer_name"`
    Title        string   `json:"title"`
    Game         string   `json:"game"`
    DurationMin  int      `json:"duration_min"`
    PeakViewers  int      `json:"peak_viewers"`
    ViewerIDs    []string `json:"viewer_ids"`

    // new field
    SnapshotURLs []string `json:"snapshot_urls"` // empty slice if none captured
}
```

### 2.3 `streaming.go` — StopStreamRPC

In `StopStreamRPC`, before calling `AppendCrewEvent` (currently line 310), list the R2 prefix and populate `SnapshotURLs`:

```go
snapshotURLs, err := listSnapshotURLs(ctx, snapshotsS3Client, crewID, sessionID)
if err != nil {
    logger.Warn("StopStreamRPC: snapshot R2 list failed, continuing without snapshots",
        zap.Error(err))
    snapshotURLs = []string{}
}

event := CrewEvent{
    ID:        generateULID(),
    CrewID:    crewID,
    Type:      "stream_session",
    ActorID:   hostUserID,
    Timestamp: time.Now().UnixMilli(),
    Score:     30,
    Data: StreamSessionData{
        // existing fields...
        SnapshotURLs: snapshotURLs,
    },
}
AppendCrewEvent(ctx, nk, crewID, event)
```

`listSnapshotURLs` lists the R2 prefix `snapshots/{crew_id}/{session_id}/`, extracts the timestamp from each key filename, sorts ascending by timestamp, and constructs public CDN URLs:

```go
func listSnapshotURLs(ctx context.Context, s3c *s3.Client, crewID, sessionID string) ([]string, error) {
    ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
    defer cancel()

    prefix := fmt.Sprintf("snapshots/%s/%s/", crewID, sessionID)
    // s3 ListObjectsV2 with prefix against mello-snapshots bucket
    // extract unix_timestamp_ms from key filename (e.g. "1714000010000.jpg" → 1714000010000)
    // sort ascending by timestamp
    // construct public URL: https://{SNAPSHOTS_S3_PUBLIC_URL}/snapshots/{crew_id}/{session_id}/{ts}.jpg
    // return sorted URL slice, max 6 entries
}
```

On error or timeout: return empty slice, log WARN. Do not fail the RPC.

### 2.4 S3 Client for Snapshots Bucket

Nakama already initializes an S3 client for the clips bucket (`mello-clips`) using existing env vars. Snapshots live in a separate bucket (`mello-snapshots`) on the same R2 account. Initialize a second S3 client at startup pointing at the snapshots bucket using two new env vars, reusing existing credentials:

| Env var | Description |
|---|---|
| `SNAPSHOTS_S3_BUCKET` | Snapshot bucket name (default: `mello-snapshots`) |
| `SNAPSHOTS_S3_PUBLIC_URL` | Public base URL for snapshot CDN (e.g. `https://snapshots.m3llo.app`) |

Reuse `S3_ENDPOINT`, `S3_ACCESS_KEY`, `S3_SECRET_KEY` from the existing clips S3 client. Same R2 account, different bucket. Only `ListObjectsV2` permission is needed on this bucket from Nakama's side; write access is only needed by the SFU.

### 2.5 CrewTimelineRPC

No changes needed to the RPC itself. `CrewTimelineRPC` reads `StreamSessionData` from the crew event ledger and passes it through to the client. `snapshot_urls` appears in the payload automatically once `StreamSessionData` includes it.

`snapshot_urls` will be an empty slice (`[]`) for:
- Sessions that ended before this feature shipped.
- Sessions shorter than 10 seconds (no IDR captured in time).
- Sessions where the R2 list failed.

Clients handle the empty case via the fallback rendering (section 5.3).

---

## 3. mello-core Changes

### 3.1 Types

File: `mello-core/src/crew_state.rs` (or wherever feed card types live).

Add `snapshot_urls` to the stream session card type. Field names must match the event ledger JSON (`streamer_id`, `streamer_name`, `duration_min`):

```rust
pub struct StreamSessionCard {
    pub streamer_id: String,
    pub streamer_name: String,
    pub title: String,
    pub game: Option<String>,
    pub duration_min: u32,
    pub peak_viewers: u32,
    pub viewer_ids: Vec<String>,
    pub ended_at: i64,               // Unix ms (from CrewEvent.Timestamp)
    pub snapshot_urls: Vec<String>,  // empty if none captured
}
```

### 3.2 crew_timeline Parsing

File: `mello-core/src/client/presence.rs` (or wherever `crew_timeline` RPC is called).

When deserializing a `stream_session` entry from the RPC response, populate `snapshot_urls` from the field. If the field is absent (older sessions, pre-feature), default to an empty `Vec`.

### 3.3 Event

The existing event that carries feed cards to the UI already carries `StreamSessionCard` as a variant. No new event type needed. `snapshot_urls` flows through automatically.

---

## 4. Slint: SessionPreviewCard Component

File: `client/ui/panels/session_preview_card.slint`

### 4.1 Inputs

```slint
component SessionPreviewCard {
    in property <[string]> snapshot-urls;    // ordered CDN URLs, may be empty
    in property <string> game-name;
    in property <string> streamer-username;
    in property <int> duration-min;
    in property <int> peak-viewers;
    in property <int> clip-count;            // 0 = hide clip badge
    in property <duration> time-since-ended; // for "3h ago" label
    in property <bool> is-hero;              // true = 2×2 bento cell, false = 1×1
}
```

### 4.2 Layout

```
┌─────────────────────────────────────────┐
│                                         │
│   [crossfading snapshot image]          │  60% of card height
│                                         │
│   ▶  ostkatt · CS2          ✂ 2 clips  │  overlay, bottom of image
│      1h 12m                             │
├─────────────────────────────────────────┤
│  [av][av][av] +2 watched    3h ago      │  16px padding, text-secondary
└─────────────────────────────────────────┘
```

Image area uses `clip: true`. Overlay text: `Theme.text-primary` 12px Barlow on a 32px gradient scrim (`rgba(0,0,0,0) → rgba(0,0,0,0.72)`) at image bottom edge.

Clip badge: amber (#F59E0B) pill, scissors icon + count, top-right of image area. Hidden when `clip-count == 0`.

Duration displayed as `Xh Ym` (e.g. `1h 12m`) derived from `duration-min`.

### 4.3 Crossfade Implementation

Two `Image` elements stacked absolutely inside the image area (`front` and `back`).

```slint
property <int> frame-index: 0;
property <float> front-opacity: 1.0;
property <bool> fading: false;

Timer {
    interval: 2500ms;
    running: snapshot-urls.length > 1 && self.visible;
    triggered => { fading = true; }
}

animate front-opacity {
    duration: 600ms;
    easing: ease-in-out;
}

states [
    fading when fading: { front-opacity: 0.0; }
    idle when !fading:  { front-opacity: 1.0; }
]
```

On `front-opacity` animation completion (use a second timer offset by 600ms from the fade trigger):

1. Load `snapshot_urls[(frame_index + 2) % len]` into `back` Image source (preload next-next frame).
2. Swap front/back source URLs and reset front opacity to 1.0 instantly (no animation on this step).
3. Increment `frame-index`.
4. Set `fading = false`.

At any moment only two decoded images are in memory.

### 4.4 Timer Pause Rules

Timer `running` is false when:
- `snapshot-urls.length <= 1`
- Card is not visible (Slint `visible` property tied to scroll position)

No reset on re-show: resume from current `frame-index`.

### 4.5 Image Loading

Slint's `@image-url(...)` does not support runtime string URLs. Use the Slint Rust API to set image sources at runtime. Follow the existing avatar loading pattern in `client/src/avatar.rs` for fetching HTTPS images and delivering them as `slint::Image`.

Load lazily: fetch `snapshot_urls[0]` and `snapshot_urls[1]` on card first-visible. Fetch subsequent frames one step ahead of the current `frame-index`.

Cache by URL using the existing in-memory image cache (if one exists). At most 6 images per session card, ~80–120KB each on wire.

### 4.6 Fallback: No Snapshots

When `snapshot-urls` is empty:

- Show a static placeholder: `Theme.surface` background with a centered game icon (if game known) or generic controller icon at 40% opacity.
- No timer, no animation.
- All metadata (username, duration, viewers, clips) renders identically.

Expected for: voice sessions, stream sessions under 10 seconds, pre-feature sessions.

---

## 5. Voice Session Card

Voice sessions use the same `SessionPreviewCard` component with `snapshot-urls: []`. Fallback rendering (section 4.6) always applies. Show a microphone icon as placeholder, or a game icon if game sensing detected one.

Layout differences from stream session:
- No ▶ play icon in overlay.
- Participant avatars shown in image area instead of scrim overlay.
- Username line shows participant names: "ostkatt, FaiL, b0bben" (from `participant_names` in `VoiceSessionData`).

`SessionPreviewCard` handles both by checking `snapshot-urls.length`.

---

## 6. Memory and Performance

| Item | Budget |
|---|---|
| Decoded images in memory per card | 2 (front + back) |
| JPEG size per frame on wire | ~80–120KB |
| Decoded RGBA at 1280×720 | ~3.5MB per image |
| Total per visible card | ~7MB GPU texture |
| Cards visible at once (bento grid) | 3–4 |
| Worst case GPU texture budget | ~28MB |

Off-screen cards: timer stops, no new fetches. Images stay resident until card is removed from model.

---

## 7. Code Pointers

| Concern | File |
|---|---|
| StreamSessionData struct | `backend/nakama/data/modules/crew_events.go` |
| StopStreamRPC + AppendCrewEvent | `backend/nakama/data/modules/streaming.go` line 310 |
| R2 list helper (`listSnapshotURLs`) | `backend/nakama/data/modules/streaming.go` (new function) |
| Snapshot S3 client init | `backend/nakama/data/modules/main.go` |
| Existing clips S3 client (reuse credentials) | `backend/nakama/data/modules/main.go` |
| CrewTimelineRPC (no changes) | `backend/nakama/data/modules/crews.go` |
| Feed card types | `mello-core/src/crew_state.rs` |
| crew_timeline parsing | `mello-core/src/client/presence.rs` |
| Image loading pattern | `client/src/avatar.rs` |
| SessionPreviewCard component | `client/ui/panels/session_preview_card.slint` |
| Bento grid layout | `client/ui/panels/crew_feed.slint` |
