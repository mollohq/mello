# MELLO Game UI Surfaces Specification

> **Component:** Control Bar Game States, Crew Sidebar Game List, Member Playing Line, Game Feed Cards, Game Icons
> **Version:** 0.1
> **Status:** Implemented. Section 7.1 (HUD overlay) is not built. Section 3.4 is built as a feed card, with a different data path.
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [17-GAME-SENSING.md](./17-GAME-SENSING.md), [18-GAME-TELEMETRY.md](./18-GAME-TELEMETRY.md), [16-CREW-EVENT-LEDGER.md](./16-CREW-EVENT-LEDGER.md), [19-FEED-CURATION-PERSONAL-STATS.md](./19-FEED-CURATION-PERSONAL-STATS.md), [11-PRESENCE-CREW-STATE.md](./11-PRESENCE-CREW-STATE.md)

---

## 1. Overview

This spec defines how game activity is drawn. It contains no detection logic and
no curation logic.

```
  SENSE                      RECORD                  CURATE          PRESENT
  ┌──────────────────┐      ┌─────────────────┐    ┌───────────┐   ┌──────────┐
  │ 17 Game Sensing  │      │ 16 Crew Event   │    │ 19 Feed   │   │ 22 Game  │
  │    which game,   │─────▶│    Ledger       │───▶│  Curation │──▶│    UI    │
  │    how long      │      │    game_session │    │  which    │   │  how it  │
  │                  │      │                 │    │  cards    │   │  looks   │
  │ 18 Telemetry     │      │    user_game_   │    │  survive  │   │          │
  │    W/L, streaks  │─────▶│    stats        │───▶│           │   │          │
  └──────────────────┘      └─────────────────┘    └───────────┘   └──────────┘
```

Spec 19 decides which cards exist. This spec defines what they look like.

| Question | Spec |
|---|---|
| Does this session get a card? | 19 |
| What does the card show without a W/L record? | 20 |

Specs 17 and 18 held UI sections before spec 19 existed. Those sections moved
here.

---

## 2. Control Bar Game States

File: `client/ui/panels/control_bar.slint`.

The centre region has three states. The game state manager sets the state. See
[17](./17-GAME-SENSING.md) section 4.

### 2.1 Now Playing

```
[game icon]  NOW PLAYING          [STREAM]
             Counter-Strike 2
```

The icon is the runtime-extracted executable icon. See section 6. The `STREAM`
button shows only when the client detects a hardware encoder.

### 2.2 Post-Game

The centre content changes when the game exits after `MIN_SESSION_POSTGAME_MIN`
minutes.

```
[game icon]  How'd it go?   [trophy] [skull] [star]
```

| Input | Result |
|---|---|
| Trophy | Posts a `moment` with sentiment `win` |
| Skull | Posts a `moment` with sentiment `loss` |
| Star | Opens text input, posts a `moment` with sentiment `highlight` |
| No input for 30 seconds | Dismiss. Log `game_session_end` only |

A telemetry adapter can pre-fill this card, for example `CS2 · 5W–3L · +2
streak`. The user then confirms with one tap. Games without an adapter use the
manual tap. See [18](./18-GAME-TELEMETRY.md).

### 2.3 Idle

```
[avatar]  Navigator    [voice controls]
          #001
```

### 2.4 Session thresholds

File: `mello-core/src/game_state.rs`.

| Constant | Value | Effect below this value |
|---|---|---|
| `MIN_SESSION_LEDGER_MIN` | 2 | No `game_session` event is recorded |
| `MIN_SESSION_POSTGAME_MIN` | 5 | The session is recorded, but no post-game prompt shows |

A 3-minute session reaches the ledger and the feed. It does not show the
post-game prompt. The code asserts `MIN_SESSION_POSTGAME_MIN >
MIN_SESSION_LEDGER_MIN` at compile time.

---

## 3. Crew Sidebar Game List

File: `client/ui/panels/crew_panel.slint`.

### 3.1 Data sources

| Source | Data | Use |
|---|---|---|
| Crew state `active_games` | Who plays what now | Green dots, player count, live indicator |
| Ledger `game_session` events | Who played what in the last 7 days | Entries stay when no member is online |

`crew_state.go` computes `active_games` from published game presence. The client
reads it as `crew_state.active_games`.

### 3.2 Item rendering

```
┌──────────────────────────────────────────────────┐
│  [CS]  Counter-Strike 2                     3    │
│        ●● ●                                      │
└──────────────────────────────────────────────────┘
```

| Element | Content |
|---|---|
| Badge | `short_name` on the game colour, or the runtime icon |
| Name | Full game name |
| Dots | Green for live players, grey for recent players |
| Count | Total unique players across live and recent |

An entry with no live players renders dimmed.

### 3.3 Per-member playing line

The member list shows one line below each crewmate:

```
▶ Valorant · 1h 23m
```

Functions: `format_game_line` and `game_lines_from_members` in
`client/src/converters.rs`. The source is `presence.game`. The client computes
the elapsed time from `started_at`. The server does not send formatted text.

The line refreshes on presence and voice events. The minute value advances on
the presence heartbeat. No dedicated timer is needed.

The line is empty when the member does not play a game, or when the member
disabled activity sharing. See section 5.

### 3.3a Live session card

The crew feed shows a game in progress as a `LiveSessionCard`. See
[CLIPS.md](./features/CLIPS.md) section 7.1 for the placement rule.

| Source | Data |
|---|---|
| `presence.game` of a crewmate | Game id, game name, `started_at` |
| `GameDetected` of the viewer | Same three values from the local sensor |

The `presence_change` push carries `game`, so the card needs no crew state load.
The client sends no new data. The card is never a ledger event.

### 3.4 Recent games data

The recent games card gets its data from the `crew_feed` RPC. Entry type: `recent-games`. No `recent_games` field on `crew_catchup` and no `crew_recent_games` RPC exist.

```json
{ "games": [ { "game_id": "valorant", "game_name": "Valorant", "launcher": "steam",
               "minutes": 600, "last_played": 1760000000000, "member_ids": ["<user id>"] } ],
  "session_count": 14, "total_min": 1860 }
```

- Source: the library documents of the crew members plus the `game_session` events of the week. Builder: `buildRecentGames` in `crew_feed.go`. Spec [19](./19-FEED-CURATION-PERSONAL-STATS.md) section 3.3 has the rules.
- Library documents: one per member, in the `user_library` collection. The client writes them with `library_report`. See spec [16](./16-CREW-EVENT-LEDGER.md) section 3.6. The feed RPC reads the documents of the crew members as the system. A member with no document adds nothing.
- Merge: the union of both sources by game id. A library game with no matching id falls back to a match on the lower-case name, because sessions carry catalogue ids and the library carries launcher ids.
- Name: the library name when present, else the session name.
- `minutes`: the summed `playtime_min` of the members when present, else the session minutes.
- `last_played`: the newer of the library and the sessions, in milliseconds. The tiles sort by it, newest first.
- `member_ids`: members who have the game installed or played it in the week. The client card does not draw them yet.
- The header line comes from sessions only. With no sessions the line is empty.
- Tiles: at most six, keyed by game id. A legacy event with no id uses the lower-case game name.
- `launcher`: the launcher prefix of the library entry (`steam`, `epic`, `gog`). Absent for a game only seen in sessions.
- Art ladder, first hit wins: the runtime game icon (cached or crew-shared), then the local Steam header art, then the monogram.
- Local Steam art: `mello_core::library::local_steam_art` reads `<steam root>/appcache/librarycache/`. It tries `<appid>_header.jpg`, then `header.jpg` and `library_header.jpg` in `<appid>/` and in each hash folder under it, then `library_600x900.jpg`. It reads the disk only. Nothing is fetched or uploaded. The log carries counts only.
- Monogram: two letters, the first letters of the first two words of the name (one word: its first two letters), on a two-corner tile in an identity hue chosen from the game id.
- Client: `RecentGamesCard` in `crew_feed.slint`, one row per game, at most three rows. Each row has the art (76 px wide, the shape of a Steam header), the name in Oxanium, and under it the hours in capitals ("4,167 H"; one decimal under 100 hours). With no hours known it shows the launcher ("EPIC"). The right side of the row shows "TRACKED" when the game sensor catalogue knows the game (by game id, Steam appid or name). Otherwise it shows nothing.
- Header: "RECENT GAMES" on the left. The right side is the sessions line (`subtitle`), or empty. It never says where the games come from.
- The sidebar list still uses live presence only.

---

## 4. Game Feed Cards

File: `client/ui/panels/crew_feed.slint`.

Spec [19](./19-FEED-CURATION-PERSONAL-STATS.md) selects which cards reach the
feed. This section defines what they render.

### 4.1 `GameSessionCard`

The card has two bodies. The presence of a telemetry record selects the body.

| Component | Condition | Content |
|---|---|---|
| `GameSessionRecordPanel` | `wins + losses + draws > 0` | W/L block, streak, duration, co-players |
| `GameSessionCompactBody` | No record | Icon, "ostkatt played Valorant", duration footer, co-player avatars |

A card must not render an empty stat slot. See spec 19 section 3.6. Before this
split, the card assumed a record and drew empty W/L boxes for all games without
an adapter.

### 4.2 `GameRollupCard`

One `CREW PLAY` card per crew per day. It aggregates the sessions that curation
removed.

| Element | Content |
|---|---|
| Member line | `ostkatt · Valorant · 4h`, rendered by `GameRollupLineRow` |
| Footer | Session count and total crew hours |

### 4.3 Duration copy — the overnight guard

A surface must not show a play time that the crew can disprove.

A game left open overnight has correct wall time and low active time. Each
surface applies the same rule:

- Show wall time.
- Show active time instead when active time is less than one third of wall time.

Three implementations apply this rule. Keep them in step.

| Scope | Function | File |
|---|---|---|
| One session | `game_session_duration_display` | `client/src/handlers/clip.rs` |
| One week | `weekly_time_text` | `client/src/handlers/stats.rs` |
| Recent games total | `reportableMinutes` | `backend/nakama/data/modules/crew_feed.go` |

The rollup implementation was added last. Before it, a 10-hour overnight session
showed 40 minutes on its own card and added 10 hours to the `CREW PLAY` card
above it. The recent games card replaced that card and keeps the same rule.

### 4.4 Co-play copy

Co-play copy, for example "you and kim played 2h of CS2", must use
`player_overlap_min`. It must not use the actor's `duration_min`. The actor's
duration does not state how long another member was present.

No surface renders this copy. The ledger records the data. See
[16](./16-CREW-EVENT-LEDGER.md) section 2.

---

## 5. Activity Sharing Consent

Onboarding shows a checkbox at the nickname step: "Show crew what I play".
The default is on. Settings → Games changes it later.

The flag gates every surface in this spec.

| Layer | Gate |
|---|---|
| Presence publish | `game_presence_to_publish(share, current)` in `mello-core/src/client/game_services.rs` |
| Session sharing | `should_emit_game_session_end(share)` in the same file |
| Persistence | `settings.share_game_activity`, default from `default_share_game_activity()` |
| Runtime change | `Command::SetShareGameActivity` |
| Library report | `LibraryReporter::plan` in `mello-core/src/library_report.rs` sends only when the flag is on and the session is active |

The client clears published presence on the next scan when the user disables the
flag. `sync_game_presence` recomputes the value and detects the change.

### Per-game hiding

The user can hide single games from the crew. The setting is `hidden_games` in
the client settings: a list of game ids (`<prefix>-<external_id>`, spec
[17](./17-GAME-SENSING.md) section 3.3). Settings → Games shows one
"Hidden from crew" switch for each installed game.

| Rule | Behavior |
|---|---|
| Library report | The client leaves every hidden game out of `library_report`. The server also drops an entry that carries `hidden: true` |
| Change | A change sends a new report at once, without the game |
| Steam default | A game hidden in the Steam library is hidden from the crew by default. The row shows "HIDDEN IN STEAM" and has no switch. To share it, unhide it in Steam |
| Sessions | Hiding a game affects the library report only. The presence line and `game_session_end` still follow the global toggle |
| Toggle off | The client sends an empty report once. The server document is empty |

The consent copy is the same on both surfaces.

| Surface | Text |
|---|---|
| Onboarding | "Show crew what I play". Subline: "Your installed games, play sessions and what you're in right now." |
| Settings, ACTIVITY SHARING | "Let crewmates see your installed games, what game you're in, and your play sessions in the crew feed." |

---

## 6. Game Icons

The client extracts icons from the running executable. It does not bundle them.
A bundled set cannot cover a catalogue of 137,688 games.

| Platform | Mechanism | State |
|---|---|---|
| Windows | `SHDefExtractIconW` in `client/src/platform/exe_icon.rs` | Verified |
| macOS | ICNS chunk parsing | Written. Never run on a Mac |

Icons are 256px. A 256px icon is about 84KB. The backend limit is
`gameIconMaxBytes = 256 * 1024` in
`backend/nakama/data/modules/game_icons.go`. An earlier 48KB limit rejected
these icons. Keep `exe_icon.rs` and this constant in step.

The icon reaches the UI as `game-runtime-icon` and `game-has-runtime-icon`,
through `control_bar.slint` and `main.slint`. Surfaces show the `short_name`
badge on the game colour when extraction fails.

---

## 7. Slint Component Reference

| Component | File | Purpose |
|---|---|---|
| `GameSessionCard` | `crew_feed.slint` | Session card shell |
| `GameSessionRecordPanel` | `crew_feed.slint` | W/L body |
| `GameSessionCompactBody` | `crew_feed.slint` | Body without telemetry |
| `RecentGamesCard` | `crew_feed.slint` | Recent games: header line and tiles |
| `RecentGameTileCell` | `crew_feed.slint` | One game tile |
| `PlayingBadge` | `crew_feed.slint` | Live playing marker |
| `LiveSessionCard` | `crew_feed.slint` | NOW PLAYING card of the crew feed: pip, game art, game name, octagons, "since N min" |

`client/ui/types.slint` declares the types. Both member row structs carry
`game-line`.

### 7.1 HUD overlay — NOT BUILT

A live round and score line during a match was designed for
`client/src/hud_manager.rs` and `HudState`. It is not built.

---

## 8. Testing

Follow `TESTING.md`.

- Use the headless harness in `client/src/testkit.rs` for UI behavior. It drives
  the real `callbacks::wire_all` and `handlers::handle_event`. Inject an `Event`
  and assert on `MainWindow`. Invoke a callback and assert on the emitted
  `Command` values.
- Assert on structure, not on accessibility. These panels declare almost no
  `accessible-role`, so `accessible_enabled()` reads as absent on a correct
  screen. Query component type names.
- Unit tests cover the three duration functions in section 4.3, including the
  overnight case.

No surface in this spec has run against a live backend with real crew traffic.

---

## 9. Out of Scope

- Deep profile view. See spec 19 section 2.4.
- Rich telemetry session card beyond the W/L panel in section 4.1.
- The HUD overlay (section 7.1).
