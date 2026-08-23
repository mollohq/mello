# MELLO Game Sensing Specification

> **Component:** Game Detection, Game Catalogue, Game Presence
> **Version:** 0.3
> **Status:** Implemented on Windows. The macOS paths (process enumeration, ICNS icon extraction, launcher locations) are written and compile, but have never been executed on a Mac.
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [02-MELLO-CORE.md](./02-MELLO-CORE.md), [03-LIBMELLO.md](./03-LIBMELLO.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md), [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md), [18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md), [20-GAME-UI-SURFACES.md](./20-GAME-UI-SURFACES.md)

---

## 1. Overview

Game sensing answers one question: **which game is this user playing, and for
how long.** It does not decide what is worth showing, and it does not draw
anything.

```
  SENSE (this spec)          RECORD             CURATE         PRESENT
  ┌───────────────────┐    ┌────────────┐    ┌──────────┐   ┌──────────┐
  │ 17 Game Sensing   │───▶│ 16 Ledger  │───▶│ 19 Feed  │──▶│ 20 Game  │
  │    which game     │    │            │    │ Curation │   │    UI    │
  │    how long       │    │ game_      │    │          │   │          │
  │                   │    │ session    │    │          │   │          │
  │ 18 Telemetry      │───▶│ user_game_ │───▶│          │   │          │
  │    W/L, streaks   │    │ stats      │    │          │   │          │
  └───────────────────┘    └────────────┘    └──────────┘   └──────────┘
```

UI that was once specified here — the crew sidebar game list, the bottom bar
states, and the Slint component reference — now lives in
[20-GAME-UI-SURFACES.md](./20-GAME-UI-SURFACES.md).

### Key decisions

- **Identity comes from install path, not filename.** Two games can ship
  `game.exe`; no two occupy the same directory. This is why the library scan is
  keyed on path prefix.
- **Every game produces a session.** A title nothing can name is still tracked
  provisionally, because "ostkatt played something for 4h" beats silence. What
  reaches a feed is spec 19's decision, not this layer's.
- **Sessions date from process creation time**, not from when Mello noticed. A
  game already running when the client starts reports the hours it actually ran.
- **Several games are tracked at once.** The active set is a map, not a single
  slot; one of them is chosen as primary for presence.

### Amendment (spec 18 — Game Telemetry)

This spec covers *process-level* detection only. In-game outcomes (win/loss,
score, streaks) are a separate, pluggable layer **above** the process sensor —
the `GameSensor` keeps emitting `Started`/`Stopped` unchanged. See
[18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md).

---

## 2. Detection

`mello-core/src/game_sensing.rs`.

### 2.1 Scan loop and cadence

Process enumeration comes from libmello (`mello_enumerate_games`), which fills
`MelloGameProcess` records with pid, exe, full path, window title, fullscreen
and foreground flags, and process creation time.

The loop runs at two rates:

| Constant | Value | When |
|---|---|---|
| `GAME_SCAN_INTERVAL` | 15s | Idle — nothing is being tracked |
| `GAME_SCAN_INTERVAL_ACTIVE` | 4s | A game is active, so a state change matters sooner |

Started/stopped edges are computed against a `HashMap<u32, ActiveGame>` keyed by
pid, so several concurrent games are tracked rather than one.

`started_at_ms` comes from the OS process creation time — `GetProcessTimes` on
Windows, converted from FILETIME to Unix ms. When the OS will not report it, the
field is `0` and the session falls back to first-seen time.

### 2.2 Resolution ladder

Every enumerated process runs down four rungs, in order. The first hit wins.

| Rung | Source | Covers |
|---|---|---|
| 1 | Curated executable table (`head.bin`) | The head of the play distribution — launcher-agnostic titles like Valorant, League, Hearthstone that no library scan sees |
| 2 | Installed library scan, by path prefix | Everything installed through Steam, Epic or GOG |
| 3 | User-confirmed custom games (`user_games.rs`) | What the user named themselves |
| 4 | Provisional tracking | A game nothing above could name |

Rung 2 upgrades to full catalogue identity when the Steam appid maps to an IGDB
id through `appid_index.bin`; Epic and GOG have no such mapping and fall back to
the launcher's own name, which is authoritative anyway.

### 2.3 What counts as a game

`looks_like_a_game` gates **provisional tracking**, not merely prompting. An
earlier draft gated prompting only, which would have recorded every focused
window as a session — "played Notepad for 4 hours" is worse than missing an
obscure game.

The gate, in order:

1. Reject empty path or empty window title.
2. Reject anything on `UNKNOWN_DENYLIST` (exe) or `UNKNOWN_PATH_DENYLIST` (path).
3. Reject auxiliary binaries (§2.4).
4. Accept Unreal shipping binaries outright — `*-Win64-Shipping.exe` is
   self-describing.
5. Otherwise require **fullscreen, or an engine marker beside the executable**
   (`unityplayer.dll`, `gameassembly.dll`, `steam_api64.dll`, a `*_Data`
   directory, a `.pck` file, and similar).

Rule 5 is what stops windowed launchers. It is also the narrowest part of the
gate: a launcher that is both windowed **and** sits beside engine files would
pass. `LeagueClientUx.exe` is exactly that shape minus the engine files — it
carries the window title "League of Legends" and is rejected only because its
directory has no marker. Verified live; see §8.

### 2.4 Auxiliary binaries

The engine-marker check is directory-scoped, so every executable beside a Unity
or Unreal build inherits that build's signature. Against a real Hearthstone
install that made `Hearthstone Beta Launcher.exe` its own tracked game sitting
next to the real one.

`AUXILIARY_SUFFIXES` matches suffixes of the executable **stem**, not
substrings, so a game whose name merely contains one of the words is unaffected
(`agent47.exe` ends in "47", not "agent"):

```
launcher, updater, update, patcher, setup, installer, uninstall,
crashhandler, crashreporter, crashpad, errorreporter, helper,
service, services, daemon, server, config, settings, benchmark
```

Trailing architecture digits are stripped first, so `LeagueCrashHandler64`
reduces to `leaguecrashhandler` and matches.

### 2.5 Primary game selection

`pick_primary` chooses which of the tracked games is published to presence.
Fullscreen and foreground win over background processes.

### 2.6 Restart recovery

`session_store.rs` persists in-flight sessions so a client restart does not
orphan them. A stored session resumes only when **both** pid and `started_at_ms`
match, because a pid alone is recycled by the OS.

### 2.7 Unresolved telemetry

`unresolved.rs` tallies each unresolved executable once per run. These are the
rows worth curating into `scripts/exe_mappings.json`, so the curated table grows
from evidence rather than guesswork.

`folder_of` splits on **both** separators rather than using `std::path`, which
treats a backslash as a separator only on Windows. Two reasons: a macOS build
must still parse a path recorded on Windows, and the separators genuinely mix —
Steam reports its own root with forward slashes.

---

## 3. Catalogue and Resolution Sources

### 3.1 Bundled artifacts

Built by `scripts/build_catalogue.py` from IGDB data dumps and shipped in the
installer. IGDB prefers dump consumption over live API calls, and the live API
is capped at 4 req/s with 8 concurrent connections — unusable for resolution at
runtime.

| Artifact | Magic | Size | Contents |
|---|---|---|---|
| `client/assets/catalogue/head.bin` | `MHD2` | 154 KB | 2,000 game records, 66 curated executables, 64 KB string blob |
| `client/assets/catalogue/appid_index.bin` | `MAI2` | 538 KB | 137,688 Steam appid → IGDB id pairs, delta + varint encoded |

`head.bin` deliberately holds the **head** of the play distribution rather than
every title ever released. Catching the top 50 games matters more than 95%
coverage of the long tail, and the long tail is reached anyway through the
library scan.

`catalogue.rs` exposes `lookup_exe`, `by_game_id`, `by_name` and `get`;
`AppIdIndex::bundled()` decodes the varint pairs.

Games are filtered by a **denylist** of non-launchable IGDB types, not an
allowlist of `game_type = "Main Game"`. The allowlist silently dropped Minecraft
(Port), Resident Evil 2 (Remake) and Half-Life 2: Episode Two (Standalone
Expansion). The denylist recovered 9,538 games.

### 3.2 Curated executable table

`scripts/exe_mappings.json` — currently 54 games across 66 executables. This
covers titles that no library scan can see, because their launchers keep no
machine-readable install index: Riot, Battle.net, and similar.

Entries verified against real installs: Valorant
(`VALORANT-Win64-Shipping.exe`), League of Legends (`League of Legends.exe`),
Hearthstone (`Hearthstone.exe`), CS2, Dota 2.

Entries still unverified, because nobody has run them here: World of Warcraft,
Diablo IV, Overwatch 2, PUBG, War Thunder, Roblox, and the HoYoPlay set. A wrong
name here is a bug, and it is the most likely source of a "my game is not
detected" report.

### 3.3 Installed library scan

`mello-core/src/library.rs`. `LibraryIndex::scan()` walks each launcher and
returns entries resolved by **longest path prefix**, so a game installed inside
another game's directory wins over its parent.

| Source | Mechanism |
|---|---|
| Steam | `libraryfolders.vdf` → each library's `appmanifest_*.acf` |
| Epic | `.item` JSON manifests |
| GOG | Registry keys |

`game_id` is `<prefix>-<external_id>`, which keeps ids stable across launcher
reinstalls and across a library move to another disk.

Two traps this scan handles, both found on real hardware:

- **Duplicate libraries.** `libraryfolders.vdf` spells the Steam root
  differently from the registry (`c:/program files (x86)/steam` versus
  `C:\Program Files (x86)\Steam`), which turned 8 games into 16 until
  `dedup_paths` normalised them.
- **Non-games.** `NON_GAME_APPIDS` excludes entries like Steamworks Common
  Redistributables, which is an installed appid but not a game.

### 3.4 User games

`user_games.rs` holds only what the user contributed: confirmed custom games and
dismissed executables. It replaced the deleted `game_db.rs`/`games.json`
allowlist, which capped detection at 25 titles.

**Crowd-sourced mappings are deliberately not built.** They were considered and
dropped as disproportionate effort for the current stage. This is unbuilt scope,
not a defect — no bug report will ever surface it.

---

## 4. Game State Manager

The game state manager in mello-core consumes `GameEvent`s from the scanner and orchestrates all downstream effects.

```rust
// mello-core/src/game_state.rs

pub struct GameStateManager {
    current_game: Option<ActiveGame>,
    session_start: Option<i64>,
}

impl GameStateManager {
    pub fn handle_event(&mut self, event: GameEvent, ctx: &AppContext) {
        match event {
            GameEvent::Started(game) => {
                self.current_game = Some(game.clone());
                self.session_start = Some(now_ms());

                // 1. Update presence
                ctx.presence.update_activity(Activity::Playing {
                    game_name: game.game_name.clone(),
                    game_id: game.game_id.clone(),
                    started_at: game.started_at,
                });

                // 2. Update bottom bar UI
                ctx.ui.show_now_playing(&game);
            }

            GameEvent::Stopped(game) => {
                let duration_min = self.session_start
                    .map(|s| ((now_ms() - s) / 60_000) as u32)
                    .unwrap_or(0);

                self.current_game = None;
                self.session_start = None;

                // 1. Clear presence game activity
                ctx.presence.clear_game_activity();

                // 2. Report game session to backend (feeds event ledger)
                if duration_min >= 2 {
                    ctx.backend.call_rpc("game_session_end", GameSessionEndRequest {
                        crew_id: ctx.active_crew_id(),
                        game_name: game.game_name.clone(),
                        duration_min,
                    });
                }

                // 3. Trigger post-game UI flow
                if duration_min >= 5 {
                    ctx.ui.show_post_game(&game, duration_min);
                }
            }
        }
    }
}
```

### 4.1 Minimum Session Thresholds

| Threshold | Value | Purpose |
|-----------|-------|---------|
| Minimum session for ledger event | 2 minutes | Filters accidental launches and launcher processes |
| Minimum session for post-game card | 5 minutes | Don't prompt "how'd it go?" for a game the user barely opened |

---

## 5. Presence Integration (Spec 11 Amendment)

### 5.1 New Activity Type

Add `playing` to the activity types table in spec 11, section 2.1:

| Type | Fields | Description |
|------|--------|-------------|
| `playing` | `game_name`, `game_id`, `started_at` | Playing a detected game |

Example presence payload:

```json
{
    "user_id": "user_abc",
    "status": "online",
    "activity": {
        "type": "playing",
        "game_name": "Counter-Strike 2",
        "game_id": "counter-strike-2",
        "started_at": "2026-04-03T14:00:00Z"
    },
    "updated_at": "2026-04-03T14:00:05Z"
}
```

### 5.2 Compound Activity

A user can be in voice AND playing a game simultaneously. This is the most common scenario (you're in voice chat with your crew while gaming). The presence model handles this with a compound approach:

```json
{
    "user_id": "user_abc",
    "status": "online",
    "activity": {
        "type": "in_voice",
        "crew_id": "crew_xyz",
        "channel_id": "ch_general",
        "channel_name": "General"
    },
    "game": {
        "game_name": "Counter-Strike 2",
        "game_id": "counter-strike-2",
        "started_at": "2026-04-03T14:00:00Z"
    },
    "updated_at": "2026-04-03T14:00:05Z"
}
```

The `game` field is a separate top-level field on presence, not nested inside `activity`. This way, `activity` still represents the primary social action (voice, streaming, watching), and `game` is an orthogonal signal that can coexist with any activity type.

**Rules:**
- `game` is set when a game is detected, regardless of `activity.type`
- `game` is cleared when the game process exits
- `activity.type` = `playing` is only used when the user is not in voice/streaming/watching (i.e., they're just online and gaming, no social activity)
- When `activity.type` is `in_voice` or `streaming`, the `game` field still shows what they're playing

### 5.3 Presence Update Flow

```
Game detected
    |
    v
Is user in voice/streaming/watching?
    |
    ├── Yes: Keep current activity type, set game field
    |         presence_update({ game: { ... } })
    |
    └── No:  Set activity type to "playing", set game field
              presence_update({ activity: { type: "playing", ... }, game: { ... } })
    |
    v
Game process exits
    |
    v
Clear game field
    |
    ├── Was activity "playing"? → Set activity to "none"
    └── Was activity something else? → Keep activity, just clear game
```

### 5.4 Server-Side Crew State Extension

The crew state (spec 11, section 2.2) gains a new field in the aggregated state:

```json
{
    "crew_id": "crew_xyz",
    "counts": { "online": 4, "total": 6 },
    "active_games": [
        {
            "game_id": "counter-strike-2",
            "game_name": "Counter-Strike 2",
            "short_name": "CS2",
            "color": "#DE9B35",
            "players": [
                { "user_id": "user_a", "username": "ash" },
                { "user_id": "user_b", "username": "koji" }
            ]
        }
    ],
    "voice_channels": [ ... ],
    "stream": { ... }
}
```

`active_games` is computed by the crew state manager by scanning online member presences for the `game` field. Updated on every presence change. Pushed to subscribers following existing spec 11 cadence (instant for active crew, batched for sidebar).

---

---

## 6. Backend RPCs

### 6.1 Game Session End (Spec 16, already defined)

```go
// game_session_end RPC — already in crew_events.go (spec 16)
// Called when game process exits and duration >= 2 min
```

No new backend RPCs needed for game sensing. The existing `presence_update`, `crew_catchup`, and `game_session_end` RPCs handle everything.

### 6.2 Crew Recent Games (new, optional)

If the catch-up response extension (section 6.4) is insufficient, add a dedicated RPC:

```go
initializer.RegisterRpc("crew_recent_games", CrewRecentGamesRPC)

// Request:
// { "crew_id": "crew_xyz" }

// Response:
// { "games": [ { "game_id": "...", "game_name": "...", ... } ] }

func CrewRecentGamesRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    var req struct {
        CrewID string `json:"crew_id"`
    }
    json.Unmarshal([]byte(payload), &req)

    ledger := readLedger(ctx, nk, req.CrewID)
    cutoff := time.Now().Add(-7 * 24 * time.Hour).UnixMilli()

    gameMap := make(map[string]*RecentGame)
    for _, event := range ledger.Events {
        if event.Timestamp < cutoff || event.Type != "game_session" {
            continue
        }
        d := event.Data.(GameSessionData)
        g, ok := gameMap[d.GameName]
        if !ok {
            g = &RecentGame{
                GameName:     d.GameName,
                PlayerSet:    make(map[string]string),
                SessionCount: 0,
            }
            gameMap[d.GameName] = g
        }
        g.SessionCount++
        if event.Timestamp > g.LastPlayed {
            g.LastPlayed = event.Timestamp
        }
        for i, pid := range d.PlayerIDs {
            g.PlayerSet[pid] = d.PlayerNames[i]
        }
    }

    // Convert to response, sort by last_played desc
    // ...
}
```

---

---

## 7. File Structure

### 7.1 Sensing modules

```
mello-core/src/
├── game_sensing.rs     # Scan loop, resolution ladder, GameEvent enum
├── catalogue.rs        # head.bin / appid_index.bin readers
├── library.rs          # Steam / Epic / GOG install scan
├── user_games.rs       # User-confirmed games, dismissed executables
├── unresolved.rs       # Unresolved-executable tally
├── session_store.rs    # Restart recovery for in-flight sessions
├── game_state.rs       # GameStateManager, UI/presence coordination
└── presence.rs         # GamePresence, activity types

client/assets/catalogue/
├── head.bin            # 2,000 games + 66 curated executables
└── appid_index.bin     # 137,688 Steam appid → IGDB pairs

scripts/
├── build_catalogue.py  # IGDB dump → binary artifacts
└── exe_mappings.json   # Curated executable table (54 games)
```

`game_db.rs` and `assets/games.json` were **deleted**. Anything still referring
to them is stale.

### 7.2 Backend

| File | Role |
|---|---|
| `backend/.../presence.go` | `game` field on presence, handled in `presence_update` |
| `backend/.../crew_state.go` | Computes `active_games` from member presences |
| `backend/.../game_icons.go` | Icon upload/serve, `gameIconMaxBytes` |

---

## 8. Testing

### 8.1 Unit tests

- Resolution ladder: each rung hit in isolation and in precedence order
- `is_auxiliary_binary`: suffix matching, architecture-digit stripping
- `looks_like_a_game`: denylists, engine markers, Unreal shipping binaries
- Primary selection: fullscreen preference, single game, no games
- `dedup_paths`: mixed separators and letter case
- `folder_of`: Windows paths parsed correctly on every platform
- Restart recovery: resume requires both pid and `started_at_ms`
- Session thresholds: under 2 min not reported, under 5 min skips post-game

### 8.2 Verified on real hardware

The following were confirmed by running the sensor against live processes on a
Windows machine, and each one found something unit tests could not:

- Valorant resolves to `Valorant` while its 0.2 MB `VALORANT.exe` stub, which
  runs for the whole session, is correctly ignored for having no window title.
- League resolves to `League of Legends` during a match while `LeagueClient.exe`,
  `LeagueClientUx.exe` (window title "League of Legends") and six
  `LeagueClientUxRender.exe` processes run alongside and are all ignored.
- `LeagueCrashHandler64.exe` is filtered by the architecture-digit strip.
- Riot Client, `RiotClientServices.exe`, Vanguard's `vgc.exe`/`vgtray.exe` and
  `EpicWebHelper.exe` are all ignored.
- A full Steam library relocation to another disk re-resolved every game with no
  duplicates and no orphans.

### 8.3 Not verified

- **macOS: nothing.** No part of the macOS sensing path has run on a Mac.
- The curated executable names listed as unverified in §3.2.

---

*This spec covers game detection, the game catalogue, and presence integration.
For how game activity is drawn, see [20-GAME-UI-SURFACES.md](./20-GAME-UI-SURFACES.md).
For which sessions reach a feed, see [19-FEED-CURATION-PERSONAL-STATS.md](./19-FEED-CURATION-PERSONAL-STATS.md).
For in-game outcomes, see [18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md).
For the event ledger and post-game moments, see [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md).
For presence and crew state, see [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md).
For the video capture pipeline, see [14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md).*
