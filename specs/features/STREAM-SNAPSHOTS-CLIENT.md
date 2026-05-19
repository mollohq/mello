# Stream Snapshots — Client & Backend

> **Component:** Nakama backend (Go), mello-core (Rust), Client UI (Slint)  
> **Version:** 1.3  
> **Status:** Implemented  
> **Depends on:** [16-CREW-EVENT-LEDGER.md](../16-CREW-EVENT-LEDGER.md), [CLIPS.md](./CLIPS.md), [00-ARCHITECTURE.md](../00-ARCHITECTURE.md)  
> **Related:** SFU snapshot capture (`mello-sfu/STREAM-SNAPSHOTS-SFU.md` if present)

---

## 1. Purpose

Stream session cards in the crew feed show still frames captured during a session. Cards with `snapshot_urls` render as **session-preview** tiles with a poster thumbnail and optional manual slideshow. This spec covers Nakama ledger fields, client loading (disk-first), `SessionPreviewCard`, and bento feed curation.

---

## 2. Backend (Nakama)

### 2.1 Data flow

```
StopStreamRPC (streaming.go)
    │
    ├── List R2 prefix: snapshots/{crew_id}/{session_id}/
    │       → sorted []string of public CDN URLs, oldest first
    │
    ├── Build StreamSessionData { ..., SnapshotURLs: urls }
    │
    └── AppendCrewEvent → crew event ledger
                              │
                              └── crew_timeline RPC
                                      → entries[].data.snapshot_urls
```

The SFU writes JPEGs to R2 (`mello-snapshots`). `StopStreamRPC` lists the prefix, sorts by timestamp, and stores URLs on `StreamSessionData`. No runtime coordination between SFU and Nakama.

### 2.2 `StreamSessionData`

```go
type StreamSessionData struct {
    StreamerID   string   `json:"streamer_id"`
    StreamerName string   `json:"streamer_name"`
    Title        string   `json:"title"`
    Game         string   `json:"game"`
    DurationMin  int      `json:"duration_min"`
    PeakViewers  int      `json:"peak_viewers"`
    ViewerIDs    []string `json:"viewer_ids"`
    SnapshotURLs []string `json:"snapshot_urls"` // empty if none captured
}
```

### 2.3 `listSnapshotURLs`

- Prefix: `snapshots/{crew_id}/{session_id}/`
- Sort ascending by timestamp in filename
- Public URL: `https://{SNAPSHOTS_S3_PUBLIC_URL}/snapshots/...`
- Max 6 entries; on error return `[]` and log WARN (do not fail RPC)

### 2.4 Snapshots S3 env

| Env var | Description |
|---|---|
| `SNAPSHOTS_S3_BUCKET` | Default `mello-snapshots` |
| `SNAPSHOTS_S3_PUBLIC_URL` | CDN base (e.g. `https://snapshots.m3llo.app`) |

Reuse `S3_ENDPOINT`, `S3_ACCESS_KEY`, `S3_SECRET_KEY` from clips. Nakama needs `ListObjectsV2` only; SFU writes.

### 2.5 `crew_timeline` RPC

Registered as `crew_timeline` in `backend/nakama/data/modules/clips.go`. Returns paginated ledger entries (newest first, default limit 20). No snapshot-specific shaping — client maps `stream_session` + `snapshot_urls` to feed cards.

Empty `snapshot_urls` when: pre-feature sessions, very short streams, or R2 list failure.

---

## 3. Client — timeline → feed cards

### 3.1 Card type mapping

In `client/src/handlers/clip.rs`:

| Ledger `type` | Condition | Feed `card-type` |
|---|---|---|
| `stream_session` | `snapshot_urls` non-empty | `session-preview` |
| `stream_session` | empty URLs | `session` |
| `voice_session` / `game_session` | — | `session` |
| `clip` | — | `clip` |

Only `session-preview` uses `SessionPreviewCard` and snapshot loading.

### 3.2 `FeedCardData` snapshot fields

`client/ui/types.slint`:

- `snapshot-loading`, `snapshot-poster`, `snapshot-poster-ready`, `snapshot-error`
- `snapshot-playback-frame`, `snapshot-playback-index`, `snapshot-playback-revision` (manual play only)

### 3.3 Events

`crew_timeline` → `Event::TimelineLoaded` → `handlers/clip.rs` builds cards, runs `feed_layout::order_feed_cards`, sets `feed_cards`, triggers `SnapshotLoader` for posters.

---

## 4. `SessionPreviewCard`

File: `client/ui/panels/session_preview_card.slint`

### 4.1 Poster + manual play

- **At rest:** frame 0 on `snapshot-poster`, play overlay, loading skeleton while poster fetches.
- **No auto-play** on first visibility.
- **Tap play:** crossfade slideshow; frames 1..N loaded on demand via `session-preview-request-frame`.
- **`session-seen`:** only after user completes a manual full cycle (≥70% of frames).

### 4.2 Image loading (disk-first)

1. `snapshot_cache.rs` — JPEGs in temp `mello_snapshots/` (50MB LRU); thumb decode ≤480px wide; optional `{hash}_thumb.jpg` on disk.
2. `snapshot_loader.rs` — async fetch + decode off UI thread; `invoke_from_event_loop` updates feed row.
3. No in-memory decoded URL cache (per `00-ARCHITECTURE.md`).

On `TimelineLoaded`: poster jobs for each `session-preview` in the ordered bento only. No bulk prefetch of every historical URL.

### 4.3 Fallback

Empty `snapshot-urls` or `snapshot-error`: placeholder UI (gradient + monitor icon). Stream sessions without snapshots use **`SessionCard`**, not this component.

---

## 5. Crew feed bento curation

File: `client/src/feed_layout.rs` (client-side; see note below).

### 5.1 Goals

- **Hero:** best `session-preview` by quality score (visual priority over clips).
- **Deprioritize** short streams (≤2 min, ≤4 snapshots) for hero/wide slots.
- **Mix:** at least one of each present type in the nine grid slots: `clip`, `session`, `session-preview`, `catchup`, plus pinned `recap`.
- **Priority fill:** clips → strong previews → generic sessions → catchups.
- **Wide slot:** strongest visual among fillers (preview or clip).

### 5.2 Quality score (session-preview)

```text
if duration_min <= 2 && snapshot_count <= 4 → heavily penalized
else duration_min * 10 + snapshot_count * 3 + bonuses for duration ≥15 / snapshots ≥8
```

### 5.3 Pagination placeholder

When `crew_timeline` returns `has_more: true`, feed shows a **Show more** control (no-op until premium pagination ships). `feed-has-more` on `MainWindow` / `CrewFeed`.

### 5.4 Future: backend curation

Ranking/mix rules may move to `crew_timeline` when premium scroll ships so all clients and pages stay consistent. Bento cell geometry can remain client-side.

---

## 6. Memory and performance

Aligned with `00-ARCHITECTURE.md`. **Disk is the cache; RAM is the exception.**

| Item | Budget |
|---|---|
| Disk cache | ≤50MB LRU (`mello_snapshots/`) |
| Decoded RAM per card at rest | 1 thumb poster (~200–400KB RGBA) |
| Decoded RAM during manual play | ≤2 thumbs (crossfade) |
| JPEG on wire | ~80–120KB typical |
| Decode size | ≤480px wide — not full 720p |
| In-memory decoded cache | None |

---

## 7. Code pointers

| Concern | File |
|---|---|
| `StreamSessionData` | `backend/nakama/data/modules/crew_events.go` |
| `StopStreamRPC`, `listSnapshotURLs` | `backend/nakama/data/modules/streaming.go` |
| Snapshot S3 client | `backend/nakama/data/modules/main.go` |
| `crew_timeline` RPC | `backend/nakama/data/modules/clips.go` |
| Timeline → feed cards | `client/src/handlers/clip.rs` |
| Bento ordering | `client/src/feed_layout.rs` |
| Snapshot disk cache | `client/src/snapshot_cache.rs` |
| Snapshot async loader | `client/src/snapshot_loader.rs` |
| `FeedCardData` | `client/ui/types.slint` |
| `SessionPreviewCard` | `client/ui/panels/session_preview_card.slint` |
| Feed UI | `client/ui/panels/crew_feed.slint` |
| mello-core timeline | `mello-core/src/client/clip.rs` |

---

## 8. Testing

- RPC: `crew_timeline` with `{ "crew_id": "<uuid>", "limit": 20 }` — inspect `stream_session` entries and `data.snapshot_urls`.
- Client unit tests: `feed_layout` (hero, diversity, long preview vs noise sessions).
- Client unit tests: `snapshot_cache` JPEG decode.
