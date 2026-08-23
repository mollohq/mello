# MELLO Game Sensing Specification

> **Component:** Game Detection, Game Catalogue, Game Presence
> **Version:** 0.3
> **Status:** Implemented on Windows. The macOS paths compile and pass CI, but have never run on a Mac. These are process enumeration, ICNS icon extraction and launcher locations.
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [02-MELLO-CORE.md](./02-MELLO-CORE.md), [03-LIBMELLO.md](./03-LIBMELLO.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md), [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md), [18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md), [22-GAME-UI-SURFACES.md](./22-GAME-UI-SURFACES.md)

---

## 1. Overview

Game sensing answers one question: which game does the user play, and for how
long. It does not select what to show. It does not draw anything.

```
  SENSE (this spec)          RECORD             CURATE         PRESENT
  ┌───────────────────┐    ┌────────────┐    ┌──────────┐   ┌──────────┐
  │ 17 Game Sensing   │───▶│ 16 Ledger  │───▶│ 19 Feed  │──▶│ 22 Game  │
  │    which game     │    │            │    │ Curation │   │    UI    │
  │    how long       │    │ game_      │    │          │   │          │
  │                   │    │ session    │    │          │   │          │
  │ 18 Telemetry      │───▶│ user_game_ │───▶│          │   │          │
  │    W/L, streaks   │    │ stats      │    │          │   │          │
  └───────────────────┘    └────────────┘    └──────────┘   └──────────┘
```

UI that this spec once defined now lives in
[22-GAME-UI-SURFACES.md](./22-GAME-UI-SURFACES.md). That UI is the crew sidebar
game list, the bottom bar states and the Slint component reference.

### Key decisions

| Decision | Reason |
|---|---|
| Identity comes from the install path, not the filename | Two games can ship `game.exe`. No two occupy the same directory. The library scan uses a path prefix for this reason |
| Every game produces a session | A game that nothing can name is tracked as provisional. Spec 19 decides what reaches a feed |
| A session starts at the process creation time | A game that runs before the client starts reports the full time it ran |
| Several games are tracked at once | The active set is a map. One entry is selected as primary for presence |

### Amendment (spec 18 — Game Telemetry)

This spec covers process-level detection only. In-game outcomes are a separate
layer above the process sensor. Those outcomes are win, loss, score and streak.
The `GameSensor` continues to emit `Started` and `Stopped` without change. See
[18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md).

---

## 2. Detection

File: `mello-core/src/game_sensing.rs`.

### 2.1 Scan loop and cadence

libmello supplies the process list through `mello_enumerate_games`. It fills
`MelloGameProcess` records. Each record holds the pid, the executable name, the
full path, the window title, the fullscreen flag, the foreground flag and the
process creation time.

The loop runs at two rates.

| Constant | Value | Condition |
|---|---|---|
| `GAME_SCAN_INTERVAL` | 15s | No game is tracked |
| `GAME_SCAN_INTERVAL_ACTIVE` | 4s | A game is active |

The loop compares each scan against a `HashMap<u32, ActiveGame>` keyed by pid.
This tracks several concurrent games.

`started_at_ms` holds the OS process creation time. On Windows this comes from
`GetProcessTimes`, converted from FILETIME to Unix milliseconds. The field is
`0` when the OS does not supply the value. The session then uses the first-seen
time.

### 2.2 Resolution ladder

Each process goes through four rungs in order. The first match wins.

| Rung | Source | Coverage |
|---|---|---|
| 1 | Curated executable table in `head.bin` | Games that no library scan finds, such as Valorant, League of Legends and Hearthstone |
| 2 | Installed library scan, by path prefix | Games installed through Steam, Epic or GOG |
| 3 | User-confirmed games in `user_games.rs` | Games the user named |
| 4 | Provisional tracking | A game that no rung above can name |

Rung 2 gets full catalogue identity when `appid_index.bin` maps the Steam appid
to an IGDB id. Epic and GOG have no such map. They use the launcher name, which
is authoritative.

### 2.3 What counts as a game

`looks_like_a_game` controls provisional tracking, not only the confirm prompt.
An earlier design controlled the prompt only. That design recorded every focused
window as a session.

The gate applies these rules in order.

1. Reject an empty path or an empty window title.
2. Reject any entry in `UNKNOWN_DENYLIST` (executable) or `UNKNOWN_PATH_DENYLIST`
   (path).
3. Reject auxiliary binaries. See section 2.4.
4. Accept an Unreal shipping binary. The name `*-Win64-Shipping.exe` identifies
   it.
5. Otherwise require one of two conditions: the process is fullscreen, or an
   engine marker file is in the same directory.

Engine markers include `unityplayer.dll`, `gameassembly.dll`, `steam_api64.dll`,
a `*_Data` directory and a `.pck` file.

Rule 5 rejects windowed launchers. It is also the narrowest rule. A launcher
that is windowed and has an engine marker in its directory passes the gate.
`LeagueClientUx.exe` has the window title "League of Legends". Its directory
holds no engine marker, so the gate rejects it. See section 8.

### 2.4 Auxiliary binaries

The engine-marker check applies to a directory. Every executable beside a Unity
or Unreal build gets that signature. On a Hearthstone install, this made
`Hearthstone Beta Launcher.exe` a tracked game beside the real game.

`AUXILIARY_SUFFIXES` matches a suffix of the executable stem. It does not match
a substring. A game whose name contains one of these words is not affected.
`agent47.exe` ends in "47", not in "agent".

```
launcher, updater, update, patcher, setup, installer, uninstall,
crashhandler, crashreporter, crashpad, errorreporter, helper,
service, services, daemon, server, config, settings, benchmark
```

The check removes trailing architecture digits first. `LeagueCrashHandler64`
becomes `leaguecrashhandler` and then matches.

### 2.5 Primary game selection

`pick_primary` selects the game to publish to presence. A fullscreen or
foreground process wins over a background process.

### 2.6 Restart recovery

`session_store.rs` stores sessions that are in progress. A client restart then
keeps them. A stored session resumes only when both the pid and `started_at_ms`
match. The OS reuses a pid, so the pid alone is not sufficient.

### 2.7 Unresolved telemetry

`unresolved.rs` counts each unresolved executable one time per run. These
executables are the candidates for `scripts/exe_mappings.json`.

`folder_of` splits on both separators. It does not use `std::path`, which treats
a backslash as a separator on Windows only. There are two reasons. A macOS build
must parse a path that was recorded on Windows. Steam also reports its own root
with forward slashes.

---

## 3. Catalogue and Resolution Sources

### 3.1 Bundled artifacts

`scripts/build_catalogue.py` builds these files from IGDB data dumps. The
installer ships them. IGDB prefers dump consumption over live API calls. The
live API allows 4 requests per second and 8 concurrent connections, which is too
slow for runtime resolution.

| Artifact | Magic | Size | Contents |
|---|---|---|---|
| `client/assets/catalogue/head.bin` | `MHD2` | 154 KB | 2,000 game records, 66 curated executables, 64 KB string blob |
| `client/assets/catalogue/appid_index.bin` | `MAI2` | 538 KB | 137,688 Steam appid to IGDB id pairs, delta and varint encoded |

`head.bin` holds the most-played games, not every released title. The library
scan reaches the remaining titles.

`catalogue.rs` supplies `lookup_exe`, `by_game_id`, `by_name` and `get`.
`AppIdIndex::bundled()` decodes the varint pairs.

The build filters games with a denylist of non-launchable IGDB types. It does
not use an allowlist of `game_type = "Main Game"`. The allowlist removed
Minecraft (Port), Resident Evil 2 (Remake) and Half-Life 2: Episode Two
(Standalone Expansion). The denylist recovered 9,538 games.

### 3.2 Curated executable table

File: `scripts/exe_mappings.json`. It holds 54 games and 66 executables.

This table covers games that no library scan finds. Their launchers keep no
machine-readable install index. Riot and Battle.net are examples.

| Game | Executable | State |
|---|---|---|
| Valorant | `VALORANT-Win64-Shipping.exe` | Verified against a real install |
| League of Legends | `League of Legends.exe` | Verified against a real install |
| Hearthstone | `Hearthstone.exe` | Verified against a real install |
| World of Warcraft, Diablo IV, Overwatch 2, PUBG, War Thunder, Roblox, HoYoPlay set | various | Not verified |

An incorrect name in this table is a bug. It is the most probable cause of a
"my game is not detected" report.

### 3.3 Installed library scan

File: `mello-core/src/library.rs`. `LibraryIndex::scan()` reads each launcher.
It resolves an entry by the longest path prefix. A game installed inside another
game's directory wins over the parent directory.

| Source | Mechanism |
|---|---|
| Steam | `libraryfolders.vdf`, then each `appmanifest_*.acf` |
| Epic | `.item` JSON manifests |
| GOG | Registry keys |

`game_id` has the form `<prefix>-<external_id>`. The id stays stable across a
launcher reinstall and across a library move to another disk.

The scan handles two conditions that occur on real machines.

| Condition | Handling |
|---|---|
| Duplicate libraries. `libraryfolders.vdf` and the registry spell the Steam root differently, for example `c:/program files (x86)/steam` against `C:\Program Files (x86)\Steam`. This produced 16 entries for 8 games | `dedup_paths` normalises the paths |
| Non-games. Steamworks Common Redistributables is an installed appid, but not a game | `NON_GAME_APPIDS` excludes it |

### 3.4 User games

`user_games.rs` holds only user-supplied data: confirmed games and dismissed
executables. It replaced `game_db.rs` and `games.json`. That allowlist limited
detection to 25 titles.

Crowd-sourced mappings are not built. The effort was too large for the current
stage.

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

---

## 7. File Structure

### 7.1 Sensing modules

```
mello-core/src/
├── game_sensing.rs     # Scan loop, resolution ladder, GameEvent enum
├── catalogue.rs        # head.bin / appid_index.bin readers
├── library.rs          # Steam / Epic / GOG install scan
├── user_games.rs       # User-confirmed games, dismissed executables
├── unresolved.rs       # Unresolved-executable count
├── session_store.rs    # Restart recovery for sessions in progress
├── game_state.rs       # GameStateManager, UI and presence coordination
└── presence.rs         # GamePresence, activity types

client/assets/catalogue/
├── head.bin            # 2,000 games, 66 curated executables
└── appid_index.bin     # 137,688 Steam appid to IGDB pairs

scripts/
├── build_catalogue.py  # IGDB dump to binary artifacts
└── exe_mappings.json   # Curated executable table, 54 games
```

`game_db.rs` and `assets/games.json` are deleted. Any document that refers to
them is out of date.

### 7.2 Backend

| File | Role |
|---|---|
| `backend/.../presence.go` | `game` field on presence, handled in `presence_update` |
| `backend/.../crew_state.go` | Computes `active_games` from member presences |
| `backend/.../game_icons.go` | Icon upload and serve, `gameIconMaxBytes` |

---

## 8. Testing

### 8.1 Unit tests

- Resolution ladder: each rung alone, and the order of precedence.
- `is_auxiliary_binary`: suffix match, architecture-digit removal.
- `looks_like_a_game`: denylists, engine markers, Unreal shipping binaries.
- Primary selection: fullscreen preference, one game, no games.
- `dedup_paths`: mixed separators and mixed case.
- `folder_of`: a Windows path parses on every platform.
- Restart recovery: a resume needs both the pid and `started_at_ms`.
- Session thresholds: below 2 minutes records nothing. Below 5 minutes records
  the session but shows no post-game prompt.

### 8.2 Verified on real hardware

The sensor ran against live processes on a Windows machine. The results follow.

| Process | Result |
|---|---|
| `VALORANT-Win64-Shipping.exe` | Resolved to Valorant |
| `VALORANT.exe` (0.2 MB stub, runs for the full session) | Ignored. It has no window title |
| `League of Legends.exe` | Resolved to League of Legends during a match |
| `LeagueClient.exe`, `LeagueClientUx.exe`, 6 × `LeagueClientUxRender.exe` | Ignored, while the match ran |
| `LeagueCrashHandler64.exe` | Ignored. The architecture-digit removal matched it |
| `Riot Client.exe`, `RiotClientServices.exe` | Ignored |
| `vgc.exe`, `vgtray.exe` (Vanguard) | Ignored |
| `EpicWebHelper.exe` | Ignored |

A full Steam library move to another disk re-resolved every game. It produced no
duplicates and no orphans.

### 8.3 Not verified

- macOS: no part of the macOS sensing path has run on a Mac.
- The executable names marked "Not verified" in section 3.2.

---

*This spec defines game detection, the game catalogue and presence integration.
For the appearance of game surfaces, see [22-GAME-UI-SURFACES.md](./22-GAME-UI-SURFACES.md).
For the sessions that reach a feed, see [19-FEED-CURATION-PERSONAL-STATS.md](./19-FEED-CURATION-PERSONAL-STATS.md).
For in-game outcomes, see [18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md).
For the event ledger and post-game moments, see [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md).
For presence and crew state, see [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md).
For the video capture pipeline, see [14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md).*
