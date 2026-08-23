# MELLO Feed Curation & Personal Stats Specification

> **Component:** Personal Stats Surface, Crew Feed Curation for Game Sessions
> **Version:** 0.3
> **Status:** Implemented — T0 notability scoring, adaptive threshold, feed budget, the `CREW PLAY` rollup and the You strip all ship. The deep profile view (§2.4) and the rich telemetry session card (§3.6) are **not built**.
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [17-GAME-SENSING.md](./17-GAME-SENSING.md), [18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md), [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md), [20-GAME-UI-SURFACES.md](./20-GAME-UI-SURFACES.md), [04-BACKEND.md](./04-BACKEND.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md)

---

## 1. Overview

Spec 18 produces game outcomes and per-user streaks. Spec 17 produces sessions
for *every* game, with or without telemetry. This spec decides **which of them
are worth showing**, and carries the personal stats surface.

It does not draw anything. What the cards look like is
[20-GAME-UI-SURFACES.md](./20-GAME-UI-SURFACES.md).

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

### The decision that changed

The original design gated feed cards on **telemetry signals** — rank changes,
streak milestones, personal bests. Nine adapters exist against a catalogue of
thousands of games, so that gate silently excluded the overwhelming majority of
play. A user whose games have no adapter would never appear in their crew's feed
at all.

The gate is now **T0-first**: duration and company earn a card on their own, and
telemetry bonuses stack on top when they happen to exist. "ostkatt played
Valorant for 4h" is the product promise, and it must not require an adapter.

### Decisions (locked with the operator)

| Decision | Choice | Consequence |
|---|---|---|
| Personal surface | Both a You strip and a deeper profile | Survey ask served independently of the feed |
| Feed personalization | Identical for everyone | Feed filters on notability only; ownership is irrelevant there |
| Volume handling | Adaptive notability threshold | One knob scales low to high volume crews |
| Routine sessions in feed | **Own `CREW PLAY` rollup card** | Superseded the original "fold into the weekly recap" choice — see §3.3 |
| Sessions without telemetry | **Earn cards on duration and company** | The majority of play is visible at all |

### Why "identical feed" simplifies everything

Because the feed is the same for everyone, it never asks *"whose session is
this?"* — only *"is this crew-worthy?"* Ownership matters **only** in the
personal lane. So the two-axis model (notability × ownership) collapses: the
feed is a pure notability filter, and the personal lane carries everything about
you, always.

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

> **Shipped.** Note the selection rule changed: the strip picks the most
> recently played game outright (`games.first()`), not the first game with a
> win/loss record. Selecting by record meant a user whose games have no
> adapter saw an empty strip forever. When no record exists it shows play
> time instead — `weekly_minutes` / `weekly_time_text` in
> `client/src/handlers/stats.rs`, which apply the overnight guard
> ([20](./20-GAME-UI-SURFACES.md) §4.3).

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

> Designed, never built. `RecentDays` exists in the store to back the
> trend views below, so the data side is ready. Unbuilt scope, not a defect.

A dedicated stats view, deeper than the strip:
- Per-game cards: streak (current/longest), W/L/D record, win-rate, recent-form sparkline.
- Streak-over-time and win-rate trend (from the rolling history).
- Rank progress where the adapter provides it (e.g. League LP; CS2 Premier rating only if a source exists — see §3.5).

---

---

## 3. Lane B — Crew Feed Curation

Shared, identical for all members. Implemented in
[crew_feed.go](../backend/nakama/data/modules/crew_feed.go) (`buildThisWeek`,
`pruneGameSessions`, `fillerPriority`, `fillerRole`, `sessionPreviewQuality`).

### 3.1 Notability gate

`gameSessionQuality(card)` scores every `game_session`. It is built in two
layers, and the order matters: **the T0 base always applies, and telemetry only
adds.**

**T0 base — `gameSessionT0Score`. Needs no adapter.**

| Signal | Score |
|---|---|
| Duration ≥ 240 min | +40 |
| Duration ≥ 120 min | +25 |
| Duration ≥ 30 min | +10 |
| Each co-player beyond the actor | +20, capped at +60 |

**Telemetry bonuses, added only when `wins + losses + draws > 0`:**

| Signal | Score |
|---|---|
| Streak magnitude ≥ 5 | +120 |
| Streak magnitude ≥ 3 | +80 |
| 3+ wins, zero losses (flawless night) | +70 |
| 5+ losses, zero wins (rough night, sympathy card) | +50 |
| 8+ matches | +50 |
| 5+ matches | +20 |

A non-`game_session` card returns `feedMinQuality`, so it never competes in this
gate.

### 3.2 Adaptive threshold

The bar and the cap both move with how much the crew played that week, so a
quiet crew sees its play and a loud crew does not drown.

| Game sessions in the week | Floor (`gameSessionNotableFloor`) | Card cap (`gameSessionCardCap`) |
|---|---|---|
| ≤ 5 | 10 | 4 |
| 6–15 | 30 | 2 |
| > 15 | 50 (`feedGameSessionNotableMin`) | 2 (`feedGameSessionMaxCards`) |

Note what the low band buys: a floor of 10 is exactly one 30-minute solo
session, so in a quiet crew any real session earns a card. A crew override
("show everything" ↔ "highlights only") remains a future option.

### 3.3 Routine play — the `CREW PLAY` rollup

`pruneGameSessions` returns the pruned sessions separately, and
`buildGameRollup` synthesizes **one card per crew per day** from them: per-member
top lines (`ostkatt · Valorant · 4h`), session count, and total crew hours.
Fewer than three pruned sessions produces no rollup.

> **This supersedes the original design.** Version 0.2 of this spec said routine
> play was "**not** given its own feed card" and would live inside the weekly
> recap instead, to keep the recap worth paying for. That was reversed: a
> premium-gated recap would have hidden the ordinary play of non-paying crews
> entirely, which contradicts the T0 decision in §1. The recap enrichment in
> §3.5 is still wanted, but it is additive, not the home for routine play.

Rollup minutes obey the overnight guard — see
[20](./20-GAME-UI-SURFACES.md) §4.3. `reportableMinutes` exists in
`crew_feed.go` for exactly this, because summing raw wall time made the rollup
contradict the session cards below it.

### 3.4 Curation budget

`buildThisWeek`:

- Non-game cards pass through untouched and in order.
- `game_session` cards above the floor survive, capped by volume (§3.2).
- Pruned sessions feed the rollup (§3.3).
- `game_session` and `game_rollup` are both registered in `mapCardType`,
  `fillerPriority` and `fillerRole`.

Curation stays **server-side**: only `order / role / size / type` cross to the
client, so threshold and budget tuning need no client release.

### 3.5 Weekly recap enrichment — NOT BUILT

The recap already carries `GameRecords` (per-member W/L) and `BestStreak` from
spec 18 ([crew_recaps.go](../backend/nakama/data/modules/crew_recaps.go)).
Extending it into a "this week in games" section was designed but not built:

```go
GamesPlayed   []GameTally       `json:"games_played"`
Leaderboard   []RecapGameRecord `json:"leaderboard"`
Awards        []RecapAward      `json:"awards"`
```

Candidate awards: grinder of the week (most matches), biggest heater / worst
skid, most improved (win-rate delta), clutch counts where the adapter provides
them, head-to-head rivalry, comeback (snapped the longest skid).

This is unbuilt scope, not a defect. No bug report will surface it.

### 3.6 Per-game data degradation

Cards render whatever subset of stats exists and **never show an empty slot**.

| Game | Available | Not available |
|---|---|---|
| CS2 (GSI) | K/D, W/L, streak, MVPs, map | ADR, HS%, Premier rating |
| League (Live Client API) | KDA, CS/min, vision, rank/LP | — |
| Valorant, Fortnite, most of the catalogue | duration, co-players | everything else |

This is why the session card has two bodies rather than one — see
[20](./20-GAME-UI-SURFACES.md) §4.1. The **compact body ships**; the rich
telemetry card from the mockup, with its full CS2 stat set, is **not built** and
needs a source that does not exist yet (a Leetify/Steam-style API or scoreboard
OCR), tracked in spec 18 future work.

---

## 4. Data Model & API Summary

| Change | Where | Status |
|---|---|---|
| `Draws`, `RecentForm`, `LastPlayed` on `UserGameStats` | `user_game_stats.go` | Shipped |
| `PlayedMinTotal`, `ActiveMinTotal`, `RecentDays` | `user_game_stats.go` | Shipped |
| `user_game_stats_get` RPC | `user_game_stats.go` + `main.go` | Shipped |
| `gameSessionT0Score`, `gameSessionQuality`, adaptive floor/cap | `crew_feed.go` | Shipped |
| `pruneGameSessions`, `buildGameRollup`, `reportableMinutes` | `crew_feed.go` | Shipped |
| You strip | `client/src/handlers/stats.rs`, `client/ui/panels/*` | Shipped |
| Weekly recap game section | `crew_recaps.go` | **Not built** (§3.5) |
| Deep profile view | client | **Not built** (§2.4) |
| Rich telemetry session card | client | **Not built** (§3.6) |

---

## 5. Build Order

Complete. Delivered in this order, each step shippable:

1. T0 scoring + compact session card — the dream sentence reaches the feed.
2. Per-member playing line — sidebar parity ([20](./20-GAME-UI-SURFACES.md) §3.3).
3. `CREW PLAY` rollup — scales the feed to loud crews.
4. Per-co-player `overlap_min` — data only; no surface renders it yet.
5. You strip play time — every game gets stats, not just the nine with adapters.

---

## 6. Testing

- **Pure curation (Go, no Nakama):** `gameSessionT0Score` and `gameSessionQuality` ranking, including that a telemetry-free session still scores; floor and cap steps across simulated low/medium/high volume; `pruneGameSessions` caps surviving cards; `buildGameRollup` aggregates pruned sessions and returns false under three; `reportableMinutes` overnight guard, including that an overnight session does not inflate the rollup it appears in.
- **Stats (Go):** `user_game_stats_get` returns only the caller's data, sorted by `last_played`; `RecentForm` capped and ordered; draws counted without moving the streak; `applySessionPlayTime` accumulates wall and active minutes.
- **Client:** You strip renders from `user_game_stats_get`, shows play time when no W/L record exists, empty state when no games at all.
- **Not covered.** None of these surfaces has run against a live backend with real crew traffic. The manual pass — drive several sessions across members with the spec-18 emulator and confirm the feed shows a handful of session cards plus one `CREW PLAY` rollup rather than a wall — has not been done.

---

## 7. Out of Scope / Future

Designed but **not built**, recorded so they are not mistaken for bugs:

- Deep profile view (§2.4).
- Weekly recap game section — leaderboard and awards (§3.5).
- Rich telemetry session card (§3.6).
- Co-play "played together for Xh" copy. `player_overlap_min` is recorded; no
  surface reads it. See [20](./20-GAME-UI-SURFACES.md) §4.4 for the rule it must
  follow when written.

Genuinely out of scope:

- Per-match streaks (this spec keeps spec 18's per-session granularity).
- External stat sources for CS2 ADR/HS%/rating (spec 18 future).
- Crew-configurable curation ("show everything" toggle).
- Cross-game "career" profile and seasonal resets.

---

*This spec covers curation: which game sessions earn a place in the feed, and the personal stats lane. What the surfaces look like is [20-GAME-UI-SURFACES.md](./20-GAME-UI-SURFACES.md). The outcome data it consumes is produced by [18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md), the sessions by [17-GAME-SENSING.md](./17-GAME-SENSING.md). For the ledger mechanics it builds on, see [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md).*
