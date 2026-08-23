# MELLO Game Telemetry Specification

> **Component:** Game Telemetry Adapters, Match Outcomes, Streak Stats
> **Version:** 0.3
> **Status:** Implemented — nine adapters shipped (CS2, Dota 2, LoL, Rocket
> League, LoR, Hearthstone, Minecraft, Path of Exile, StarCraft II)
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md), [17-GAME-SENSING.md](./17-GAME-SENSING.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md), [02-MELLO-CORE.md](./02-MELLO-CORE.md), [19-FEED-CURATION-PERSONAL-STATS.md](./19-FEED-CURATION-PERSONAL-STATS.md), [22-GAME-UI-SURFACES.md](./22-GAME-UI-SURFACES.md)
>
> **Scope.** This spec covers *producing* outcome and streak data: adapters,
> the normalized outcome model, and persistence. It does not cover how any of
> it is drawn. Which sessions reach a feed is
> [19-FEED-CURATION-PERSONAL-STATS.md](./19-FEED-CURATION-PERSONAL-STATS.md);
> what the surfaces look like is
> [22-GAME-UI-SURFACES.md](./22-GAME-UI-SURFACES.md).
>
> **Coverage.** Nine adapters cover a catalogue of thousands of games. Most
> sessions therefore carry no W/L record. Every surface must treat outcome data
> as absent by default. See spec 19 section 3.6.

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

### 2.1 Adapter Trait (v0.2 — generalized source model)

Adapters come in five **source classes**, all sharing one trait and one event
channel into the client loop:

| Source class | Transport | Examples |
|--------------|-----------|----------|
| Local push listener | game POSTs to Mello's loopback listener | CS2 GSI, Dota 2 GSI |
| Local poll/subscribe | Mello connects to a game-hosted endpoint | LoL Live Client (HTTPS poll), Rocket League Stats API (websocket), SC2 `:6119`, LoR |
| Local websocket server | game connects to Mello | Apex LiveAPI |
| Log/file tailer | Mello tails a game-written log | Hearthstone `Power.log`, MTG Arena, PoE `Client.txt`, WoW combat log |
| Run/replay importer | parse files after match/run end | Slay the Spire `.run`, SC2 replays |

```rust
// telemetry/mod.rs

/// A per-game integration that turns local game state into outcome events.
pub trait GameTelemetryAdapter: Send + Sync {
    /// Game DB id this adapter serves (e.g. "counter-strike-2").
    fn game_id(&self) -> &str;

    /// Install/refresh whatever the game needs to emit telemetry (idempotent).
    /// Called eagerly at client startup and again on detection.
    fn ensure_installed(&self, token: &str, port: u16) -> Result<(), TelemetryError>;

    /// Push sources: parse one inbound loopback payload into events.
    /// ROUTING CONTRACT: the listener offers payloads to *every* adapter, so
    /// each must positively identify its own game (e.g. provider appid) and
    /// yield nothing otherwise. Default: not a push source.
    fn parse(&self, body: &str, token: &str) -> Vec<TelemetryEvent> { Vec::new() }

    /// Active sources: spawn the adapter-owned transport (poller / websocket /
    /// tail) on game detection; send events into `tx` until `reset`.
    /// Default: no-op for pure push sources.
    fn start(&self, tx: mpsc::Sender<TelemetryEvent>) {}

    /// Stop any transport + clear cross-payload state on game exit.
    fn reset(&self) {}
}

#[derive(Debug, Clone)]
pub enum TelemetryEvent {
    MatchStarted { mode: String, map: String },
    /// Live score change, player perspective (HUD / auto-clip hooks).
    ScoreChanged { own: u32, opp: u32 },
    MatchEnded(MatchResult),
}
```

### 2.1a Outcome Model — normalized stat slots

`MatchResult` carries a universal scoreline plus **optional stat-slot groups**
filled per game. Surfaces (spec 19) render only the slots present — never
empty stat boxes.

```rust
pub struct MatchResult {
    pub game_id: String,
    pub mode: String,
    pub map: String,
    pub result: Outcome,
    /// Whether this result may move a streak (ranked-ish mode, per adapter).
    pub streak_eligible: bool,
    /// Player-perspective scoreline: rounds/goals/points won vs lost.
    pub own_score: u32,
    pub opp_score: u32,
    pub performance: Option<Performance>, // K/D/A, MVPs, damage/healing, goals/saves, CS
    pub build: Option<BuildInfo>,         // character/deck code/items/build order
    pub run: Option<RunInfo>,             // stage reached, difficulty, duration
    pub source: SourceQuality,            // Live | PostMatch | Replay | Manual
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

`SourceQuality` tells surfaces how confidently to render a result (a `Live`
"confirmed W" vs a `Manual` self-report). Session tallies count only
`streak_eligible` decisive results; other matches are recorded as played.

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
| `round.phase` resolves | `ScoreChanged { own, opp }` (live HUD / future auto-clip hook) |
| `→ gameover` | `MatchEnded` — derive `Outcome` |
| `map` block disappears while a match is active (abandon/disconnect to menu) | `MatchEnded` with `Outcome::Incomplete`; reset match state |

`Outcome` at `gameover`:
- Read the player's current side from `player.team` (`"CT"`/`"T"`) — GSI reports the live side, so halftime side-switches are handled by reading at gameover.
- `own = (player.team == CT) ? map.team_ct.score : map.team_t.score`; `opp = the other`.
- `own > opp → Win`, `own < opp → Loss`, `own == opp → Draw`.
- `own_score = own`, `opp_score = opp`; `player_match_stats` (K/D/A, MVPs, score) fills the `performance` slot when present.

`mode` from `map.mode` (`competitive`, `premier`, `wingman`, `casual`, `deathmatch`, …). Only `competitive`/`premier`/`wingman`/`scrimcomp2v2` are tracked; **non-streak modes emit no telemetry events at all** (v1 decision — casual/DM play stays "played only" at the process level, spec 17). Recording non-streak matches as played-not-streaked becomes possible once `MatchResult` carries explicit streak eligibility (generalized adapter model, §2) and is deferred until an adapter needs it.

**Robustness:** all GSI fields are optional in serde structs (`#[serde(default)]`); a malformed or partial payload yields no events rather than an error. The token check happens before parsing.

### 3.3 Dota 2 GSI Adapter

Same push mechanism, sharing the listener, token, and Steam-discovery helpers. Differences:

- **Config:** `…/dota 2 beta/game/dota/cfg/gamestate_integration/gamestate_integration_mello.cfg` (the subdir is created if missing). Subscribes to `provider`, `map`, `player`, `hero`.
- **Launch option:** GSI is dead without `-gamestateintegration`. We never edit Steam's `localconfig.vdf`; instead the adapter emits `TelemetryEvent::SetupRequired` on detection when the flag looks absent (read-only heuristic: any `userdata/*/config/localconfig.vdf` containing the flag counts as set). The client shows the hint under the "now playing" card (`Event::TelemetrySetupHint`).
- **State machine:** keyed on `map.game_state` (`DOTA_GAMERULES_STATE_*`). Active states (hero selection → in progress) start a match; `POST_GAME` ends it with the winner from `map.win_team` vs `player.team_name`; a disappearing `map` block finalizes as `Incomplete` (same as CS2). Custom games (`customgamename` set) emit nothing.
- **Stats:** kill score from `radiant_score`/`dire_score` (player-oriented) → `own/opp`; `player` K/D/A + last hits → `performance`; `hero.name` → `build.character`. All matchmaking results are `streak_eligible` (GSI doesn't distinguish ranked/unranked).

### 3.4 League of Legends Live Client Data Adapter

First **active** source: the game client hosts `https://127.0.0.1:2999/liveclientdata/allgamedata` during a match (self-signed Riot loopback cert — accepted for this client only; it never leaves 127.0.0.1). No install step.

- `start()` spawns a poll thread (3 s interval, sliced sleeps so `reset()` stops it promptly); request errors just mean "no match running" and are quiet.
- A snapshot without a `GameEnd` event while untracked → `MatchStarted { gameMode, mapName }`.
- The `GameEnd` event (`Result: "Win"|"Lose"`) → `MatchEnded`; own/opp = total champion kills per side oriented by the active player's team; the active player's scoreboard entry (matched by riot id) fills `performance` (K/D/A, CS) and `build.character` (champion). A guard flag prevents re-emitting from trailing post-game polls.
- `PRACTICETOOL`/`TUTORIAL` are recorded but not `streak_eligible`.

### 3.5 Rocket League Stats API Adapter

Active source over the official `MatchStatsExporter_TA` feature: `ensure_installed` drops `TAGame/Config/DefaultStatsAPI.ini` (never clobbering an existing user config), which makes the game host a local socket on `127.0.0.1:49123` that streams concatenated JSON envelopes during matches (Psyonix calls it a websocket; the wire format is a raw TCP JSON stream). Requires a game restart after first install — same eager-install-at-startup mitigation as CS2.

- `start()` spawns a connect-and-read thread with quiet 3 s reconnects; `reset()` stops it.
- `MatchCreated` primes fresh state; the first `UpdateState` tick emits `MatchStarted` and team goal changes emit `ScoreChanged`.
- The local side is inferred from the spectator `Target` of **non-replay** ticks (first-person play views the local car; goal replays may retarget the scorer and are ignored).
- `MatchEnded (WinnerTeamNum)` vs the inferred side → outcome; the target player's tick fills `performance` (goals/assists/saves/shots/score). `MatchDestroyed` or a socket drop mid-match → `Incomplete`.
- Playlist/MMR are not broadcast; matches with a `MatchGuid` (online/LAN) are `streak_eligible`, offline modes are not.

### 3.6 Legends of Runeterra Local API Adapter

Poll source: the LoR client hosts `http://127.0.0.1:21337` (`/game-result`, `/static-decklist`). A `GameID` increment on `/game-result` means a match just finished: `LocalPlayerWon` → outcome, the last seen `DeckCode` → `build.deck_code`. The API doesn't expose the queue, so all completed games are `streak_eligible` (v1 decision).

### 3.7 Hearthstone Power.log Adapter

Log tailer over the Blizzard-tolerated deck-tracker mechanism. `ensure_installed` writes the `[Power]` section into `%LOCALAPPDATA%\Blizzard\Hearthstone\log.config` (append-only, never clobbers). The tailer follows the newest `Logs/Hearthstone_*/Power.log`. Only `GameState.*` lines are read (`PowerTaskList.*` duplicates lag behind animations): `CREATE_GAME` → `MatchStarted`; the friendly side is the player whose `zone=HAND` entities carry a revealed card id; `tag=PLAYSTATE value=WON/TIED` → outcome (opponent's WON = our loss; unattributable → `Incomplete`). The log doesn't name the queue, so completed games are `streak_eligible`.

### 3.8 Minecraft World-Stats Adapter

File diff, singleplayer/LAN only: snapshot every `saves/<world>/stats/<uuid>.json` at game start, diff at exit (worlds save on quit, before `reset()` runs). The world with the biggest playtime delta becomes one played-only `MatchEnded` (`Incomplete`, never streaked): playtime → `run.duration_sec`, mob kills/deaths → `performance`, blocks mined/placed → `run.stage_reached`. Third-party servers keep stats server-side and contribute nothing.

### 3.9 Path of Exile Client.txt Adapter

Log tailer over the GGG-tolerated client log (Steam install; found via the shared Steam helpers). PoE is sessionful, so the adapter accumulates zones entered, level-ups (`build.character` = "Name (Class), level N"), and deaths, then flushes a single `MatchEnded` with the `run` slot on `reset()` — always `Incomplete`/non-streaked, feeding the "run stats" ask without touching records.

### 3.10 StarCraft II Client API Adapter

Poll source on the undocumented-but-stable `127.0.0.1:6119` (`/ui` + `/game`). Empty `activeScreens` = in a game; the transition back to menus ends it, reading each player's `result` from `/game`. Attribution limit: the API doesn't mark the local player, so vs-AI games (exactly one `user`-type player) get a real outcome (never streaked — it's AI) with race → `build.character`; ladder games with two `user` players are recorded played-only (`Incomplete`) until cross-game name tracking exists. Replays are ignored entirely.

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

### 5.4 Riot API proxy (`riot.go`) — server-verified LoL/TFT outcomes

The one place a web API beats local telemetry: Riot's post-match data can *verify* client-reported outcomes. The production API key lives server-side only (`RIOT_API_KEY` env var; never shipped in the client).

- **Opt-in link:** `riot_link { riot_id: "GameName#TAG", region }` resolves the Riot ID via account-v1 and stores `{riot_id, puuid, region}` in the owner-only `riot_account` collection. `riot_unlink` deletes it (stopping all Riot requests for the user); `riot_status` reports `{available, linked, riot_id, region}` for the settings UI. Client wrappers exist on `NakamaClient`.
- **Enrichment:** inside `GameSessionEndRPC`, sessions with `game_id == "league-of-legends"` from linked users fetch the player's own match ids since session start (match-v5, falling back to tft-match-v1 — TFT runs in the LoL client), tally win/loss per match (LoL: participant `win`, remakes <5 min skipped; TFT: top-4 = win), and **supersede the client-reported tally**, setting `verified: true` on the public `game_session` event. Strictly best-effort: any failure leaves the client numbers standing. Budget ≤4 requests per session end (2.5 s timeout each).
- **Scope guard:** only the linked user's own matches are ever requested — no lookups of other players, no public profiles.

---

## 6. Games Settings & Consent

> The surfacing table moved out of this spec. It listed the post-game card, the
> catch-up fragment, the feed card, the sidebar badge, the weekly recap and the
> HUD. Curation is now [19-FEED-CURATION-PERSONAL-STATS.md](./19-FEED-CURATION-PERSONAL-STATS.md).
> Appearance is now [22-GAME-UI-SURFACES.md](./22-GAME-UI-SURFACES.md).
>
> This section keeps adapter consent and account linking. Both belong to
> telemetry.

An earlier version listed a personal stats page as out of scope. That page is
Lane A of spec 19 and is implemented.

### 6.1 Adapter consent

Settings → **Games** lists every registered adapter (`AdapterInfo`: name, one-line
mechanism note, `writes_files` flag) with an install-detection badge
(`detect_install()`, Windows-only probe) and a per-game consent toggle.
Consent is **default-on**; disabled game ids persist in client settings
(`disabled_game_integrations`) and are pushed to the core
(`Command::SetGameIntegrations`, seeded before `run()`), where they skip both
`ensure_installed` (no files written into game folders) and active-transport
`start()`. Disabling never deletes previously installed config files.

The tab also hosts **Connected accounts → Riot Games**: connect opens the
Riot ID dialog (`riot_link_modal.slint`, Riot ID + routing region), backed by
`Command::RiotLink/RiotUnlink/LoadRiotStatus` → the `riot_*` RPCs (§5.4).

Proactive ask: when a session of a Riot-linkable game (currently
`league-of-legends`) ends with the post-game card showing and no link exists,
the card grows a one-time "GET VERIFIED STATS" CTA that opens the same dialog.
Dismissing it sets `riot_prompt_dismissed` and it is never shown again.

---

## 7. File Structure

### 7.1 New Files

```
mello-core/src/telemetry/
├── mod.rs              # GameTelemetryAdapter trait, TelemetryEvent, MatchResult, Outcome, registry
├── listener.rs         # tiny_http loopback listener, token auth, mpsc to client loop
├── steam.rs            # shared Steam library/path discovery (Windows)
├── log_tail.rs         # shared log tailer for file-based adapters
├── cs2_gsi.rs          # CS2 GSI adapter: cfg install + outcome derivation
├── dota2_gsi.rs        # Dota 2 GSI adapter (+ launch-option hint)
├── lol_live.rs         # LoL Live Client Data poller
├── rocket_league.rs    # Rocket League Stats API stream
├── lor_local.rs        # Legends of Runeterra local API poller
├── hearthstone_log.rs  # Hearthstone Power.log tailer
├── minecraft_stats.rs  # Minecraft world-stats diff
├── poe_log.rs          # Path of Exile Client.txt tailer
└── sc2_client.rs       # StarCraft II client API poller

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

## 9. Adapter Expansion Matrix

Priorities, source classes, and risk per game (research verified mid-2026;
detailed per-game behavior lands in small adapter specs when implementation
starts). Stat slots use the §2.1a groups.

| Priority | Game | Source class | Setup requirement | Stat slots | Risk |
|----------|------|--------------|-------------------|------------|------|
| shipped | CS2 | push (GSI) | cfg drop (auto) | scoreline, performance | low — official |
| shipped | Dota 2 | push (GSI) | cfg drop + `-gamestateintegration` launch option (user prompt) | scoreline, performance, build | low — official |
| shipped | League of Legends | poll (`https://127.0.0.1:2999/liveclientdata`) | none (self-signed cert pinned) | scoreline, performance, build | low — official, register product on dev portal |
| shipped | Rocket League | subscribe (ws `127.0.0.1:49123`) | ini drop (Stats API, Apr 2026) | scoreline, performance | low — official; no playlist/MMR |
| shipped | Legends of Runeterra | poll (`127.0.0.1:21337`) | none | scoreline, build (exact decklist) | low — official |
| shipped | Hearthstone | log tail (`Power.log`) | log.config drop (auto) | scoreline | low — publisher-tolerated |
| shipped | Minecraft | file diff (world stats JSON) | none (singleplayer/LAN only) | performance, run | low |
| shipped | Path of Exile 1/2 | log tail (`Client.txt`) | none | run, build | low — GGG-friendly |
| shipped | StarCraft 2 | poll (`:6119`) | none | scoreline (vs AI), build (race) | med — `:6119` undocumented; ladder attribution pending |
| 2 | MTG Arena | log tail (detailed logs) | detailed-logs toggle | scoreline, build | low — publisher-tolerated |
| 2 | Slay the Spire | run importer (`.run` files) | none | run, build | low |
| 2 | StarCraft 2 replays | replay import (`s2protocol`) | none | build (build order vs race) | med |
| 2 | WoW | log tail (combat log) | user enables `/combatlog` (or addon) | performance (dmg/heal, boss attempts) | med — heavy parser, Blizzard-sanctioned |
| spike | Apex Legends | ws server (LiveAPI) | launch options / config.json | scoreline, performance | med — verify pubs event coverage |
| presence-only | Roblox | log tail (experience detection) | none | (spec 17 presence enrichment; no outcomes) | med — undocumented log format, degrade to "Roblox" |
| gated | LoL/TFT match-v5 | web API via backend proxy (§5.4, built; awaits production key) | Riot production key (applied mid-2026) + user links Riot ID | scoreline (verified) | low — awaiting approval |
| gated | Valorant (RSO) | web API via backend proxy | Riot production key + RSO opt-in flow | scoreline, performance | med — RSO integration |
| avoid | FFXIV, CoD, Tarkov, R6 (unofficial), Marvel Rivals (live) | — | — | — | ToS-hostile or no legitimate source |

## 10. Future Extensions (Not In Scope)
- **Outcome-driven auto-clips:** use `ScoreChanged`/ace/clutch signals to auto-mark highlights (blocked on video clip capture, spec 14 — only audio clips exist today).
- **Per-match streaks & full stat pages:** ADR/HS%, rank/MMR, a personal profile surface built on `user_game_stats` + stored `matches`.
- **Rank tracking:** CS2 Premier rating deltas per session.
- **Played-not-streaked capture:** record non-streak-mode matches (casual/DM) via `streak_eligible: false` once a surface consumes them.

---

*This spec covers the telemetry adapter framework, the CS2 GSI adapter, the session/outcome model, the per-user streak store, and crew-first surfacing. For process-level detection and the game DB, see [17-GAME-SENSING.md](./17-GAME-SENSING.md). For the event ledger and post-game moments, see [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md).*
