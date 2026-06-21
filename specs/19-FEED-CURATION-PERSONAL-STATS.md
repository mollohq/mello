# MELLO Feed Curation & Personal Stats Specification

> **Component:** Personal Stats Surface, Crew Feed Curation for Game Sessions
> **Version:** 0.1
> **Status:** Planned
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md), [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md), [04-BACKEND.md](./04-BACKEND.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md)

---

## 1. Overview

Spec 18 produces game outcomes and per-user streaks. This spec covers **how they are surfaced**: a personal "my stats" view (the literal survey ask — *"an overview of my win/loss streaks in CS"*) and a crew feed that mixes game sessions with clips, streams, and recaps **without being flooded** as a crew's play scales up.

The core realization: these are **two independent jobs** that were being conflated.

```
┌────────────────────────────┐     ┌────────────────────────────────────┐
│ PERSONAL LANE (per-user)   │     │ CREW LANE (shared, identical)        │
│ "my win/loss streaks"      │     │ the curated crew feed                │
│                            │     │                                      │
│  • You strip (glance)      │     │  • notability filter (is it          │
│  • Profile (depth)         │     │    crew-worthy?) → rich card         │
│                            │     │  • everything else → 1 digest card   │
│  always on · no curation · │     │  • threshold rises with crew volume  │
│  volume-proof              │     │  • SAME for every member             │
│  backed by user_game_stats │     │  built on crew_feed.go curation      │
└────────────────────────────┘     └────────────────────────────────────┘
```

### Decisions (locked with the operator)

| Decision | Choice | Consequence |
|----------|--------|-------------|
| Personal surface | **Both** a "You" strip + a deeper profile | Survey ask served independently of the feed |
| Feed personalization | **Identical for everyone** | Feed filters on *notability only*; ownership is irrelevant there |
| Volume handling | **Adaptive notability threshold** | One knob scales low → high volume crews |
| Routine sessions in feed | **Aggregated into one crew digest** | No per-session card spam |
| Notable sessions in feed | **Rich card (mockup), degraded to per-game available stats** | Earned, rare, high-signal |

### Why "identical feed" simplifies everything

Because the feed is the same for everyone, it never asks *"whose session is this?"* — only *"is this crew-worthy?"* Ownership matters **only** in the personal lane. So the two-axis model (notability × ownership) collapses: the **feed is a pure notability filter**, and the **personal lane carries everything about you**, always.

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
}
```

Win-rate is derived (`wins / (wins+losses)`), not stored. Draws now appear here, which also resolves the "draw-only session showed nothing" gap from the first CS2 test — a draw surfaces in `RecentForm` and the digest even though it doesn't move the streak.

> Per-match performance aggregates (K/D, MVPs) for the rich card and profile come from spec 18's match capture (CS2 GSI `player_match_stats`) and are tracked there; this spec consumes them.

### 2.2 RPC

```
user_game_stats_get   →   { "games": [ UserGameStats, ... ] }  // sorted by last_played desc
```
Authenticated; returns only the caller's own stats across all games. Owner-read enforced by storage permissions.

### 2.3 You strip (Phase A1)

A compact card pinned at the **top of the crew feed**, showing the viewer's top/active game:

```
[CS2]  Counter-Strike 2          W4 streak
       62% WR · 5–3 this week    Gold II ↑
       recent: W W L W D                  ›
```

- Source: `user_game_stats_get`, pick the most recently played game (or let the user pin one).
- Tappable → profile.
- Always present (even at zero crew activity); shows an empty/encouraging state if no games tracked.

### 2.4 Profile / "Me" view (Phase A2)

A dedicated stats view, deeper than the strip:
- Per-game cards: streak (current/longest), W/L/D record, win-rate, recent-form sparkline.
- Streak-over-time and win-rate trend (from the rolling history).
- Rank progress where the adapter provides it (e.g. League LP; CS2 Premier rating only if a source exists — see §3.5).

---

## 3. Lane B — Crew Feed Curation

Shared, identical for all members. Built on the existing curation in [crew_feed.go](../backend/nakama/data/modules/crew_feed.go) (`buildThisWeek`, `fillerPriority`, `fillerRole`, `sessionPreviewQuality`, `feedQuietBackendTypes`).

### 3.1 Notability gate

A `gameSessionQuality(card)` scorer (mirroring `sessionPreviewQuality`) decides whether a session **earns a rich card**. Signals (server-side, tunable):

| Signal | Example |
|--------|---------|
| Rank change | promoted/demoted a tier or division |
| Streak milestone | reached a 3+/5+ win streak, or **snapped** a long loss skid |
| Personal/seasonal best | career-high K/D, first ace, a flawless `5–0` night |
| Notably bad | a brutal `0–7` (sympathy card) |
| Big session | long duration × many matches |

Sessions below the bar do **not** get individual cards — they fold into the digest (§3.3).

### 3.2 Adaptive threshold

The bar rises with crew activity so the feed stays balanced at any scale:

| Crew volume | Threshold | Notable cards in `this_week` | Routine sessions |
|-------------|-----------|------------------------------|------------------|
| Low (few/wk) | low — almost anything qualifies | most sessions show | digest may be omitted |
| Medium | mid | crew highlights only | small digest |
| High (all day) | high — milestones only | ≤1–2/day | prominent rolling digest |

Computed from recent `game_session` volume for the crew (e.g. a percentile of the week's session quality scores, or a simple count-based step). Auto-tuned server-side; a crew override ("show everything" ↔ "highlights only") is a future option.

### 3.3 `games_digest` aggregate card

One crew-level rollup that absorbs all routine play — scale-invariant (always ≤1 in `this_week`):

```json
{
  "type": "games_digest",
  "period": "this_week",
  "total_matches": 312,
  "games": [{ "game": "Counter-Strike 2", "matches": 180 }, { "game": "Rocket League", "matches": 132 }],
  "leaders": [{ "name": "JD", "wins": 24, "losses": 9, "streak": 4 }],
  "best_streak": { "name": "b0bben", "n": 6 }
}
```

Built from the ledger's `game_session` events — the same inputs as the weekly recap's `GameRecords` + `BestStreak` (already implemented in [crew_recaps.go](../backend/nakama/data/modules/crew_recaps.go)); the digest is essentially a rolling/live version of that.

### 3.4 Curation budget

Extend `buildThisWeek`:
- Promote at most **N** (≈2) notable game-session cards, chosen by `gameSessionQuality`.
- Insert the single `games_digest` card.
- Routine `game_session` events no longer become individual cards (today they render as quiet/sm rows — the four identical "SESSION · Counter-Strike 2" cards seen in testing).
- Add `games_digest` and the rich session card to `mapCardType` / `fillerPriority` / `fillerRole`.

### 3.5 Per-game data degradation

The rich card (mockup) uses the **same IA, different stat slots**, populated by whatever the game's adapter actually provides:

| Game | Available (rich card slots) | Not available |
|------|------------------------------|---------------|
| CS2 (GSI) | K/D, W/L, streak, MVPs, map | ADR, HS%, Premier rating |
| League (Live Client API) | KDA, CS/min, vision, rank/LP | — (rich) |
| Apex | — (no official live API) | most live stats |

Cards must render gracefully with whatever subset exists; never show empty slots. The mockup's full CS2 stat set (ADR/HS%/rating) is **aspirational** and needs an extra source (a Leetify/Steam-style API or scoreboard OCR) tracked separately in spec 18 future work.

---

## 4. Data Model & API Summary

| Change | Where | New/Modified |
|--------|-------|--------------|
| `Draws`, `RecentForm`, `LastPlayed` on `UserGameStats` | `user_game_stats.go` | Modified (additive) |
| `user_game_stats_get` RPC | new `user_game_stats.go` handler + `main.go` registration | New |
| `gameSessionQuality()` + threshold | `crew_feed.go` | New |
| `games_digest` card type | `crew_feed.go`, client feed renderer | New |
| `buildThisWeek` budget + digest insert | `crew_feed.go` | Modified |
| You strip + Profile surfaces | `client/ui/panels/*`, handlers, a `Command`/`Event` for `user_game_stats_get` | New |

Curation stays server-side: only `order / role / size / type` cross to the client, so threshold and budget tuning need no client release.

---

## 5. Build Order

0. **Deploy spec-18 backend** (prerequisite — streak persistence).
1. **A1 — You strip** + `user_game_stats_get` (and `Draws`/`RecentForm`/`LastPlayed`). → Delivers the survey ask; no curation risk.
2. **B1 — `games_digest` + notability gate + budget.** → Fixes feed flooding immediately, even before the rich card.
3. **B2 — rich notable session card** (mockup, CS2 stat set).
4. **A2 — deep profile view.**
5. Later — more adapters (League next), richer per-game stat capture (spec 18), crew-configurable threshold.

Steps 1→2 deliver ~80% of the value (personal streaks + a clean feed) before the heavier rich-card/profile work.

---

## 6. Testing

- **Pure curation (Go, no Nakama):** `gameSessionQuality` ranking; threshold steps across simulated low/med/high volume; `buildThisWeek` caps notable cards at N and always emits exactly one `games_digest`; routine sessions never produce individual cards.
- **Stats (Go):** `user_game_stats_get` returns only the caller's data, sorted by `last_played`; `RecentForm` capped and ordered; draws counted without moving the streak.
- **Client:** You strip renders from `user_game_stats_get`, empty state when no games; profile per-game breakdown.
- **Manual:** with the spec-18 emulator, drive several sessions across members and confirm the feed shows a digest + ≤N notable cards (not a wall of session cards), and the You strip reflects the viewer's own streak.

---

## 7. Out of Scope / Future

- Per-match streaks (this spec keeps spec 18's per-session granularity).
- External stat sources for CS2 ADR/HS%/rating (spec 18 future).
- Crew-configurable curation ("show everything" toggle).
- Cross-game "career" profile and seasonal resets.

---

*This spec covers surfacing: the personal stats lane (You strip + profile) and crew feed curation (notability gate, adaptive threshold, games digest). The outcome/streak data it consumes is produced by [18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md). For the feed/ledger mechanics it builds on, see [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md).*
