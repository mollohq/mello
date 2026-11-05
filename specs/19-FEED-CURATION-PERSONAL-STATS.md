# MELLO Feed Curation & Personal Stats Specification

> **Component:** Personal Stats Surface, Crew Feed Curation for Game Sessions
> **Version:** 0.4
> **Status:** Implemented. The crew lane is the zone feed ([CREW-FEED-ZONES.md](./features/CREW-FEED-ZONES.md)): every session of the free window is a card, so the notability gate and the curation budget of version 0.3 are retired. The deep profile view (section 2.4) and the rich telemetry session card (section 3.6) are not built.
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [17-GAME-SENSING.md](./17-GAME-SENSING.md), [18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md), [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md), [22-GAME-UI-SURFACES.md](./22-GAME-UI-SURFACES.md), [04-BACKEND.md](./04-BACKEND.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md)

---

## 1. Overview

Spec 18 produces game outcomes and per-user streaks. Spec 17 produces a session
for every game, with or without telemetry. This spec selects which sessions to
show. It also defines the personal stats surface.

This spec does not define how a card looks. See
[22-GAME-UI-SURFACES.md](./22-GAME-UI-SURFACES.md).

```
┌────────────────────────────┐     ┌────────────────────────────────────┐
│ PERSONAL LANE (per-user)   │     │ CREW LANE (shared, identical)      │
│ "my win/loss streaks"      │     │ the curated crew feed              │
│                            │     │                                    │
│  • You strip (glance)      │     │  • every session → a card          │
│  • Profile (NOT BUILT)     │     │  • the day → Today on the board    │
│                            │     │  • the week → the weekly recap     │
│  always on · no curation   │     │  • same for every member           │
│  backed by user_game_stats │     │  built on crew_feed_zones.go       │
└────────────────────────────┘     └────────────────────────────────────┘
```

### The change from version 0.3

Version 0.3 scored every game session and kept the notable ones, to a cap
that fell as the crew played more. The rest went into a recent games card.
The zone feed shows every session of the free window in the Sessions
carousel, newest first, so the gate, the cap and the recent games card are
gone. The day goes to Today on the board, the week to the weekly recap.

### The change from version 0.2

Version 0.2 gated feed cards on telemetry signals: rank changes, streak
milestones and personal bests. Nine adapters cover a catalogue of thousands of
games. That gate excluded most play. A user whose games have no adapter never
appeared in the crew feed.

The gate is now T0-first. Duration and company earn a card without an adapter.
Telemetry bonuses add to that score when they exist.

### Decisions

| Decision | Choice | Consequence |
|---|---|---|
| Personal surface | A You strip and a deeper profile | The survey request is served without the feed |
| Feed personalization | Identical for every member, except Crew history, which follows the viewer's m3llo+ | CREW-FEED-ZONES.md section 7 |
| Volume handling | Every session of the free window in one carousel | A busy crew pages the carousel; nothing is pruned |
| Sessions without telemetry | A card like any other | All play stays visible |

### Effect of an identical feed

The feed never asks which member owns a session. Ownership applies to the
personal lane only. The personal lane always carries the user's own data.

---

## 2. Lane A — Personal Stats

Per-user, always available, no curation, unaffected by crew volume. This is the direct answer to the survey.

### 2.1 Data (extends spec 18's `user_game_stats`)

The `user_game_stats/{game_id}` store (owner-read, server-write) gains display-oriented fields:

```go
type UserGameStats struct {
    GameID            string   `json:"game_id"`
    Wins              int      `json:"wins"`
    Losses            int      `json:"losses"`
    Draws             int      `json:"draws"`              // NEW — draws count toward play, not streak
    CurrentStreak     int      `json:"current_streak"`     // signed (sessions)
    LongestWinStreak  int      `json:"longest_win_streak"`
    LongestLossStreak int      `json:"longest_loss_streak"`
    RecentForm        []string `json:"recent_form"`        // NEW — last ~10 sessions: "W"|"L"|"D"
    LastResult        string   `json:"last_result"`
    LastPlayed        int64    `json:"last_played"`        // NEW — for "active/top game" selection
    UpdatedAt         int64    `json:"updated_at"`
    PlayedMinTotal    int              `json:"played_min_total"` // NEW - lifetime wall minutes
    ActiveMinTotal    int              `json:"active_min_total"` // NEW
    RecentDays        []RecentDayEntry `json:"recent_days"`      // NEW - oldest first, capped
}

```go
type RecentDayEntry struct {
    Date      string `json:"date"`       // "YYYY-MM-DD" (UTC)
    WallMin   int    `json:"wall_min"`
    ActiveMin int    `json:"active_min"`
}
```

Win-rate is derived (`wins / (wins+losses)`), not stored. Draws now appear here, which also resolves the "draw-only session showed nothing" gap from the first CS2 test — a draw surfaces in `RecentForm` and the recap rollup even though it doesn't move the streak.

> Per-match performance aggregates (K/D, MVPs) for the rich card and profile come from spec 18's match capture (CS2 GSI `player_match_stats`) and are tracked there; this spec consumes them.

### 2.2 RPC

```
user_game_stats_get   →   { "games": [ UserGameStats, ... ] }  // sorted by last_played desc
```
Authenticated; returns only the caller's own stats across all games. Owner-read enforced by storage permissions.

### 2.3 You strip

> **Implemented.** The selection rule changed. The strip picks the most
> recently played game with `games.first()`. It no longer picks the first game
> that has a win/loss record. Selection by record left an empty strip for any
> user whose games have no adapter.
>
> The strip shows play time when no record exists. Functions: `weekly_minutes`
> and `weekly_time_text` in `client/src/handlers/stats.rs`. Both apply the
> overnight guard. See [22](./22-GAME-UI-SURFACES.md) section 4.3.

A compact card pinned at the **top of the crew feed**, showing the viewer's top/active game:

```
[CS2]  Counter-Strike 2          W4 streak
       62% WR · 5–3 this week    Gold II ↑
       recent: W W L W D                  ›
```

- Source: `user_game_stats_get`, pick the most recently played game (or let the user pin one).
- Tappable → profile.
- Always present (even at zero crew activity); shows an empty/encouraging state if no games tracked.

### 2.4 Profile / "Me" view — NOT BUILT

> Designed. Not built. The store holds `RecentDays`, which supplies the trend
> views below, so the data is available.

A dedicated stats view, deeper than the strip:
- Per-game cards: streak (current/longest), W/L/D record, win-rate, recent-form sparkline.
- Streak-over-time and win-rate trend (from the rolling history).
- Rank progress where the adapter provides it (e.g. League LP; CS2 Premier rating only if a source exists — see §3.5).

---

---

---

## 3. Lane B — Crew Feed

The crew feed is the zone feed. Its zones and payload are in
[CREW-FEED-ZONES.md](./features/CREW-FEED-ZONES.md). Implementation:
[crew_feed_zones.go](../backend/nakama/data/modules/crew_feed_zones.go).

### 3.1 Sessions

`zoneSessions` puts every game, voice and stream session of the free window
in the Sessions zone, newest first. A session with no reportable minutes has
no card (`cardHasContent`). Minutes use the overnight guard, see
[22](./22-GAME-UI-SURFACES.md) section 4.3; `reportableMinutes` in
`crew_feed.go` applies it.

### 3.2 Installed games on a quiet week

On a quiet week with no session in the window, `zoneInstalled` sends the
games most members have installed (`installed`): the library documents of
the crew, one batched storage read, the most owners first, at most three. A
crew of one gets its own games on the solo board. Spec
[22](./22-GAME-UI-SURFACES.md) section 3.4 has the merge rules of the tiles.

### 3.5 Weekly recap enrichment

The recap carries `GameRecords` and `BestStreak` from spec 18. See
[crew_recaps.go](../backend/nakama/data/modules/crew_recaps.go).

The recap also carries the head2head belts. See
[HEAD2HEAD.md](./features/HEAD2HEAD.md) section 6.

| Field | Content |
|---|---|
| `belts_moved` | Titles whose holder changed in the recap week, with the holder, the previous holder and the value |
| `belts_held` | The number of titles each member holds when the recap is generated |
| `awards` | The existing game awards, then one `belt` award for each moved belt |
| `leaderboard` | The same entries as `belts_held` |

- The job reads the belts from the holders doc at generation time.
- A recap with no activity has no belts.
- `GamesPlayed` is built.

Awards not built: largest win-rate change, clutch counts where the adapter
supplies them, head-to-head record, and longest loss streak ended.

### 3.5a Daily recap

A daily recap is the weekly recap for one day, smaller. The server builds it
from the sources the weekly recap reads. See
[crew_daily_recap.go](../backend/nakama/data/modules/crew_daily_recap.go).
The board shows the recap of the current UTC day as its Today tab
(CREW-FEED-ZONES.md section 3, `board.today`): hangout, clips, sessions, the
members who took part and the top game.

- A day belongs to the UTC day of the event timestamp. The day that is not over
  is today so far.
- A day with no minutes and no clip has no recap (`dailyRecapHasContent`). A
  belt that moved on that day does not make it a day. Holding titles is a
  standing state, as in the weekly recap.
- The day must also add something: a clip, a belt moved that day, or three or
  more active members. Otherwise it repeats the weekly recap, and Today is
  empty.

| Field | Content |
|---|---|
| `day_start`, `day_end` | 00:00 UTC of the day and of the next day, in ms |
| `total_hangout_min` | Voice and stream minutes of the day |
| `total_game_min` | Game minutes, counted by `reportableMinutes` |
| `total_min` | The two together. Today leads with it |
| `session_count` | Voice, stream and game sessions with reportable minutes |
| `clip_count` | Clips of the day, from the clips doc |
| `top_game`, `game_id` | The game with the most reportable minutes. A tie goes to the name that sorts first |
| `member_ids` | Members who took part, the most active first, at most eight |
| `belts_moved` | Titles whose `moved_at_ms` is in the day, newest first, at most two. The same shape as the weekly recap |
| `sessions` | The sessions of the day with reportable minutes, the one that ended last first, at most four: `kind` (`voice`, `stream`, `game`), `title`, `game_id`, `member_ids`, `duration_min`, `peak`, `end_ms` |

The sessions themselves are cards in the Sessions zone, voice sessions too.

### 3.6 Per-game data degradation

A card renders the stats that exist. A card must not show an empty slot.

| Game | Available | Not available |
|---|---|---|
| CS2 (GSI) | K/D, W/L, streak, MVPs, map | ADR, HS%, Premier rating |
| League (Live Client API) | KDA, CS/min, vision, rank/LP | — |
| Valorant, Fortnite, most of the catalogue | Duration, co-players | All match statistics |

For this reason the session card has two bodies. See
[22](./22-GAME-UI-SURFACES.md) section 4.1. The compact body is implemented.

The rich telemetry card from the mockup is not built. Its full CS2 stat set
needs a source that does not exist yet, such as a Leetify-style API or scoreboard
OCR. Spec 18 tracks that source.

---

## 4. Data Model & API Summary

| Change | Where | Status |
|---|---|---|
| `Draws`, `RecentForm`, `LastPlayed` on `UserGameStats` | `user_game_stats.go` | Implemented |
| `PlayedMinTotal`, `ActiveMinTotal`, `RecentDays` | `user_game_stats.go` | Implemented |
| `user_game_stats_get` RPC | `user_game_stats.go`, `main.go` | Implemented |
| `reportableMinutes` | `crew_feed.go` | Implemented |
| Sessions and installed games of the zone feed | `crew_feed_zones.go` | Implemented. Section 3 |
| You strip | `client/src/handlers/stats.rs`, `client/ui/panels/*` | Implemented |
| Weekly recap belts | `crew_recaps.go` | Implemented. Section 3.5 |
| Daily recap, `buildDailyRecaps` | `crew_daily_recap.go`, Today on the board | Implemented. Section 3.5a |
| Deep profile view | Client | Not built. Section 2.4 |
| Rich telemetry session card | Client | Not built. Section 3.6 |

---

## 5. Build Order

The build is complete. The steps were delivered in this order.

1. T0 scoring and the compact session card.
2. Per-member playing line. See [22](./22-GAME-UI-SURFACES.md) section 3.3.
3. `CREW PLAY` rollup.
4. Per-co-player `overlap_min`. Data only. No surface reads it.
5. You strip play time.
---

## 6. Testing

Feed, in Go, without Nakama (`crew_feed_zones_test.go`):

- Every session of the window is in the Sessions zone, newest first.
- The installed games show only on a quiet week with no session.
- `reportableMinutes` overnight guard.

Daily recap, in Go (`crew_daily_recap_test.go`):

- Events group by UTC day, newest day first, with their own sums.
- A day with nothing has no entry. A belt alone does not make a day.
- At most two belts, newest first. The top game ties go by name.
- The recap of the current day is the Today of the board.

Stats, in Go:

- `user_game_stats_get` returns only the caller's data, sorted by `last_played`.
- `RecentForm` is capped and ordered.
- A draw counts, but does not change the streak.
- `applySessionPlayTime` adds wall and active minutes.

Client:

- The You strip renders from `user_game_stats_get`.
- The strip shows play time when no W/L record exists.
- The strip shows an empty state when the user has no games.

End to end: the journeys `feed-day-card` (Today with three members) and
`feed-stage-two` (a session card with game art) run against a local stack.

---

## 7. Out of Scope / Future

Designed, not built:

| Item | Section |
|---|---|
| Deep profile view | 2.4 |
| Weekly recap game section | 3.5 |
| Rich telemetry session card | 3.6 |
| Co-play duration copy | [22](./22-GAME-UI-SURFACES.md) 4.4 |

The ledger records `player_overlap_min`. No surface reads it.

Out of scope:

- Per-match streaks. This spec keeps the per-session granularity of spec 18.
- External stat sources for CS2 ADR, HS% and rating. See spec 18.
- Crew-configurable curation.
- Cross-game career profile and seasonal resets.

---

*This spec defines curation and the personal stats lane. For card appearance,
see [22-GAME-UI-SURFACES.md](./22-GAME-UI-SURFACES.md). Spec
[18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md) produces the outcome data. Spec
[17-GAME-SENSING.md](./17-GAME-SENSING.md) produces the sessions. Spec
[16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md) defines the ledger.*
