# Crew Memory Durability: Backend Delta (Delta 1 of 3)

> **Component:** Crew Event Ledger, Clips, Crew Feed (backend only)
> **Version:** 1.0
> **Status:** Ready for implementation
> **Parent:** [00-ARCHITECTURE.md](../00-ARCHITECTURE.md)
> **Related:** [16-CREW-EVENT-LEDGER.md](../16-CREW-EVENT-LEDGER.md), [CLIPS.md](../CLIPS.md), [11-PRESENCE-CREW-STATE.md](../11-PRESENCE-CREW-STATE.md), [09-MONETIZATION.md](../09-MONETIZATION.md)
> **Files touched:** `backend/nakama/data/modules/crew_events.go`, `backend/nakama/data/modules/clips.go`, `backend/nakama/data/modules/main.go`, new file `backend/nakama/data/modules/crew_recaps.go`, new file `backend/nakama/data/modules/entitlements.go`
>
> **File ownership:** one durable concern per file. Clip storage and clip RPCs stay in `clips.go`. All recap code (structs, store helpers, `AppendRecap`, the generation job, the recaps RPC) moves to a new `crew_recaps.go`. Future durable types (for example stream recordings) get their own file too. Do not consolidate these into `clips.go`.

This is a delta. It describes changes against the current implementation, not a full rewrite. Apply the changes in the order given. Do not open a PR autonomously. Build, run the done-criteria checks, report back.

---

## 0. Why

Today every crew event, including clips and weekly recaps, is written into a single per-crew ledger document and trimmed at 7 days inside `AppendCrewEvent`. `crew_timeline` returns that ledger sorted purely by timestamp. The result: a crew that was active for four or five days, then quiet for a week, shows an empty feed even though it produced clips and session content, because the durable artifacts were deleted with everything else and the feed renders time linearly.

This delta separates two concerns that are currently fused:

- The **ledger** stays a short, 7-day rolling store of ephemeral pulse events (voice, stream, game, joins, chat activity, moments). Trim unchanged.
- **Clips and recaps become durable** in their own per-crew stores, outside the trim, so the crew's memory persists.

Client rendering (the layered feed, the durable spine, the presence pulse row, any locked or upsell treatment) is out of scope here and is specced in Delta 2 (desktop) and Delta 3 (iOS).

---

## 1. Decisions locked for this delta

| Decision | Choice | Note |
|----------|--------|------|
| Recap storage | New `crew_recaps/{crew_id}` per-crew document | Tiny volume, single doc is correct |
| Clip storage | New `crew_clips/{crew_id}` per-crew document, capped at most-recent 500 | Avoids the 256KB Nakama doc limit; full history deferred to m3llo+ |
| Free-tier clip expiry | None for now | No gating built yet, so clips and media both persist for everyone |
| R2 media deletion | None for now | See section 11 (infra action item) |
| Entitlement gate | Single stub `IsUserPremium(userID) bool` returning false | m3llo+ is per-user; the gate keys on the viewing user, not the crew |
| Ledger trim | Unchanged for ephemeral types | Clips and recaps no longer enter the ledger at all |
| Migration | None | Greenfield, no production data |

The clip cap (500) is the one number to confirm. Everything else follows the prior design discussion.

---

## 2. New durable storage

Both new stores mirror the existing ledger pattern (system-owned, keyed by crew, public read, server-only write, optimistic-concurrency retry). They are separate documents so they are never touched by the ledger trim.

### 2.1 Recaps store (in `crew_recaps.go`)

```go
const CrewRecapsCollection = "crew_recaps"

type CrewRecapsDoc struct {
    CrewID    string            `json:"crew_id"`
    Recaps    []WeeklyRecapData `json:"recaps"` // newest appended last
    UpdatedAt int64             `json:"updated_at"`
}
```

### 2.2 Clips store (in `clips.go`)

```go
const CrewClipsCollection = "crew_clips"
const CrewClipsMaxRetained = 500 // cap to stay under Nakama's 256KB object limit

type StoredClip struct {
    EventID         string   `json:"event_id"`   // time-sortable, from generateEventID()
    ClipID          string   `json:"clip_id"`
    ActorID         string   `json:"actor_id"`
    Ts              int64    `json:"ts"`
    Score           int      `json:"score"`      // 50, matches current clip score
    ClipType        string   `json:"clip_type"`
    ClipperName     string   `json:"clipper_name"`
    DurationSeconds float64  `json:"duration_seconds"`
    Participants    []string `json:"participants,omitempty"`
    ParticipantNames []string `json:"participant_names,omitempty"`
    Game            string   `json:"game,omitempty"`
    LocalPath       string   `json:"local_path,omitempty"`
    MediaURL        string   `json:"media_url,omitempty"`
}

type CrewClipsDoc struct {
    CrewID    string       `json:"crew_id"`
    Clips     []StoredClip `json:"clips"` // newest appended last
    UpdatedAt int64        `json:"updated_at"`
}
```

### 2.3 Read/write helpers

Clip helpers (`readClipsDoc`, `writeClipsDoc`, `AppendClip`) live in `clips.go`. Recap helpers (`readRecapsDoc`, `writeRecapsDoc`, `AppendRecap`) live in `crew_recaps.go`. Both mirror `readLedger` / `writeLedger`. Use `SystemUserID`, `PermissionRead: 2`, `PermissionWrite: 0`, the same 3-attempt optimistic retry with jitter used in `AppendCrewEvent`.

```go
func readClipsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string) (*CrewClipsDoc, string) { /* StorageRead, unmarshal, return empty doc + "" on miss */ }
func writeClipsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string, doc *CrewClipsDoc, version string) error { /* StorageWrite */ }

func readRecapsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string) (*CrewRecapsDoc, string) { /* ... */ }
func writeRecapsDoc(ctx context.Context, nk runtime.NakamaModule, crewID string, doc *CrewRecapsDoc, version string) error { /* ... */ }
```

```go
// AppendClip appends a clip to the durable clips doc, trims to the most-recent cap, retries on version conflict.
func AppendClip(ctx context.Context, nk runtime.NakamaModule, crewID string, clip StoredClip) error {
    for attempt := 0; attempt < 3; attempt++ {
        doc, version := readClipsDoc(ctx, nk, crewID)
        doc.Clips = append(doc.Clips, clip)
        // Cap: keep the most recent CrewClipsMaxRetained by timestamp.
        if len(doc.Clips) > CrewClipsMaxRetained {
            sort.Slice(doc.Clips, func(i, j int) bool { return doc.Clips[i].Ts < doc.Clips[j].Ts })
            doc.Clips = doc.Clips[len(doc.Clips)-CrewClipsMaxRetained:]
        }
        doc.UpdatedAt = time.Now().UnixMilli()
        if err := writeClipsDoc(ctx, nk, crewID, doc, version); err == nil {
            return nil
        }
        jitter, _ := rand.Int(rand.Reader, big.NewInt(50))
        time.Sleep(time.Duration(50*(attempt+1)+int(jitter.Int64())) * time.Millisecond)
    }
    return fmt.Errorf("crew_clips write failed after 3 retries for crew %s", crewID)
}

// AppendRecap appends a recap to the durable recaps doc (no cap; recaps are tiny).
func AppendRecap(ctx context.Context, nk runtime.NakamaModule, crewID string, recap WeeklyRecapData) error { /* same retry shape, no trim */ }
```

Note for the implementer: `rand` and `big` are already imported in `crew_events.go`. If these helpers live in `clips.go`, add the imports there.

---

## 3. Ledger trim: leave it, but stop putting durable types in it

`AppendCrewEvent` in `crew_events.go` (the trim at the cutoff loop) stays exactly as is. It now only ever receives ephemeral types, because clips and recaps are routed to their own stores (sections 4 and 6). Do not special-case the trim by type. The cleaner the ledger stays as a pure 7-day pulse store, the better.

The `"clip"` and `"weekly_recap"` cases in `renderEventFragment` (catch-up text) stay defined, but their inputs now come from the durable stores via the catch-up merge in section 9, not from the ledger.

---

## 4. PostClipRPC: write to the durable clips store

In `clips.go`, `PostClipRPC` currently builds a `CrewEvent` of type `"clip"` and calls `AppendCrewEvent`. Change it to:

1. Replace the rate-limit count source. Currently it counts `"clip"` events in the ledger over the last 24h. Read from the durable clips doc instead:

```go
clipsDoc, _ := readClipsDoc(ctx, nk, req.CrewID)
dayStart := time.Now().Truncate(24 * time.Hour).UnixMilli()
clipCount := 0
for _, c := range clipsDoc.Clips {
    if c.ActorID == userID && c.Ts >= dayStart {
        clipCount++
    }
}
if clipCount >= ClipMaxPerUserPerDay {
    return "", runtime.NewError("clip rate limit exceeded", 8)
}
```

2. Replace the `AppendCrewEvent` write with `AppendClip`:

```go
eventID := generateEventID()
clip := StoredClip{
    EventID:          eventID,
    ClipID:           req.ClipID,
    ActorID:          userID,
    Ts:               time.Now().UnixMilli(),
    Score:            50,
    ClipType:         req.ClipType,
    ClipperName:      username,
    DurationSeconds:  req.DurationSeconds,
    Participants:     req.Participants,
    ParticipantNames: participantNames,
    Game:             req.Game,
    LocalPath:        req.LocalPath,
}
if err := AppendClip(ctx, nk, req.CrewID, clip); err != nil {
    logger.Error("Failed to append clip: %v", err)
    return "", runtime.NewError("failed to save clip", 13)
}
```

The response shape (`success`, `event_id`, `clip_id`) is unchanged. Keep `event_id` equal to the clip's `EventID` so clients can correlate.

The chat-card side effect (the auto-posted clip card in crew chat) is unaffected; it is driven separately and does not depend on the ledger.

---

## 5. ClipUploadCompleteRPC: update the durable clips record

Currently this RPC scans the ledger for the clip event and sets `MediaURL`. Point it at the clips doc:

```go
key := fmt.Sprintf("crews/%s/%s.mp4", req.CrewID, req.ClipID)
mediaURL := S3PublicURL(key)

for attempt := 0; attempt < 3; attempt++ {
    doc, version := readClipsDoc(ctx, nk, req.CrewID)
    found := false
    for i := range doc.Clips {
        if doc.Clips[i].ClipID == req.ClipID {
            doc.Clips[i].MediaURL = mediaURL
            found = true
            break
        }
    }
    if !found {
        return "", runtime.NewError("clip not found", 5)
    }
    doc.UpdatedAt = time.Now().UnixMilli()
    if err := writeClipsDoc(ctx, nk, req.CrewID, doc, version); err == nil {
        break
    }
    if attempt == 2 {
        return "", runtime.NewError("failed to update clip media url", 13)
    }
    // jitter sleep as elsewhere
}
```

Object key convention (`crews/{crew_id}/{clip_id}.mp4`) is unchanged.

---

## 6. Weekly recap: store durably, count clips from the clips store

In `crew_recaps.go`, `generateWeeklyRecap` (moved here from `clips.go` along with `RecapMember`, `WeeklyRecapData`, `topActor`, `topActors`, `StartWeeklyRecapJob`, `generateRecapsForAllCrews`):

1. The clip count currently comes from `case "clip": clipCount++` while scanning the ledger. Clips are no longer in the ledger. Replace that source by reading the clips doc and counting clips inside the recap window:

```go
clipsDoc, _ := readClipsDoc(ctx, nk, crewID)
clipCount := 0
for _, c := range clipsDoc.Clips {
    if c.Ts >= startMs {
        clipCount++
        actorClips[c.ActorID]++
    }
}
```

Remove the `case "clip":` branch from the ledger scan loop.

2. Replace the recap write. Currently it builds a `weekly_recap` `CrewEvent` and calls `AppendCrewEvent`. Call `AppendRecap` instead:

```go
if err := AppendRecap(ctx, nk, crewID, recap); err != nil {
    logger.Error("Failed to store weekly recap for crew %s: %v", crewID, err)
}
```

Remove the now-dead `WeeklyRecapCollection = "weekly_recaps"` constant (it was never wired). Use `CrewRecapsCollection` from section 2.1.

The `StartWeeklyRecapJob` schedule and `generateRecapsForAllCrews` loop are unchanged.

---

## 7. New RPCs: crew_clips and crew_recaps

`CrewClipsRPC` lives in `clips.go`. `CrewRecapsRPC` lives in `crew_recaps.go`. Both paginate over their per-crew array by index, newest first, same cursor convention as `crew_timeline`.

### 7.1 crew_clips

```go
type ClipsPageRequest struct {
    CrewID string `json:"crew_id"`
    Cursor string `json:"cursor,omitempty"` // last EventID of previous page
    Limit  int    `json:"limit,omitempty"`
}

type ClipsPageResponse struct {
    CrewID  string       `json:"crew_id"`
    Clips   []StoredClip `json:"clips"`
    Cursor  string       `json:"cursor,omitempty"`
    HasMore bool         `json:"has_more"`
}
```

Behavior: membership check, read clips doc, sort by `Ts` descending, apply cursor by matching `EventID`, page by `limit` (default and max `TimelinePageSize`). Does NOT update last_seen (this is deep history browsing, not the live feed).

### 7.2 crew_recaps

```go
type RecapsPageRequest struct {
    CrewID string `json:"crew_id"`
    Cursor string `json:"cursor,omitempty"` // week_start of previous page's last item, as string
    Limit  int    `json:"limit,omitempty"`
}

type RecapsPageResponse struct {
    CrewID  string            `json:"crew_id"`
    Recaps  []WeeklyRecapData `json:"recaps"`
    Cursor  string            `json:"cursor,omitempty"`
    HasMore bool              `json:"has_more"`
}
```

Behavior: membership check, read recaps doc, sort by `WeekStart` descending, page. Recaps have no id field, so use `WeekStart` as the cursor key.

### 7.3 Registration

In `main.go` `InitModule`, alongside the existing crew RPC registrations:

```go
initializer.RegisterRpc("crew_clips", CrewClipsRPC)
initializer.RegisterRpc("crew_recaps", CrewRecapsRPC)
```

---

## 8. crew_timeline: merge ephemeral + recent durable into the live feed

`crew_timeline` stays the single entrypoint for the live and this-week surface, so existing clients keep working with minimal change. It now merges three sources, all bounded to recent so the merge stays cheap:

1. Ledger events (already 7-day).
2. Clips from the clips doc with `Ts >= now - 7 days`.
3. The single most recent recap from the recaps doc, if any.

Map clips and the recap into `TimelineEntry` (type `"clip"` and `"weekly_recap"`, carrying their `Data`) so the existing client card mapping is preserved. Concatenate, sort by timestamp descending, paginate by index using the existing cursor approach. Keep the `updateLastSeen` call.

This means the live feed shows clips again (they were being read from the ledger before), and a quiet current week still surfaces the latest recap. The deep durable history (older clips, all past recaps) is reached through the new RPCs in section 7, which Delta 2 and 3 wire into the spine.

Leave the timeline ordering as timestamp descending. The "peaks not time" feeling is delivered by the durable spine existing at all plus client-side curation, not by reordering the live feed.

---

## 9. Catch-up: include recent clips

`buildCatchup` currently ranks only ledger events. Clips are high-value catch-up, so merge recent clips into the candidate set before ranking:

In `CrewCatchupRPC`, after reading the ledger, also read the clips doc and build synthetic `CrewEvent` values (type `"clip"`, score 50, `Data` = the clip) for clips with `Ts > lastSeen`, append them to the events slice passed to `buildCatchup`. The existing `selectDiverse` and `renderEventFragment` `"clip"` branch already handle the rest. No template changes needed.

Recaps in catch-up are low value; do not add them to catch-up in this delta.

---

## 10. Entitlement seam

New file `backend/nakama/data/modules/entitlements.go`:

m3llo+ is a per-user subscription, so entitlement keys on the viewing user, not the crew. Durable clips and recaps remain crew-owned; what a given user can unlock (unbounded history, locked-card replay) depends on that user's status.

```go
package main

// IsUserPremium reports whether a user has an active m3llo+ subscription.
// Single gate for future paid behavior: unbounded clip history, locked-card
// unlock, extended retention. No gating is built yet, so this returns false
// and nothing in Delta 1 consults it for deletion or limiting. It exists so
// the m3llo+ work has one well-known seam to wire, not a scattered set of checks.
func IsUserPremium(userID string) bool {
    return false
}
```

Do not call this anywhere in Delta 1. It is a documented placeholder. The clip cap, retention, and any locked or upsell rendering that will eventually read it are future work behind the m3llo+ track.

---

## 11. Infrastructure action item (not code)

CLIPS.md section 6.2 states free-tier clip media is deleted from R2 after 7 days via a lifecycle rule. We now persist clip cards and media for everyone. Before this ships, confirm on the R2 bucket `mello-clips`:

- If a lifecycle rule deletes `crews/*` objects after 7 days, disable it.
- If no such rule exists, no action.

Storage cost remains negligible per CLIPS.md (about 5MB per active crew per month, zero egress). This is a Bob action, flagged here so it is not missed.

---

## 12. Migration

None. m3llo has no production data. Do not write backfill or compatibility code. Existing `weekly_recap` and `clip` entries in any dev ledger can be ignored; they will age out of the ledger on their own and the new stores start empty.

---

## 13. Spec resync (do this in the same PR)

These docs have drifted from the implementation. Bring them back in sync as part of this work.

**16-CREW-EVENT-LEDGER.md**
- Change Status from Planned to Implemented.
- Document that the ledger is now strictly the 7-day ephemeral pulse store. Clips and recaps are no longer ledger events.
- Add a short section pointing to the new `crew_clips` and `crew_recaps` collections and the `crew_clips` / `crew_recaps` RPCs.
- Update the document-size budget note to drop clips and recaps from the ledger estimate.

**CLIPS.md**
- Update section 6.1 and 6.2 retention: clips are durable in `crew_clips/{crew_id}` (capped at most-recent 500 for now), not 7-day. Remove the "cloud copy deleted after 7 days" line, replace with the cap and the future m3llo+ unbounded-history note.
- Update section 10 (weekly recap) and section 11 (crew feed data) to reflect the durable recaps store and the new RPCs.
- Update the v1 scope list: `crew_timeline` now merges durable sources; add `crew_clips` and `crew_recaps`.
- Change Status to reflect the durable-memory work landing.

**09-MONETIZATION.md**
- Do not change in this delta. Flag only: monetization is now per-user (m3llo+ subscription), which aligns with the existing per-user "aura" concept but contradicts the crew-level history perks and the "clip creation is fully paid" line. Reconcile when the gating track starts. Out of scope here.

---

## 14. Done criteria

- A clip posted via `post_clip` survives more than 7 days and still appears in `crew_clips` and in `crew_timeline`.
- A weekly recap generated by the job survives more than 7 days and appears in `crew_recaps`.
- The ledger doc no longer contains `clip` or `weekly_recap` entries after this change.
- `crew_timeline` still returns clips and the latest recap in the live feed (existing client card mapping unbroken).
- `crew_clips` and `crew_recaps` paginate correctly with cursors.
- Clip rate limit (50 per user per day) still enforced, now counted from `crew_clips`.
- `crew_catchup` surfaces a recent clip.
- Clips doc never exceeds 500 entries; oldest are trimmed, R2 media untouched.
- `go build` clean, existing crew-events and clips tests pass, new tests for `AppendClip` cap behavior and the two new RPCs added.

## 15. Out of scope (Delta 2 and 3)

- The layered feed (pulse, this-week, memory spine) rendering.
- The presence-driven pulse row ("crew was hanging out 6h ago").
- Any locked-card or upsell treatment.
- Wiring `crew_clips` and `crew_recaps` into the desktop and iOS spine UIs.
- Share export, m3llo+ flow, gating enforcement.

## 16. Claudie rules

Spec-first. Build, run done-criteria, report. Do not open a PR autonomously. Prefer the delta changes described here over rewriting the files. No em-dashes or exclamation points in code comments.
