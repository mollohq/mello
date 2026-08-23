# MELLO Feed Curation & Personal Stats Specification

> **Component:** Personal Stats Surface, Crew Feed Curation for Game Sessions
> **Version:** 0.3
> **Status:** Implemented. The deep profile view (section 2.4), the weekly recap game section (section 3.5) and the rich telemetry session card (section 3.6) are not built.
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
│  • You strip (glance)      │     │  • session clears the bar → card   │
│  • Profile (NOT BUILT)     │     │  • the rest → CREW PLAY rollup     │
│                            │     │  • bar rises with crew volume      │
│  always on · no curation   │     │  • same for every member           │
│  backed by user_game_stats │     │  built on crew_feed.go             │
└────────────────────────────┘     └────────────────────────────────────┘
```

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
| Feed personalization | Identical for every member | The feed filters on notability only |
| Volume handling | Adaptive notability threshold | One control scales low and high volume crews |
| Routine sessions in feed | A `CREW PLAY` rollup card | Replaces the version 0.2 choice. See section 3.3 |
| Sessions without telemetry | Earn cards from duration and company | Most play stays visible |

### Effect of an identical feed

The feed never asks which member owns a session. It asks only whether the crew
wants to see it. Ownership applies to the personal lane only. The feed is a
notability filter. The personal lane always carries the user's own data.

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

## 3. Lane B — Crew Feed Curation

The crew feed is shared and identical for all members. Implementation:
[crew_feed.go](../backend/nakama/data/modules/crew_feed.go). Functions:
`buildThisWeek`, `pruneGameSessions`, `fillerPriority`, `fillerRole`,
`sessionPreviewQuality`.

### 3.1 Notability gate

`gameSessionQuality(card)` scores every `game_session`. The score has two parts.
The T0 base always applies. Telemetry only adds to it.

T0 base, from `gameSessionT0Score`. No adapter is needed.

| Signal | Score |
|---|---|
| Duration 240 min or more | +40 |
| Duration 120 min or more | +25 |
| Duration 30 min or more | +10 |
| Each co-player after the actor | +20, to a maximum of +60 |

Telemetry bonuses. These apply only when `wins + losses + draws > 0`.

| Signal | Score |
|---|---|
| Streak magnitude 5 or more | +120 |
| Streak magnitude 3 or more | +80 |
| 3 or more wins, no losses | +70 |
| 5 or more losses, no wins | +50 |
| 8 or more matches | +50 |
| 5 or more matches | +20 |

A card that is not a `game_session` returns `feedMinQuality`. It does not enter
this gate.

### 3.2 Adaptive threshold

The floor and the card cap change with the number of sessions in the week.

| Game sessions in the week | Floor (`gameSessionNotableFloor`) | Card cap (`gameSessionCardCap`) |
|---|---|---|
| 5 or fewer | 10 | 4 |
| 6 to 15 | 30 | 2 |
| More than 15 | 50 (`feedGameSessionNotableMin`) | 2 (`feedGameSessionMaxCards`) |

A floor of 10 equals one 30-minute solo session. In a low-volume crew, any real
session earns a card.

A crew override, from "show everything" to "highlights only", is a future
option.

### 3.3 Routine play — the `CREW PLAY` rollup

`pruneGameSessions` returns the removed sessions separately. `buildGameRollup`
builds one card per crew per day from them.

| Element | Content |
|---|---|
| Member line | `ostkatt · Valorant · 4h` |
| Footer | Session count and total crew hours |

Fewer than three removed sessions produce no rollup card.

Version 0.2 stated that routine play gets no card of its own, and that the crew
aggregate belongs in the weekly recap. The weekly recap is a premium surface.
That design hid the ordinary play of non-paying crews, which contradicts the
T0 decision in section 1. The recap enrichment in section 3.5 is still wanted,
but it does not hold routine play.

Rollup minutes use the overnight guard. See
[22](./22-GAME-UI-SURFACES.md) section 4.3. `reportableMinutes` in
`crew_feed.go` applies it. Raw wall time made the rollup contradict the session
cards below it.

### 3.4 Curation budget

`buildThisWeek` applies these rules:

- Cards that are not game sessions pass through unchanged and in order.
- `game_session` cards above the floor survive, to the cap in section 3.2.
- Removed sessions go to the rollup in section 3.3.
- `mapCardType`, `fillerPriority` and `fillerRole` register both `game_session`
  and `game_rollup`.

Curation runs server-side. Only `order`, `role`, `size` and `type` cross to the
client. A threshold change needs no client release.

### 3.5 Weekly recap enrichment — NOT BUILT

The recap carries `GameRecords` and `BestStreak` from spec 18. See
[crew_recaps.go](../backend/nakama/data/modules/crew_recaps.go). The extension
below is designed but not built.

```go
GamesPlayed   []GameTally       `json:"games_played"`
Leaderboard   []RecapGameRecord `json:"leaderboard"`
Awards        []RecapAward      `json:"awards"`
```

Candidate awards: most matches played, longest win streak, longest loss streak,
largest win-rate change, clutch counts where the adapter supplies them,
head-to-head record, and longest loss streak ended.

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
| `gameSessionT0Score`, `gameSessionQuality`, floor and cap | `crew_feed.go` | Implemented |
| `pruneGameSessions`, `buildGameRollup`, `reportableMinutes` | `crew_feed.go` | Implemented |
| You strip | `client/src/handlers/stats.rs`, `client/ui/panels/*` | Implemented |
| Weekly recap game section | `crew_recaps.go` | Not built. Section 3.5 |
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

Curation, in Go, without Nakama:

- `gameSessionT0Score` and `gameSessionQuality` ranking. A session without
  telemetry must still score.
- Floor and cap steps at low, medium and high volume.
- `pruneGameSessions` limits the number of surviving cards.
- `buildGameRollup` aggregates removed sessions. It returns false below three.
- `reportableMinutes` overnight guard. An overnight session must not increase
  the rollup total.

Stats, in Go:

- `user_game_stats_get` returns only the caller's data, sorted by `last_played`.
- `RecentForm` is capped and ordered.
- A draw counts, but does not change the streak.
- `applySessionPlayTime` adds wall and active minutes.

Client:

- The You strip renders from `user_game_stats_get`.
- The strip shows play time when no W/L record exists.
- The strip shows an empty state when the user has no games.

Not covered: no surface has run against a live backend with real crew traffic.
The manual test is not done. That test drives several sessions across members
with the spec-18 emulator. It then confirms that the feed shows a few session
cards and one `CREW PLAY` rollup.

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
