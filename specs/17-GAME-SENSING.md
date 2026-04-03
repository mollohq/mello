# MELLO Game Sensing Specification

> **Component:** Game Detection, Game Database, Game Presence  
> **Version:** 0.1  
> **Status:** Planned  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)  
> **Related:** [02-MELLO-CORE.md](./02-MELLO-CORE.md), [03-LIBMELLO.md](./03-LIBMELLO.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md), [14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md), [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md)

---

## 1. Overview

Game sensing detects what games the user is running, surfaces that information across the UI, and feeds the presence system, crew sidebar game list, and post-game moment flow.

```
┌────────────────────────────────────────────────────────────────────┐
│                        GAME SENSING                                │
│                                                                    │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────────────┐  │
│  │  Process      │───▶│  Game DB     │───▶│  Game State Manager  │  │
│  │  Scanner      │    │  (local)     │    │  (mello-core)        │  │
│  │  (libmello)   │    │              │    │                      │  │
│  └──────────────┘    └──────────────┘    └──────────┬───────────┘  │
│                                                      │             │
│                    ┌─────────────────┬────────────────┼──────┐     │
│                    ▼                 ▼                ▼      ▼     │
│              ┌───────────┐   ┌────────────┐  ┌───────┐ ┌───────┐  │
│              │ Presence  │   │ Bottom Bar │  │ Crew  │ │ Event │  │
│              │ Update    │   │ NOW PLAYING│  │Sidebar│ │Ledger │  │
│              └───────────┘   └────────────┘  └───────┘ └───────┘  │
│                                                                    │
└────────────────────────────────────────────────────────────────────┘
```

### Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Scan interval | 5 seconds (const, tunable) | Fast enough for responsive UI, low CPU cost |
| Game database | Bundled JSON, seeded from IGDB | No live API calls during detection; offline-capable |
| Matching strategy | Executable name lookup | Simple, reliable, cross-platform |
| Presence scope | All crews see game activity | Useful for sidebar game lists everywhere |
| Crew game list | Persistent (last 7 days) + live overlay | Games don't vanish when no one is playing |
| Platform support | Windows first, macOS/Linux follow | Game detection is OS-specific |

### What Changes (Spec 11 Amendment)

The presence activity model gains a new `playing` type:

| Type | Fields | Description |
|------|--------|-------------|
| `playing` | `game_name`, `game_id`, `started_at` | Playing a detected game |

This type can coexist with voice/streaming. A user can be `in_voice` AND `playing` simultaneously. See section 5 for how this is handled.

### What Changes (Spec 14 Amendment)

The existing `enumerate_game_processes()` in libmello (spec 14, section 4.6) is reused. The bundled `assets/games.json` is expanded with richer metadata (section 3). No changes to the C++ interface.

---

## 2. Process Scanner

### 2.1 Detection Loop

The detection loop runs in mello-core (Rust), calling into libmello's existing `enumerate_game_processes()` C API every `GAME_SCAN_INTERVAL` seconds.

```rust
// mello-core/src/game_sensing.rs

const GAME_SCAN_INTERVAL: Duration = Duration::from_secs(5);

pub struct GameSensor {
    game_db: GameDatabase,
    current_game: Option<ActiveGame>,
    scan_handle: Option<JoinHandle<()>>,
}

pub struct ActiveGame {
    pub game_id: String,       // From game DB (IGDB slug or internal ID)
    pub game_name: String,     // Display name
    pub exe: String,           // Matched executable
    pub pid: u32,
    pub started_at: i64,       // Unix ms
}

impl GameSensor {
    pub fn start(&mut self, event_tx: Sender<GameEvent>) {
        let db = self.game_db.clone();
        self.scan_handle = Some(std::thread::spawn(move || {
            let mut previous: Option<ActiveGame> = None;
            loop {
                std::thread::sleep(GAME_SCAN_INTERVAL);

                let processes = libmello::enumerate_game_processes();
                let detected = pick_primary_game(&db, &processes);

                match (&previous, &detected) {
                    (None, Some(game)) => {
                        let _ = event_tx.send(GameEvent::Started(game.clone()));
                    }
                    (Some(prev), None) => {
                        let _ = event_tx.send(GameEvent::Stopped(prev.clone()));
                    }
                    (Some(prev), Some(game)) if prev.pid != game.pid => {
                        let _ = event_tx.send(GameEvent::Stopped(prev.clone()));
                        let _ = event_tx.send(GameEvent::Started(game.clone()));
                    }
                    _ => {} // No change
                }

                previous = detected;
            }
        }));
    }
}

#[derive(Debug, Clone)]
pub enum GameEvent {
    Started(ActiveGame),
    Stopped(ActiveGame),
}
```

### 2.2 Primary Game Selection

When multiple games are running simultaneously (rare but possible), pick the one with the most recent start time. If start times are unavailable, prefer the game whose process was created most recently.

```rust
fn pick_primary_game(db: &GameDatabase, processes: &[GameProcess]) -> Option<ActiveGame> {
    // Filter to processes that match the game DB
    let mut matches: Vec<ActiveGame> = processes
        .iter()
        .filter_map(|p| db.lookup_by_exe(&p.exe).map(|entry| ActiveGame {
            game_id: entry.id.clone(),
            game_name: entry.name.clone(),
            exe: p.exe.clone(),
            pid: p.pid,
            started_at: now_ms(),
        }))
        .collect();

    // Prefer fullscreen games (likely the "active" one)
    matches.sort_by(|a, b| {
        let a_fs = processes.iter().find(|p| p.pid == a.pid).map_or(false, |p| p.is_fullscreen);
        let b_fs = processes.iter().find(|p| p.pid == b.pid).map_or(false, |p| p.is_fullscreen);
        b_fs.cmp(&a_fs)
    });

    matches.into_iter().next()
}
```

### 2.3 Platform-Specific Process Enumeration

Spec 14 defines the libmello C++ side for Windows. The C API surface:

```c
// Already in libmello C API (spec 14, section 4.6)
typedef struct {
    uint32_t    pid;
    const char* name;           // Game display name
    const char* exe;            // Executable filename
    bool        is_fullscreen;
} mello_game_process;

uint32_t mello_enumerate_game_processes(mello_game_process* out, uint32_t max_count);
```

**macOS:** Use `NSWorkspace.runningApplications` to list processes, match against game DB by bundle identifier or executable name.

**Linux:** Read `/proc/*/comm` or `/proc/*/exe` symlinks, match against game DB.

These platform backends are behind the same C API. The detection loop in mello-core is platform-agnostic.

---

## 3. Game Database

### 3.1 Schema

Bundled as `assets/games.json` (replaces the simpler version from spec 14):

```json
[
    {
        "id": "counter-strike-2",
        "igdb_id": 131800,
        "name": "Counter-Strike 2",
        "short_name": "CS2",
        "exe": [
            "cs2.exe",
            "csgo.exe"
        ],
        "icon_url": "https://images.igdb.com/igdb/image/upload/t_logo_med/cs2.png",
        "cover_url": "https://images.igdb.com/igdb/image/upload/t_cover_big/co5vst.png",
        "color": "#DE9B35",
        "category": "fps"
    },
    {
        "id": "valorant",
        "igdb_id": 126459,
        "name": "Valorant",
        "short_name": "Valorant",
        "exe": [
            "VALORANT-Win64-Shipping.exe"
        ],
        "icon_url": "https://images.igdb.com/igdb/image/upload/t_logo_med/valorant.png",
        "cover_url": "https://images.igdb.com/igdb/image/upload/t_cover_big/co2mvt.png",
        "color": "#FF4655",
        "category": "fps"
    },
    {
        "id": "league-of-legends",
        "igdb_id": 115,
        "name": "League of Legends",
        "short_name": "League",
        "exe": [
            "League of Legends.exe",
            "LeagueClient.exe"
        ],
        "icon_url": "https://images.igdb.com/igdb/image/upload/t_logo_med/lol.png",
        "cover_url": "https://images.igdb.com/igdb/image/upload/t_cover_big/co49wj.png",
        "color": "#C8AA6E",
        "category": "moba"
    }
]
```

### 3.2 Field Reference

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | string | Yes | Stable identifier, IGDB slug when available |
| `igdb_id` | number | No | IGDB game ID for future API enrichment |
| `name` | string | Yes | Full display name |
| `short_name` | string | Yes | Abbreviated name for tight UI spaces (sidebar pills, bottom bar) |
| `exe` | string[] | Yes | Executable filenames to match (case-insensitive) |
| `icon_url` | string | No | Square icon/logo URL (cached locally on first load) |
| `cover_url` | string | No | Cover art URL (cached locally on first load) |
| `color` | string | No | Brand color hex for UI accents (game badge background) |
| `category` | string | No | Genre tag: `fps`, `moba`, `br`, `mmo`, `rpg`, `racing`, `sports`, `sandbox`, `strategy`, `other` |

### 3.3 Lookup

```rust
// mello-core/src/game_db.rs

use std::collections::HashMap;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GameEntry {
    pub id: String,
    pub igdb_id: Option<u64>,
    pub name: String,
    pub short_name: String,
    pub exe: Vec<String>,
    pub icon_url: Option<String>,
    pub cover_url: Option<String>,
    pub color: Option<String>,
    pub category: Option<String>,
}

#[derive(Clone)]
pub struct GameDatabase {
    by_exe: HashMap<String, GameEntry>, // lowercase exe -> entry
}

impl GameDatabase {
    pub fn load_bundled() -> Self {
        let json = include_str!("../assets/games.json");
        let entries: Vec<GameEntry> = serde_json::from_str(json).expect("invalid games.json");
        let mut by_exe = HashMap::new();
        for entry in &entries {
            for exe in &entry.exe {
                by_exe.insert(exe.to_lowercase(), entry.clone());
            }
        }
        GameDatabase { by_exe }
    }

    pub fn lookup_by_exe(&self, exe: &str) -> Option<&GameEntry> {
        self.by_exe.get(&exe.to_lowercase())
    }
}
```

### 3.4 Database Seeding (Development)

During development, seed `games.json` with the top 20 games by calling the IGDB API:

```
POST https://api.igdb.com/v4/games
fields name, slug, cover.url, category;
where platforms = (6) & total_rating_count > 100;
sort total_rating_count desc;
limit 20;
```

Map executable names manually for the initial set. The full IGDB database dump (available on request from IGDB/Twitch) will replace this for production, with an automated pipeline to extract exe mappings from known game metadata.

### 3.5 Image Caching

Icon and cover images are downloaded on first access and cached to disk:

```
~/.mello/cache/games/icons/{game_id}.png
~/.mello/cache/games/covers/{game_id}.png
```

Cache is populated lazily (when a game first appears in the sidebar or bottom bar). Stale entries are refreshed when the game DB updates via the auto-updater.

### 3.6 Database Updates

The game DB ships with the client binary. Updates are delivered through the existing auto-updater (spec 07). The DB file is versioned:

```json
{
    "version": 1,
    "updated_at": "2026-04-01T00:00:00Z",
    "games": [ ... ]
}
```

The auto-updater can push a new `games.json` without requiring a full client update. The client checks the version number on startup and hot-reloads if the file has changed.

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

## 6. Crew Sidebar Game List

The crew sidebar game list (as shown in the mockups) combines two data sources:

### 6.1 Data Sources

| Source | Data | Purpose |
|--------|------|---------|
| Crew state `active_games` (live) | Who is playing what right now | Green dots, player count, "live" indicator |
| Event ledger `game_session` events (persistent) | Who played what in the last 7 days | Persistent game entries even when no one is online |

### 6.2 Merged View

```rust
pub struct SidebarGameEntry {
    pub game_id: String,
    pub game_name: String,
    pub short_name: String,
    pub color: Option<String>,
    pub icon_url: Option<String>,
    pub live_players: Vec<PlayerInfo>,    // From presence (playing right now)
    pub recent_players: Vec<PlayerInfo>,  // From ledger (played in last 7 days, not live)
    pub is_live: bool,                    // At least one live player
}
```

The client builds this merged list:

```
1. Start with live games from crew state active_games
2. Merge in recent games from catch-up data (event ledger)
3. Deduplicate by game_id
4. Sort: live games first (sorted by player count desc), then recent games (sorted by most recent session)
5. Cap at 8 entries to prevent sidebar bloat
```

### 6.3 Sidebar Item Rendering

Each game entry in the sidebar shows:

```
┌──────────────────────────────────────────────────┐
│  [CS]  Counter-Strike 2                     3    │
│        ●● ●                                      │
└──────────────────────────────────────────────────┘
```

- Game badge: `short_name` on a colored background (`color` field)
- Full game name
- Player dots: green for live players, gray for recent-only players
- Player count (total unique across live + recent)

When a game has only recent players (no one live), the entry appears dimmed:

```
┌──────────────────────────────────────────────────┐
│  [R]   Rocket League                        1    │
│        ○                                         │
└──────────────────────────────────────────────────┘
```

### 6.4 Recent Games Data

The client already has catch-up data from the `crew_catchup` RPC (spec 16). To build the recent games list without an extra RPC, extend the catch-up response:

```json
{
    "crew_id": "crew_xyz",
    "catchup_text": "...",
    "top_events": [ ... ],
    "has_events": true,
    "recent_games": [
        {
            "game_id": "counter-strike-2",
            "game_name": "Counter-Strike 2",
            "short_name": "CS2",
            "color": "#DE9B35",
            "player_ids": ["user_a", "user_b", "user_c"],
            "player_names": ["ash", "koji", "nav"],
            "session_count": 7,
            "last_played": 1711400000000
        }
    ]
}
```

The server computes `recent_games` by aggregating `game_session` events from the ledger, grouped by game, over the 7-day window.

Alternatively, expose a dedicated RPC:

```go
initializer.RegisterRpc("crew_recent_games", CrewRecentGamesRPC)
```

This is called once when the sidebar loads (or when the user switches to a crew), not polled. Live updates come through presence.

---

## 7. Bottom Bar UI

### 7.1 Now Playing State

When a game is detected, the bottom bar center content shows:

```
[game badge]  NOW PLAYING          [STREAM]
              Counter-Strike 2
```

The game badge uses the `short_name` and `color` from the game DB. The STREAM button is shown only if the user has streaming capability (hardware encoder detected).

### 7.2 Post-Game State

When the game exits (and session was >= 5 minutes), the center content morphs:

```
[game badge]  How'd it go?   [trophy] [skull] [star]
```

This triggers the post-game flow defined in spec 16:
- Trophy tap: posts a `moment` with sentiment `win`
- Skull tap: posts a `moment` with sentiment `loss`
- Star tap: shows text input, posts a `moment` with sentiment `highlight`
- 30-second timeout: dismiss, log `game_session_end` only

### 7.3 Idle State

When no game is detected:

```
[avatar]  Navigator    [voice controls]
          #001
```

Standard bottom bar with no game info.

### 7.4 State Transitions

```
Idle ──[game detected]──▶ Now Playing
                              │
                        [game exits, >= 5 min]
                              │
                              ▼
                         Post-Game ──[tap or 30s timeout]──▶ Idle

Now Playing ──[game exits, < 5 min]──▶ Idle (skip post-game)
```

---

## 8. Backend RPCs

### 8.1 Game Session End (Spec 16, already defined)

```go
// game_session_end RPC — already in crew_events.go (spec 16)
// Called when game process exits and duration >= 2 min
```

No new backend RPCs needed for game sensing. The existing `presence_update`, `crew_catchup`, and `game_session_end` RPCs handle everything.

### 8.2 Crew Recent Games (new, optional)

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

## 9. Slint UI Components

### 9.1 Game Badge

Reusable component for the game icon in sidebar and bottom bar:

```slint
component GameBadge inherits Rectangle {
    in property <string> short_name;
    in property <color> bg_color: #2a2a30;
    in property <length> size: 32px;

    width: size;
    height: size;
    border-radius: 8px;
    background: bg_color;

    Text {
        text: short_name;
        font-size: 11px;
        font-weight: 700;
        color: white;
        horizontal-alignment: center;
        vertical-alignment: center;
    }
}
```

### 9.2 Now Playing Bar Segment

```slint
component NowPlayingBar inherits HorizontalLayout {
    in property <string> game_name;
    in property <string> game_short_name;
    in property <color> game_color;
    in property <bool> can_stream;

    spacing: 10px;
    alignment: center;

    GameBadge {
        short_name: game_short_name;
        bg_color: game_color;
    }

    VerticalLayout {
        spacing: 2px;
        Text {
            text: "NOW PLAYING";
            font-size: 10px;
            color: #666666;
            letter-spacing: 1px;
        }
        Text {
            text: game_name;
            font-size: 13px;
            color: #cccccc;
            font-weight: 500;
        }
    }

    if can_stream: Rectangle {
        width: 80px;
        height: 28px;
        border-radius: 8px;
        background: #e8364e;

        Text {
            text: "STREAM";
            font-size: 11px;
            font-weight: 700;
            color: white;
            horizontal-alignment: center;
            vertical-alignment: center;
        }

        TouchArea {
            clicked => { /* start stream flow */ }
        }
    }
}
```

### 9.3 Sidebar Game Entry

```slint
component SidebarGameEntry inherits Rectangle {
    in property <string> game_name;
    in property <string> game_short_name;
    in property <color> game_color;
    in property <int> live_count;
    in property <int> recent_count;
    in property <bool> is_live;

    height: 56px;
    background: transparent;
    border-radius: 8px;

    HorizontalLayout {
        padding: 8px;
        spacing: 10px;
        alignment: start;

        GameBadge {
            short_name: game_short_name;
            bg_color: is_live ? game_color : #2a2a30;
            size: 40px;
        }

        VerticalLayout {
            alignment: center;
            spacing: 4px;

            Text {
                text: game_name;
                font-size: 13px;
                color: is_live ? #e8e8e8 : #666666;
                font-weight: 500;
            }

            HorizontalLayout {
                spacing: 4px;
                // Player dots rendered here
            }
        }

        // Player count on the right
        Text {
            text: live_count + recent_count;
            font-size: 13px;
            color: is_live ? #e8364e : #666666;
            vertical-alignment: center;
        }
    }
}
```

---

## 10. File Structure

### 10.1 New Files

```
mello-core/src/
├── game_sensing.rs      # Process scanner loop, GameEvent enum
├── game_db.rs           # GameDatabase, GameEntry, lookup
└── game_state.rs        # GameStateManager, event handling, UI/presence coordination

assets/
└── games.json           # Bundled game database (expanded from spec 14)

client/ui/components/
├── game_badge.slint     # Reusable game icon badge
├── now_playing.slint    # Bottom bar now-playing segment
├── post_game.slint      # Bottom bar post-game prompt
└── sidebar_game.slint   # Sidebar game entry row
```

### 10.2 Modified Files

| File | Change |
|------|--------|
| `backend/.../presence.go` | Add `game` field to presence struct, handle in `presence_update` |
| `backend/.../crew_state.go` | Compute `active_games` from member presences |
| `backend/.../crew_events.go` | Add `recent_games` to catch-up response (or new RPC) |
| `mello-core/src/presence.rs` | Add `GamePresence` struct, compound activity support |
| `client/ui/bottom_bar.slint` | Integrate now-playing and post-game states |
| `client/ui/sidebar.slint` | Add game list section |

---

## 11. Testing

### 11.1 Unit Tests

- Game DB lookup: case-insensitive exe matching, multiple exe variants
- Primary game selection: fullscreen preference, single game, no games
- Session thresholds: sessions under 2 min not reported, under 5 min skip post-game
- Sidebar merge: live + recent deduplication, sort order, cap at 8

### 11.2 Integration Tests

- Start game process, verify presence updates within 10 seconds
- Stop game process, verify `game_session_end` RPC called
- Verify crew state `active_games` reflects member game presence
- Verify catch-up `recent_games` aggregates correctly from ledger

### 11.3 Manual Test Cases

- [ ] Launch a game from `games.json`, verify NOW PLAYING appears in bottom bar
- [ ] Close game after 5+ minutes, verify post-game card appears
- [ ] Tap win/loss/highlight, verify moment appears in event ledger
- [ ] Ignore post-game card, verify it dismisses after 30 seconds
- [ ] Close game after < 2 minutes, verify no ledger event
- [ ] Close game after 2-5 minutes, verify ledger event but no post-game card
- [ ] Join voice while game is running, verify presence shows both voice + game
- [ ] Check crew sidebar shows game with live players highlighted
- [ ] Check crew sidebar shows recent games (no one playing) as dimmed

---

*This spec covers game detection, the game database, presence integration, crew sidebar game list, and bottom bar UI. For the video capture pipeline, see [14-VIDEO-PIPELINE.md](./14-VIDEO-PIPELINE.md). For the event ledger and post-game moments, see [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md). For presence and crew state, see [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md).*
