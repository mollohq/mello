# MELLO Game UI Surfaces Specification

> **Component:** Control Bar Game States, Crew Sidebar Game List, Member Playing Line, Game Feed Cards, Game Icons
> **Version:** 0.1
> **Status:** Implemented — every surface below ships, except §3.4 (`recent_games`) and §7.2, which are marked as not built
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [17-GAME-SENSING.md](./17-GAME-SENSING.md), [18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md), [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md), [19-FEED-CURATION-PERSONAL-STATS.md](./19-FEED-CURATION-PERSONAL-STATS.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md)

---

## 1. Overview

This spec owns **how game activity is drawn**. It is the presentation layer for
the game pipeline, and it deliberately contains no detection logic and no
curation logic.

```
  SENSE                      RECORD                  CURATE          PRESENT
  ┌──────────────────┐      ┌─────────────────┐    ┌───────────┐   ┌──────────┐
  │ 17 Game Sensing  │      │ 16 Crew Event   │    │ 19 Feed   │   │ 20 Game  │
  │    which game,   │─────▶│    Ledger       │───▶│  Curation │──▶│    UI    │
  │    how long      │      │    game_session │    │  which    │   │  how it  │
  │                  │      │                 │    │  cards    │   │  looks   │
  │ 18 Telemetry     │      │    user_game_   │    │  survive  │   │          │
  │    W/L, streaks  │─────▶│    stats        │───▶│           │   │          │
  └──────────────────┘      └─────────────────┘    └───────────┘   └──────────┘
```

The boundary that matters: **19 decides which cards exist, 20 decides what they
look like.** If a question is "should this session be shown at all", it is spec
19. If it is "what does the card render when there is no win/loss record", it is
this spec.

### Why this spec exists

Specs 17 and 18 accumulated UI sections because they predated spec 19. Spec 17
§6/§7/§9 and spec 18's surfacing table were all marked "pending relocation" for
several versions. This spec is that relocation, so a reader looking for a game
surface has exactly one place to look.

---

## 2. Control Bar Game States

`client/ui/panels/control_bar.slint`. The centre region of the bottom bar has
three states driven by the game state manager ([17](./17-GAME-SENSING.md) §4).

### 2.1 Now Playing

```
[game icon]  NOW PLAYING          [STREAM]
             Counter-Strike 2
```

The icon is the **runtime-extracted executable icon** (§6), not a bundled asset.
`STREAM` appears only when the user has streaming capability (hardware encoder
detected).

### 2.2 Post-Game

When the game exits and the session reached `MIN_SESSION_POSTGAME_MIN`, the
centre content morphs:

```
[game icon]  How'd it go?   [trophy] [skull] [star]
```

- Trophy — posts a `moment` with sentiment `win`
- Skull — posts a `moment` with sentiment `loss`
- Star — opens text input, posts a `moment` with sentiment `highlight`
- 30-second timeout — dismiss, and log `game_session_end` only

When a telemetry adapter produced a decisive session, this card is pre-filled
(`CS2 · 5W–3L · +2 streak`) with one-tap confirm instead of a blank prompt. The
manual tap stays as the fallback for every game without an adapter. See
[18](./18-GAME-TELEMETRY.md).

### 2.3 Idle

```
[avatar]  Navigator    [voice controls]
          #001
```

### 2.4 Session thresholds

Two distinct thresholds exist in `mello-core/src/game_state.rs`, and confusing
them causes sessions to vanish:

| Constant | Value | Meaning |
|---|---|---|
| `MIN_SESSION_LEDGER_MIN` | 2 | Below this, no `game_session` event is recorded at all |
| `MIN_SESSION_POSTGAME_MIN` | 5 | Below this, the session is recorded but the post-game prompt is skipped |

A 3-minute session therefore reaches the ledger and the feed, but never asks
"how'd it go?". The invariant `MIN_SESSION_POSTGAME_MIN > MIN_SESSION_LEDGER_MIN`
is asserted at compile time.

### 2.5 State transitions

```
Idle ──[game detected]──▶ Now Playing
                              │
                        [exits, >= 5 min]
                              │
                              ▼
                         Post-Game ──[tap or 30s timeout]──▶ Idle

Now Playing ──[exits, < 5 min]──▶ Idle (skip post-game)
```

---

## 3. Crew Sidebar Game List

`client/ui/panels/crew_panel.slint`.

### 3.1 Data sources

| Source | Data | Purpose |
|---|---|---|
| Crew state `active_games` (live) | Who is playing what right now | Green dots, player count, live indicator |
| Ledger `game_session` events (persistent) | Who played what in the last 7 days | Entries survive when nobody is online |

`active_games` is computed server-side in `crew_state.go` from published game
presence, and reaches the client as `crew_state.active_games`.

### 3.2 Item rendering

```
┌──────────────────────────────────────────────────┐
│  [CS]  Counter-Strike 2                     3    │
│        ●● ●                                      │
└──────────────────────────────────────────────────┘
```

- Game badge — `short_name` on the game's colour, or the runtime icon when available
- Full game name
- Player dots — green for live, grey for recent-only
- Count — total unique across live and recent

An entry with only recent players and nobody live renders dimmed.

### 3.3 Per-member playing line

Under each crewmate in the member list:

```
▶ Valorant · 1h 23m
```

Built by `format_game_line` / `game_lines_from_members` in
`client/src/converters.rs` from `presence.game`, with the elapsed time computed
from `started_at` rather than sent pre-formatted. The line refreshes on presence
and voice events, so the minute figure advances on the presence heartbeat
without a dedicated timer.

Empty when the member is not playing, or when they have activity sharing off
(§5).

### 3.4 Recent games data — NOT BUILT

Earlier drafts proposed either extending the `crew_catchup` response with a
`recent_games` array, or adding a `crew_recent_games` RPC aggregating
`game_session` events over the 7-day window.

**Neither shipped.** No `recent_games` field and no such RPC exist. The sidebar
list is currently live-presence only. This is recorded here so the gap is
visible: it is unbuilt scope, not a defect, and no bug report will surface it.

---

## 4. Game Feed Cards

`client/ui/panels/crew_feed.slint`. Which cards reach the feed is spec
[19](./19-FEED-CURATION-PERSONAL-STATS.md); this section is what they render.

### 4.1 `GameSessionCard`

The session card has two bodies, chosen by whether telemetry produced a record:

| Component | Used when | Renders |
|---|---|---|
| `GameSessionRecordPanel` | `wins + losses + draws > 0` | W/L block, streak, duration, co-players |
| `GameSessionCompactBody` | no record | icon, "ostkatt played Valorant", duration footer, co-player avatars |

The compact body exists because **an empty stat slot must never render**
(spec 19 §3.5). Before this split, the card assumed a record existed and drew
blank W/L boxes for the overwhelming majority of games, which have no adapter.

### 4.2 `GameRollupCard`

One `CREW PLAY` card per crew per day aggregating routine play that curation
pruned: per-member top lines (`ostkatt · Valorant · 4h`), session count, and
total crew hours. `GameRollupLineRow` renders each member line.

This is what keeps a loud crew's feed readable once the notability gate opens to
telemetry-less sessions.

### 4.3 Duration copy — the overnight guard

**Play time shown to a crew must never be a number the crew can disprove.** A
game left open overnight has honest wall time and near-zero active time, so
every surface that prints a duration applies the same rule: use wall time,
unless active time is under a third of it, in which case use active time.

The rule has three implementations and they must stay in step:

| Where | Function | File |
|---|---|---|
| Per session | `game_session_duration_display` | `client/src/handlers/clip.rs` |
| Per week | `weekly_time_text` | `client/src/handlers/stats.rs` |
| Rollup aggregation | `reportableMinutes` | `backend/nakama/data/modules/crew_feed.go` |

The rollup one was added last. Before it, a 10h overnight session rendered "40m"
on its own card and contributed 10h to the `CREW PLAY` card directly above it.

### 4.4 Co-play copy

Co-play duration copy — "you and kim played 2h of CS2" — **may only be built
from `player_overlap_min`**, never from the actor's `duration_min`. The actor's
duration says nothing about how long anyone else was present.

**No surface renders this copy yet.** The data is recorded
([16](./16-CREW-EVENT-LEDGER.md) §2), and the rule is stated here so that
whoever writes the copy does not reach for the convenient wrong field.

---

## 5. Activity Sharing Consent

Onboarding gains a checkbox at the nickname step: **"Show crew what I'm
playing"**, default **on**, flippable later in Settings → Games.

The flag gates every surface in this spec:

| Layer | Gate |
|---|---|
| Presence publish | `game_presence_to_publish(share, current)` — `mello-core/src/client/game_services.rs` |
| Session sharing | `should_emit_game_session_end(share)` — same file |
| Persistence | `settings.share_game_activity`, `default_share_game_activity() == true` |
| Runtime flip | `Command::SetShareGameActivity` |

Turning it off mid-session clears published presence on the next scan, because
`sync_game_presence` recomputes the value and sees the change. Per-game hiding
is out of scope.

---

## 6. Game Icons

Icons are **extracted from the running executable at runtime**, not bundled. A
bundled asset set cannot cover a catalogue of 137k games.

| Platform | Mechanism |
|---|---|
| Windows | `SHDefExtractIconW` — `client/src/platform/exe_icon.rs` |
| macOS | ICNS chunk parsing — **written, never executed on a Mac** |

Icons are 256px. The backend cap is `gameIconMaxBytes = 256 * 1024` in
`backend/nakama/data/modules/game_icons.go`; a 256px icon runs about 84KB, and
an earlier 48KB cap silently rejected them. Keep `exe_icon.rs` and that constant
in step — the comment in `exe_icon.rs` says so explicitly.

The runtime icon reaches the UI as `game-runtime-icon` / `game-has-runtime-icon`
through `control_bar.slint` and `main.slint`. When extraction fails, surfaces
fall back to the `short_name` badge on the game's colour.

---

## 7. Slint Component Reference

| Component | File | Purpose |
|---|---|---|
| `GameSessionCard` | `crew_feed.slint` | Session card shell |
| `GameSessionRecordPanel` | `crew_feed.slint` | W/L body |
| `GameSessionCompactBody` | `crew_feed.slint` | Telemetry-free body |
| `GameRollupCard` | `crew_feed.slint` | `CREW PLAY` daily rollup |
| `GameRollupLineRow` | `crew_feed.slint` | One member line inside the rollup |
| `PlayingBadge` | `crew_feed.slint` | Live "playing" marker |

Types are declared in `client/ui/types.slint` (`game-line` on both member row
structs).

### 7.1 HUD overlay — NOT BUILT

An optional live round/score line during a competitive match was proposed for
`client/src/hud_manager.rs` / `HudState`. It has not been built. Recorded for
the same reason as §3.4.

---

## 8. Testing

Per `TESTING.md`:

- **UI behavior** uses the headless harness in `client/src/testkit.rs`, which
  drives the real `callbacks::wire_all` and `handlers::handle_event`. Inject an
  `Event` and assert on `MainWindow`; invoke a callback and assert on emitted
  `Command`s.
- **Assert structurally, not via accessibility.** These panels declare almost no
  `accessible-role`, so `accessible_enabled()` reads as absent even on a healthy
  screen. Query component type names instead.
- Duration copy is covered by pure unit tests on the three functions in §4.3 —
  including the overnight case, which is the one that erodes trust when wrong.

Not covered by any automated test: the surfaces in this spec have never run
against a live backend with real crew traffic.

---

## 9. Out of Scope

- Deep profile view (spec 19 §2.4)
- Per-game activity hiding — only the global toggle in §5 exists
- Rich telemetry-driven session card beyond the W/L panel in §4.1
- The two unbuilt items recorded inline: `recent_games` (§3.4) and the HUD
  overlay (§7.1)
