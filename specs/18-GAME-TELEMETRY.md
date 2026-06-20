# MELLO Game Telemetry Specification

> **Component:** Game Telemetry Adapters, Match Outcomes, Streak Stats
> **Version:** 0.1
> **Status:** Planned
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md), [17-GAME-SENSING.md](./17-GAME-SENSING.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md), [02-MELLO-CORE.md](./02-MELLO-CORE.md)

---

## 1. Overview

Game **sensing** (spec 17) detects *which* game is running at the process level. Game **telemetry** (this spec) detects *what happens inside the game* — match outcomes, scores, and the win/loss streaks that drive a richer post-game experience.

The motivating signal: survey respondents ask for things like *"an overview of my win/loss streaks in CS."* Process detection can't produce that. This spec adds a pluggable per-game telemetry layer, with **Counter-Strike 2 Game State Integration (GSI)** as the first concrete adapter, plus the data model and crew-first surfacing for outcomes and streaks.

```
┌─────────────────────────────────────────────────────────────────────┐
│                         GAME TELEMETRY                              │
│                                                                     │
│  CS2 ──GSI POST──▶ ┌────────────┐    ┌──────────────────────────┐   │
│  (game)            │  Telemetry │───▶│  GameStateManager        │   │
│                    │  Listener  │    │  (accumulates a session) │   │
│  ┌──────────────┐  │ (tiny_http)│    └────────────┬─────────────┘   │
│  │ CS2 GSI      │─▶│            │                 │ SessionSummary   │
│  │ Adapter      │  └────────────┘                 ▼                  │
│  └──────────────┘                    ┌──────────────────────────┐   │
│         ▲ ensure cfg installed       │ game_session_end RPC     │   │
│         │ (winreg + libraryfolders)  └────────────┬─────────────┘   │
│  GameSensor (spec 17) ──Started/Stopped──┐        │                 │
│                                          ▼        ▼                 │
│                          ┌───────────────────────────────────────┐ │
│                          │ Backend: user_game_stats (private)     │ │
│                          │   ─ derive streak ─▶ game_session event │ │
│                          │   (public, crew-first surfacing)        │ │
│                          └───────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────────┘
```

### Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Outcome source | Official per-game integrations only | No memory reading / injection / ToS risk |
| First adapter | CS2 GSI | Valve-supported config-file integration; survey names CS |
| Adapter model | Pluggable trait; missing adapter = no telemetry | Most games degrade to spec 17's manual win/loss tap |
| Local transport | `tiny_http` loopback listener on `127.0.0.1:29406` | Reuses the OAuth listener pattern; no new dependency |
| Spoof protection | Per-install auth token embedded in the cfg, verified on each POST | Prevents other local apps from injecting fake results |
| Stat depth | Outcomes + streaks (W/L/draw, win-rate, current/longest streak) | Matches the survey ask; keeps storage lean |
| Streak modes | competitive / premier / wingman count; casual / DM = "played only" | Streaks should mean ranked-ish play (tunable) |
| Crash handling | A match with no `gameover` is `Incomplete`, never a loss | Don't punish streaks for disconnects/crashes |
| Visibility | Crew-first: streaks surface in feed/catch-up/recap | Raw history stays private; only the derived streak number is shared |
| Platform | Windows-first (matches spec 17) | GSI cfg path resolution is OS-specific |

### What Changes In Other Specs

- **Spec 17 amendment:** telemetry is a layer *above* the process sensor. `GameSensor` keeps emitting `Started`/`Stopped`. When a game with a registered adapter starts, the adapter's cfg is installed and the listener begins routing that game's POSTs.
- **Spec 16 amendment:** the `game_session` event's `data` is enriched with `wins`, `losses`, `result`, and `streak_after` (additive, backward compatible). No new event type.

---

## 2. Telemetry Adapter Framework (mello-core)

New module `mello-core/src/telemetry/`.

### 2.1 Adapter Trait

```rust
// telemetry/mod.rs

/// A per-game integration that turns local game state into outcome events.
pub trait GameTelemetryAdapter: Send + Sync {
    /// Game DB id this adapter serves (e.g. "counter-strike-2").
    fn game_id(&self) -> &str;

    /// Install/refresh whatever the game needs to emit telemetry (idempotent).
    /// Called lazily when the game is first detected.
    fn ensure_installed(&self, token: &str, port: u16) -> Result<(), TelemetryError>;

    /// Parse one inbound payload into telemetry events. `token` is the expected
    /// per-install auth token; payloads that don't carry it are rejected (`None`).
    fn parse(&self, body: &str, token: &str) -> Vec<TelemetryEvent>;
}

#[derive(Debug, Clone)]
pub enum TelemetryEvent {
    MatchStarted { mode: String, map: String },
    RoundEnded { ct_score: u32, t_score: u32 },
    MatchEnded(MatchResult),
}

#[derive(Debug, Clone)]
pub struct MatchResult {
    pub game_id: String,
    pub mode: String,
    pub map: String,
    pub result: Outcome,
    pub rounds_won: u32,
    pub rounds_lost: u32,
    pub ts: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome { Win, Loss, Draw, Incomplete }

impl Outcome {
    /// Only decisive ranked-ish results affect streaks.
    pub fn counts_toward_streak(&self) -> bool {
        matches!(self, Outcome::Win | Outcome::Loss)
    }
}
```

Adapters are registered in a small registry keyed by `game_id`. A game with no registered adapter contributes no telemetry; the post-game flow falls back to the manual win/loss tap from spec 17.

### 2.2 Listener

`telemetry/listener.rs` runs a long-lived `tiny_http::Server` bound to `127.0.0.1:29406` on a dedicated thread, mirroring `GameSensor::start` (spec 17) and the OAuth server pattern (`oauth.rs`). For each request it reads the POST body (`request.as_reader()`), dispatches to the adapter for the **currently active** game, and forwards resulting `TelemetryEvent`s over an `mpsc::Sender<TelemetryEvent>` into the client event loop.

```rust
pub const TELEMETRY_PORT: u16 = 29406;

pub struct TelemetryListener { _handle: Option<std::thread::JoinHandle<()>> }

impl TelemetryListener {
    pub fn start(
        adapters: Arc<AdapterRegistry>,
        active_game: Arc<Mutex<Option<String>>>, // game_id of the focused game
        token: String,
    ) -> (Self, mpsc::Receiver<TelemetryEvent>) { /* ... */ }
}
```

- The **auth token** is generated once per install (reuse the `rand` alphanumeric pattern from `oauth.rs`) and persisted in config. It is written into each adapter's cfg and required on every inbound payload.
- The listener always replies `200 OK` quickly (GSI retries/back-pressures otherwise).
- Binding failure is non-fatal: telemetry is best-effort; sensing and the manual flow continue.

### 2.3 Wiring (client/mod.rs)

Alongside the existing `GameSensor`:

1. On startup, build the `AdapterRegistry` and start `TelemetryListener`.
2. On `GameEvent::Started(game)`: set `active_game = Some(game.game_id)`; if an adapter exists, call `ensure_installed(token, TELEMETRY_PORT)` (log + continue on error).
3. On `GameEvent::Stopped`: clear `active_game`; any in-flight match is finalized as `Incomplete`.
4. Drain the telemetry receiver in the event loop (next to the game-event drain) and feed `TelemetryEvent`s to `GameStateManager`.

---

## 3. CS2 GSI Adapter

`telemetry/cs2_gsi.rs`.

### 3.1 Config Installation

```rust
fn ensure_installed(&self, token: &str, port: u16) -> Result<(), TelemetryError> {
    // 1. Steam root: winreg HKCU\Software\Valve\Steam\SteamPath
    // 2. Parse <steam>/steamapps/libraryfolders.vdf for the library holding app 730
    // 3. cfg dir: <library>/steamapps/common/Counter-Strike Global Offensive/game/csgo/cfg
    // 4. Write gamestate_integration_mello.cfg if missing or token/port changed (idempotent)
}
```

The cfg subscribes to the minimal data we need:

```
"Mello Game State Integration v1"
{
    "uri" "http://127.0.0.1:29406"
    "timeout" "5.0"
    "auth" { "token" "<per-install-token>" }
    "data"
    {
        "provider"      "1"
        "map"           "1"
        "round"         "1"
        "player_id"     "1"
        "player_state"  "1"
        "player_match_stats" "1"
    }
}
```

`winreg` is already a client dependency; if installation logic lives in mello-core, it is added there too (already vendored in the workspace lockfile — a location move, not a new dependency).

### 3.2 Outcome Derivation

GSI posts the full state on each change. The adapter tracks a tiny state machine keyed on `map.phase`:

| Transition | Action |
|-----------|--------|
| `→ warmup`/`live` after a previous `gameover` (or first seen) | `MatchStarted { mode, map }`; reset round tracking |
| `round.phase` resolves | `RoundEnded { ct_score, t_score }` (live HUD / future auto-clip hook) |
| `→ gameover` | `MatchEnded` — derive `Outcome` |

`Outcome` at `gameover`:
- Read the player's current side from `player.team` (`"CT"`/`"T"`) — GSI reports the live side, so halftime side-switches are handled by reading at gameover.
- `own = (player.team == CT) ? map.team_ct.score : map.team_t.score`; `opp = the other`.
- `own > opp → Win`, `own < opp → Loss`, `own == opp → Draw`.
- `rounds_won = own`, `rounds_lost = opp`.

`mode` from `map.mode` (`competitive`, `premier`, `wingman`, `casual`, `deathmatch`, …). Only `competitive`/`premier`/`wingman` set a streak-affecting `Outcome`; other modes still emit `MatchEnded` with the result for the session summary but `counts_toward_streak()` is gated on `Win`/`Loss` from those modes (casual results are recorded as played, not streaked).

**Robustness:** all GSI fields are optional in serde structs (`#[serde(default)]`); a malformed or partial payload yields no events rather than an error. The token check happens before parsing.

---

## 4. Session & Outcome Model (mello-core)

### 4.1 GameStateManager

`game_state.rs` is extended to accumulate a session:

```rust
pub struct GameStateManager {
    current_game: Option<ActiveGame>,
    session_start: Option<i64>,
    matches: Vec<MatchResult>,   // accumulated this session
}
```

- `handle_telemetry(TelemetryEvent)` pushes `MatchEnded` results into `matches` and emits a live `Event::MatchEnded { result, ct_score, t_score, map }`.
- On `GameEvent::Stopped`, build:

```rust
pub struct SessionSummary {
    pub game_name: String,
    pub duration_min: u32,
    pub matches: Vec<MatchResult>,
    pub wins: u32,    // decisive, streak-eligible
    pub losses: u32,
}
```

`GameSessionEndInfo` (spec 17 §4) is replaced by `SessionSummary` (still carrying `duration_min`; the 2-min ledger / 5-min post-game thresholds are unchanged).

### 4.2 New UI Events (events.rs)

```rust
Event::MatchEnded { result: String, ct_score: u32, t_score: u32, map: String }       // live, during play
Event::SessionSummary { game_name: String, duration_min: u32, wins: u32, losses: u32, streak_after: i32 }
```

`streak_after` is filled from the `game_session_end` RPC response (see §5.3).

---

## 5. Persistence (backend)

### 5.1 Enriched `game_session` Event (spec 16 amendment)

`GameSessionData` gains additive fields:

```go
type GameSessionData struct {
    GameName    string   `json:"game_name"`
    GameIGDBID  int      `json:"game_igdb_id"`
    PlayerIDs   []string `json:"player_ids"`
    PlayerNames []string `json:"player_names"`
    DurationMin int      `json:"duration_min"`
    Wins        int      `json:"wins,omitempty"`         // NEW
    Losses      int      `json:"losses,omitempty"`       // NEW
    Result      string   `json:"result,omitempty"`       // NEW: "win" | "loss" | "even" | ""
    StreakAfter int      `json:"streak_after,omitempty"` // NEW: signed; +N win streak, -N loss streak
}
```

Catch-up `score` for `game_session` is raised from `10` toward moment-level when there is a decisive record, so heaters surface in the catch-up card.

### 5.2 Per-User Stats Store (`user_game_stats`)

A durable, **owner-read / server-write** store mirroring the `crew_clips`/`crew_recaps` pattern (system writes, optimistic-concurrency retry):

| Field | Value |
|-------|-------|
| Collection | `user_game_stats` |
| Key | `{game_id}` |
| UserID | `{user_id}` (user-owned) |
| PermissionRead | `1` (owner only) |
| PermissionWrite | `0` (server only) |

```go
type UserGameStats struct {
    GameID          string `json:"game_id"`
    Wins            int    `json:"wins"`
    Losses          int    `json:"losses"`
    CurrentStreak   int    `json:"current_streak"`    // signed: + wins, - losses
    LongestWinStreak  int  `json:"longest_win_streak"`
    LongestLossStreak int  `json:"longest_loss_streak"`
    LastResult      string `json:"last_result"`
    UpdatedAt       int64  `json:"updated_at"`
}
```

The 7-day ledger cannot hold longest-streak history, so this store is the source of truth for streaks/win-rate. It is private to the user.

### 5.3 GameSessionEndRPC (enriched)

Request gains optional fields (backward compatible):

```go
type GameSessionEndRequest struct {
    CrewID      string `json:"crew_id"`
    GameName    string `json:"game_name"`
    GameID      string `json:"game_id"`     // NEW: stable id for the stats key
    DurationMin int    `json:"duration_min"`
    Wins        int    `json:"wins"`        // NEW
    Losses      int    `json:"losses"`      // NEW
}
```

Flow:
1. Validate membership (unchanged).
2. **Update `user_game_stats/{game_id}`** for the actor: apply `Wins`/`Losses` to totals; recompute `current_streak` (a net winning session extends/flips a win streak, a net losing session a loss streak; an even session leaves the streak unchanged); update `longest_*`. Derive `result` ("win"/"loss"/"even").
3. **Privacy bridge:** copy only the resulting `current_streak` (`StreakAfter`) and the `Wins`/`Losses`/`Result` of *this session* into the public `game_session` ledger event. Raw history stays in the owner-only store.
4. Append the enriched event; return `{ success, streak_after }`.

> Streak update granularity is **per session**, not per match — a night nets to one win/loss/even outcome for streak purposes, which matches how the survey framed "win/loss streaks." (Per-match streaks remain a future option; the stored `matches` make it possible.)

---

## 6. Crew-First Surfacing

| Surface | Change | File |
|---------|--------|------|
| Bottom-bar post-game | When telemetry produced a decisive session, pre-fill `CS2 · 5W–3L · +2 streak` with one-tap confirm/share instead of blank "How'd it go?"; manual tap remains the fallback | `client/src/handlers/game.rs`, `client/ui/.../post_game.slint` |
| Catch-up card | Streak-aware `game_session` fragment, e.g. *"ash closed the night 5W–2L in CS2, riding a 4-win streak"* | `crew_events.go` `renderEventFragment` |
| Crew feed | Render W–L + streak on the `session` card | `crew_feed.go`, `client/ui/.../crew_feed.slint`, `handlers/clip.rs` |
| Crew sidebar | Optional small record/streak badge on live game entries | `sidebar_game.slint` (spec 17 §6) |
| Weekly recap | Per-member W/L record + best streak of the week | `crew_recaps.go` `WeeklyRecapData` |
| HUD overlay | Optional live round/score line during a competitive match | `client/src/hud_manager.rs`, `HudState` |

A standalone personal stats page is out of scope (visibility is crew-first), but `user_game_stats` makes it a clean future addition.

---

## 7. File Structure

### 7.1 New Files

```
mello-core/src/telemetry/
├── mod.rs          # GameTelemetryAdapter trait, TelemetryEvent, MatchResult, Outcome, registry
├── listener.rs     # tiny_http loopback listener, token auth, mpsc to client loop
└── cs2_gsi.rs      # CS2 GSI adapter: cfg install + outcome derivation

backend/nakama/data/modules/
└── user_game_stats.go   # UserGameStats store + update helper (mirrors clips/recaps)

tools/gsi-emulator/       # dev-only: POST a recorded GSI match sequence to :29406
```

### 7.2 Modified Files

| File | Change |
|------|--------|
| `mello-core/src/game_state.rs` | Accumulate matches → `SessionSummary`; `handle_telemetry` |
| `mello-core/src/events.rs` | `Event::MatchEnded`, `Event::SessionSummary` |
| `mello-core/src/crew_events.rs` | Enrich `GameSessionEndRequest` (game_id/wins/losses) + response streak |
| `mello-core/src/client/mod.rs` | Start listener; wire adapters; drain telemetry; richer RPC call |
| `mello-core/src/config.rs` | Persist the per-install telemetry auth token |
| `backend/.../crew_events.go` | Enrich `GameSessionData`; update stats + privacy bridge in `GameSessionEndRPC`; streak catch-up fragment |
| `backend/.../crew_recaps.go` | Per-member W/L + best streak |
| `backend/.../crew_feed.go` | Surface record/streak on session cards |
| `backend/.../main.go` | (No new RPC; `user_game_stats` is written inside `GameSessionEndRPC`) |
| `client/src/handlers/game.rs`, UI panels | Pre-filled post-game; feed/sidebar/HUD record display |

---

## 8. Testing

### 8.1 Unit (Rust, next to code)
- `cs2_gsi::parse`: win/loss/draw at `gameover`; halftime side-switch; mode gating; new-match transition; partial/garbage payload → no events; wrong/missing token → rejected.
- `GameStateManager`: multi-match accumulation → correct `wins`/`losses` and `SessionSummary`; `Incomplete` never counts; thresholds preserved.

### 8.2 Backend (Go, local Docker stack)
- `GameSessionEndRPC`: streak increments on a winning session, flips/resets on a losing one, `longest_*` monotonic; `StreakAfter` mirrored into the public event; private store not world-readable.

### 8.3 Manual (end-to-end)
- `tools/gsi-emulator` POSTs a recorded competitive match sequence to `127.0.0.1:29406` (no live CS2 needed). Verify: cfg auto-installs on first CS2 detection → NOW PLAYING → live round events → post-game card pre-filled with W–L + streak → crew feed card + catch-up text reflect the record. Confirm against real CS2 when available.
- `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` clean before done.

---

## 9. Future Extensions (Not In Scope)
- **Outcome-driven auto-clips:** use `RoundEnded`/ace/clutch signals to auto-mark highlights (blocked on video clip capture, spec 14 — only audio clips exist today).
- **More adapters:** League Live Client Data API, Dota 2 GSI, Valorant (post-match Riot API only; no legit live feed).
- **Per-match streaks & full stat pages:** K/D/A/ADR/HS%, rank/MMR, a personal profile surface built on `user_game_stats` + stored `matches`.
- **Rank tracking:** CS2 Premier rating deltas per session.

---

*This spec covers the telemetry adapter framework, the CS2 GSI adapter, the session/outcome model, the per-user streak store, and crew-first surfacing. For process-level detection and the game DB, see [17-GAME-SENSING.md](./17-GAME-SENSING.md). For the event ledger and post-game moments, see [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md).*
