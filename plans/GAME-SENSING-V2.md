# Game Sensing v2 — Design & Build Plan

> **Status:** Draft for review
> **Supersedes (on acceptance):** [specs/17-GAME-SENSING.md](../specs/17-GAME-SENSING.md) §2–§3
> **Builds on:** [18-GAME-TELEMETRY.md](../specs/18-GAME-TELEMETRY.md) (unchanged), [19-FEED-CURATION-PERSONAL-STATS.md](../specs/19-FEED-CURATION-PERSONAL-STATS.md), [16-CREW-EVENT-LEDGER.md](../specs/16-CREW-EVENT-LEDGER.md), [11-PRESENCE-CREW-STATE.md](../specs/11-PRESENCE-CREW-STATE.md)

---

## 1. What changes and why

v1 asks *"is this process one of my 25 games?"* and defaults to **ignore**. The product
needs it to ask *"is this a game, and which one?"* and default to **identify**.

That is not a bigger `games.json`. It is a different system:

| | v1 | v2 |
|---|---|---|
| Catalogue | 25 hand-mapped exes, `include_str!` into the binary | ~2,000 curated games bundled (57KB) + a 135k-entry appid→igdb_id index fetched at runtime (399KB) |
| Identity | exact case-insensitive exe basename | resolution ladder: launcher manifest → crowd mapping → PE metadata → user confirm → unresolved-but-tracked |
| Coverage source | maintainer effort | the user's installed library + other users' confirmations |
| Unknown game | ignored until manually tracked | **always recorded**, named/iconed as best we can, identity backfilled later |
| Concurrency | one game at a time | a set of active sessions |
| Session start | first scan that saw the process | real process creation time, persisted across client restarts |
| Icons/colors | 24 PNGs embedded at compile time, hand-picked hex colors | **the exe's own icon first** (local, instant, what the user recognises), then crew-shared / SteamGridDB / cover; colors derived at ingest |

### Locked decisions

| Decision | Choice | Rationale |
|---|---|---|
| Head coverage | **Curated exe mappings** for the popular non-Steam titles | Launcher-independent; ~50 entries buys the whole top-50 (§2.3) |
| Tail coverage | **Full scan** of installed launcher manifests, Steam first | Exact `install_path → igdb_id` before anything runs; kills exe-name guessing at scale |
| IGDB access | **Data dumps**, not the live API | Daily CSVs, 469MB, fetched in <60s. The live API is capped at 4 req/s — a full backfill would take ~a day and any re-derivation would be a multi-day job |
| Catalogue location | **Split: head bundled, index fetched, metadata lazy** (§2.2) | Installer grows ~57KB against an install already 2MB over budget. Identity still resolves offline; only display names need the network, once per game |
| Backend role | Ingestion, index hosting, lazy metadata, crowd mappings, asset proxy | Catalogue delivery is decoupled from app releases — no updater involvement, survives replacing Velopack |
| Crowd mappings | **Shared server table**, threshold-promoted | Coverage compounds with users, not with maintenance |
| Unresolved games | **Tracked anyway** | "ALL games they play we know about" — never drop a session for lack of a name |
| Telemetry (spec 18) | **Unchanged** | Adapters key on `game_id`; v2 preserves those ids |

---

## 2. Measured against the real dump (2026-08-15)

Everything below is from the actual dumps, scoped to **PC/Windows main games** —
not the raw 372k-row catalogue, which is mostly console ports, DLC and mods we
never need to match.

| Question | Answer |
|---|---|
| Dump availability | All 76 endpoints, refreshed daily 06:00 UTC. 469MB for our 9, downloaded in 57s |
| PC/Windows main games | **172,456** |
| ...with cover art | **167,234 (97.0%)** |
| ...with a **Steam** appid | **133,381 (77.3%)** |
| ...with any launcher id (Steam/Epic/GOG/MS/itch) | **144,593 (83.8%)** |
| Epic / GOG / Microsoft / itch individually | 2.8% / 3.3% / 4.2% / 8.9% |
| Ambiguous names within PC main games | 3,322 (1.98%) — and only **16** inside the top 5,000 by popularity |
| Games with any rating/follow signal | 16,778 (9.7%) — the tail is genuinely obscure |

Spot-check: all ten of our known Steam appids resolve to the right game with cover
art (730 → Counter-Strike 2, 570 → Dota 2, 1245620 → Elden Ring, …).

The 1.98% name-ambiguity figure (16 collisions in the top 5,000) means the
PE-metadata → fuzzy-search rung is far safer than the raw catalogue suggested;
scoped to PC games it can auto-accept a confident single match without a prompt
more often than §5.2 originally assumed.

### 2.1 The install budget is already blown

`C:\Users\bob\AppData\Local\m3llo\current` measures **102 MB** — over the <100MB
target in [00-ARCHITECTURE §2](../specs/00-ARCHITECTURE.md) before game sensing v2
adds a byte. `mello.exe` is 86MB of it, `onnxruntime.dll` 13.5MB, `silero_vad.onnx`
2.2MB. The release profile is already `lto = "thin"`, `codegen-units = 1`,
`strip = "symbols"`, so this is not stray debug info and there is no quick win
hiding in it.

**Consequence for this plan: the artifact budget is not "a few MB", it is "as close
to zero as we can get".** A naive full-metadata catalogue (10.8MB on disk / 5.8MB
shipped) is therefore rejected — see §2.2.

### 2.2 Artifact design — 630 KB, not 5.8 MB

The naive design shipped names for 173,494 games. But the client does not need
*names* for every game — it needs to know **which game an appid is**, because the
igdb_id is what keys sessions and stats. Display names and covers are needed only
for games the user actually plays: a few dozen, ever.

That splits cleanly into three tiers:

| Tier | Contents | Size | Delivery |
|---|---|---|---|
| **Curated head** | ~2,000 most-played games: name, slug, cover hash, short name, accent colour | **57 KB gz** | bundled in the binary (`include_bytes!`) — metadata only, no images |
| **Identity index** | `steam_appid → igdb_id` for **all 134,989** | **399 KB gz** → 1.03 MB on disk, mmap'd | downloaded from the backend, refreshed on version change |
| **Metadata** | name + cover for anything outside the head | a few hundred bytes each | lazy per-game on first sight, cached to disk forever |

**Installer grows by ~57 KB.** The 1.03MB index is fetched at runtime, so it never
enters the installer at all. Measured encodings for tier 2:

| Encoding | On disk | Transferred |
|---|---|---|
| Flat `(u32, u32)` pairs | 1.03 MB | 602 KB gz |
| **Delta + varint** | 528 KB | **399 KB gz** |

Ship the delta form over the wire, expand to the flat form on disk so it stays
binary-searchable in place under `mmap` — resident cost is page cache only.

**What the tail costs.** A game in the index but not the head resolves to the
correct igdb_id *instantly and offline*, so the session and stats keys are right
even with no network. Only the display name and cover need one lazy fetch, cached
permanently. A game in neither falls through to rungs 3–5 of §5.2, exactly as
before. Nothing about "we know every game they play" weakens.

Full-metadata alternatives, for the record — all rejected against the 102MB
reality: everything (5.2 MB gz), popularity > 0 / 16,933 games (517 KB), top
25,000 (763 KB), top 10,000 (301 KB).

### 2.3 Breadth is not play — the number that actually decides priority

Steam is 77% of the *catalogue*. That is a fact about how many titles exist, and
it says almost nothing about what a crew plays on a Tuesday night. Weighted by
actual play, the picture inverts:

**Of the 50 most-played PC games, 13 cannot be reached by a Steam library scan:**

| Launcher | Top-50 titles it exclusively carries |
|---|---|
| Riot | League of Legends, Valorant, Teamfight Tactics, Legends of Runeterra |
| Battle.net | Hearthstone, World of Warcraft, Overwatch 2, StarCraft II |
| Epic | Fortnite |
| Other standalone | Roblox, Genshin Impact (HoYoPlay), Minecraft (Mojang/MS) |

And the 74% that *do* carry a Steam appid overstates real coverage, because
having an appid is not the same as the user having installed it from Steam —
Rocket League and Fall Guys are delisted from Steam, Diablo IV and Call of Duty
are usually played through Battle.net.

The clinching check: **5 of our 9 shipped telemetry adapters** (`lol_live`,
`lor_local`, `hearthstone_log`, `minecraft_stats`, `sc2_client`) target games a
Steam-only scan would never resolve. We already invested in deep match telemetry
for exactly the titles a breadth-first plan would deprioritise.

### 2.4 The consequence: curate the head, scan the tail

These are two different jobs and they want two different mechanisms.

| | Head (~top 200 played) | Tail (the other 172,000) |
|---|---|---|
| Mechanism | **curated `exe_mappings`** | **Steam library path-prefix scan** |
| Launcher | irrelevant — matches the exe wherever it was installed from | Steam only, by construction |
| Cost | ~50 hand-mapped entries for the popular non-Steam titles; the Steam ones the scan already covers | one scanner, zero per-game effort |
| Effort scaling | doesn't scale, doesn't need to | scales to the whole catalogue |

The curated list only has to cover **popular non-Steam games** — Steam-installed
titles are handled by the path-prefix scan for free. That is roughly 40–60
entries, which is an afternoon's work plus verification, and it buys complete
top-50 coverage on day one regardless of launcher.

This also means the Epic/GOG/Battle.net/Riot **scanners** stay genuinely low
priority — not because those launchers don't matter, but because their handful of
popular titles is already covered by curated mappings, and their tail is small.
The scanners buy better install paths and display names, not coverage of games
people actually play.

> **Practical note for building the curated list:** exact-name lookup against the
> dump failed for four of the fifty — *Minecraft*, *Overwatch 2*, *Baldur's Gate 3*
> and *StarCraft II* all sit under different canonical names or editions in IGDB.
> The curated list must be built against `alternative_names` (already in our
> ingest set) with a human eyeballing each of the ~50 rows, not by fuzzy
> auto-matching. It is a one-time cost and worth paying carefully — these are
> precisely the games we cannot afford to get wrong.

---

## 3. Architecture

Identity resolution is **entirely local**. The network is only involved for things
that must be live: crowd mappings, cover images, and games newer than the shipped
catalogue.

```
┌──────────────────────────── CLIENT ─────────────────────────────┐
│                                                                 │
│  launcher manifests            ┌────────────────────────────┐   │
│  (Steam/Epic/GOG/MS/…)  ─────▶ │  LibraryIndex scanner      │   │
│                                │  install_path → appid      │   │
│                                └─────────────┬──────────────┘   │
│                                              ▼                  │
│  ┌────────────────────────┐    ┌────────────────────────────┐   │
│  │ head.bin  57KB bundled │◀───│  GameSensor                │   │
│  │ top ~2000, full meta   │    │  classify → path-prefix    │   │
│  ├────────────────────────┤───▶│  identify (no network)     │   │
│  │ appid_index  1MB mmap  │    └─────────────┬──────────────┘   │
│  │ 135k appid → igdb_id   │                  │ GameEvent        │
│  └───────────▲────────────┘                  ▼                  │
│              │ version check   ┌────────────────────────────┐   │
│  ┌───────────┴────────────┐    │  SessionTracker (multi)    │   │
│  │ metadata cache on disk │    │  real start, persisted     │   │
│  │ lazy, per game played  │    └─────────────┬──────────────┘   │
│  └───────────▲────────────┘                  ▼                  │
│              │                presence · ledger · telemetry     │
└──────────────┼───────────────────────────────┬──────────────────┘
               │ HTTP, decoupled from releases │
┌──────────────┴───────────────────────────────▼──────────────────┐
│                        NAKAMA BACKEND                           │
│  ingestion job ──▶ head.bin (repo) + appid_index.bin (served)   │
│  game_meta (lazy name/cover)  ·  game_asset proxy (covers)      │
│  exe_mappings (crowd, live)   ·  game_resolve fallback          │
└─────────────────────────────────────────────────────────────────┘
```

**The updater is not in this picture.** Catalogue freshness is deliberately
decoupled from app releases: a game launching next week should not require a
client release, and it should not depend on which updater we are using. That
sidesteps the Velopack/GitHub-release coupling entirely and survives replacing
the updater later.

---

## 4. Backend — the catalogue

Nakama Go modules already receive `db *sql.DB` on every RPC, so custom tables live in
the same Postgres instance. No new infrastructure.

The backend does **not** serve identity lookups — the client already has the answer
locally. Its three jobs are: build the shipped artifact, host the live crowd
mappings, and proxy cover images.

### 4.1 Ingestion

A scheduled job pulls the dumps (`GET /v4/dumps` → `GET /v4/dumps/{endpoint}` →
presigned S3 URL, valid 5 minutes) and rebuilds into a shadow schema, then swaps
atomically. Only the endpoints we need: `games`, `external_games`,
`external_game_sources`, `covers`, `alternative_names`, `platforms`, `genres`,
`game_types`, `franchises` — 469MB, ~60s to fetch.

Two operational notes from the dump docs:

- **`schema_version` changes when IGDB changes the data structure.** The job must compare it against the last ingest and fail loudly rather than silently importing a shifted schema.
- **Removed or replaced images survive 30 days** before deletion. The asset proxy (§8) therefore needs a refresh policy, not a write-once cache.

> **Open item (de-risked):** the docs' License section covers only code samples; the
> data terms live in our Data Partner agreement. Confirm redistribution terms for
> serving IGDB cover art through our own proxy. Since §8.2 makes the exe icon the
> primary asset, this now affects only large-surface cover art — cosmetic, not
> blocking.

### 4.2 Build the two artifacts

The ingest job emits two things, deliberately separated by how often they change
and how they are delivered (§2.2):

**`head.bin`** — ~2,000 curated games with full metadata, 57 KB gz. Committed to
the repo and `include_bytes!`'d into the binary. Changes when we re-curate, which
is rarely, so coupling it to app releases is correct.

**`appid_index.bin`** — `steam_appid → igdb_id` for all 134,989, delta+varint
encoded, 399 KB gz. Served by the backend and fetched at runtime. Never enters
the installer.

```
head.bin      [ records ][ strings ]      full metadata, bundled
appid_index   [ delta-varint pairs  ]     transferred; expanded on disk to
                                          flat (u32,u32) for in-place mmap search
```

Scoped to **desktop main games** (`game_type = 0`, platform 6 *or* 14): 173,494
rows. Console ports, DLC, bundles and mods are excluded — we never match against
them, and dropping them halves the artifact.

**macOS rides along for free.** 31,500 Mac titles exist, but 96.7% of them are
also on Windows, so including Mac adds **1,038 rows (+0.6%)**. One artifact ships
to both platforms; there is no reason to build or version them separately.

`appid_index.bin` is version-checked on startup and re-fetched when the backend
reports a newer build — no app release, no updater involvement. This is what spec
17 §3.6 promised for `games.json` and never delivered, and it holds regardless of
which auto-updater we ship.

### 4.3 Server-side catalogue table

The same data lands in Postgres for the fallback path and for building the artifact:

```sql
CREATE TABLE game_catalog (
    igdb_id       BIGINT PRIMARY KEY,
    slug          TEXT NOT NULL UNIQUE,     -- stable game_id (see §6 migration)
    name          TEXT NOT NULL,
    short_name    TEXT NOT NULL,            -- derived, see below
    cover_image   TEXT,                     -- IGDB image_id
    accent_color  TEXT,                     -- derived from cover art at ingest
    genre         TEXT,
    franchise_id  BIGINT,
    first_release DATE,
    popularity    REAL                      -- ranks the artifact; 9.7% of rows have any signal
);
CREATE INDEX ON game_catalog USING gin (name gin_trgm_ops);  -- fuzzy name search
```

Fuzzy name search stays server-side — the shipped artifact carries no search index,
which is most of why it packs so small.

`short_name` and `accent_color` replace the hand-maintained `SHORT_NAMES` / `COLORS`
dicts in [seed_games_db.py](../scripts/seed_games_db.py). Short names come from a
derivation pass (acronym of significant words, numerals preserved: "Counter-Strike 2"
→ "CS2") with a small curated override table for the ~100 titles where the derivation
reads wrong. Accent color is the dominant non-background color of the cover, computed
once at ingest.

### 4.4 Identity indexes

```sql
CREATE TABLE external_id_index (        -- the golden key
    platform    TEXT NOT NULL,          -- 'steam' | 'epic' | 'gog' | 'xbox' | 'battlenet' | 'riot'
    external_id TEXT NOT NULL,
    igdb_id     BIGINT NOT NULL REFERENCES game_catalog,
    PRIMARY KEY (platform, external_id)
);

CREATE TABLE exe_mappings (             -- crowd + curated
    exe_name       TEXT NOT NULL,       -- lowercased basename
    path_shape     TEXT NOT NULL,       -- normalized parent-dir signature, '' = any
    igdb_id        BIGINT NOT NULL REFERENCES game_catalog,
    source         TEXT NOT NULL,       -- 'curated' | 'crowd'
    confirmations  INT NOT NULL DEFAULT 0,
    rejections     INT NOT NULL DEFAULT 0,
    status         TEXT NOT NULL,       -- 'active' | 'pending' | 'blocked'
    PRIMARY KEY (exe_name, path_shape)
);
```

`path_shape` prevents a generic basename from claiming every install: `javaw.exe`
under `…/steamapps/common/Minecraft/` is Minecraft, `javaw.exe` under
`…/Program Files/Eclipse/` is not. Curated rows always win over crowd rows.

**Abuse handling:** crowd rows land as `pending` and need N distinct-user
confirmations (start at N=3) before promotion to `active`. A row whose rejections
outweigh confirmations flips to `blocked`. Curated overrides are the escape hatch.
Rate-limit submissions per user.

### 4.5 RPCs

All four are **off the hot path** — a game starting resolves locally against
`catalogue.bin` with no network at all.

| RPC | Purpose | Called when |
|---|---|---|
| `game_asset` | Cover proxy: `(igdb_id, size)` → PNG, cached server-side. Clients never hit the IGDB CDN directly. | first time a game's art is displayed |
| `exe_mappings_sync` | Delta-fetch `active` crowd rows since a cursor. Keeps the local mapping cache current between catalogue releases. | periodically, background |
| `game_mapping_submit` | A user's confirm/reject of an exe→game mapping. Feeds `exe_mappings`. | user taps TRACK or "not this game" |
| `game_resolve` | Fallback for a launcher id absent from the shipped artifact — i.e. a game released since the last catalogue build. Also serves fuzzy name search for the PE-metadata rung. | library scan finds an unknown appid |

`game_resolve` is a genuine fallback, not the workhorse it was in the previous
draft: with a daily-built artifact, a miss means the game is newer than the user's
last update.

---

## 5. Client — library index and resolution

### 5.1 Library scanners

New `mello-core/src/library/`, one module per launcher behind a trait:

```rust
pub trait LibraryScanner: Send + Sync {
    fn platform(&self) -> &'static str;
    fn scan(&self) -> Result<Vec<LibraryEntry>, ScanError>;
}

pub struct LibraryEntry {
    pub platform: &'static str,
    pub external_id: String,     // Steam appid, Epic catalog item, GOG id, Xbox PFN…
    pub install_path: PathBuf,   // the prefix every exe of this game lives under
    pub display_name: String,    // launcher's own name — fallback before resolution
}
```

| Launcher | Windows | macOS |
|---|---|---|
| Steam | `libraryfolders.vdf` → `steamapps/appmanifest_*.acf` | `~/Library/Application Support/Steam/steamapps/` — identical formats |
| Epic | `%PROGRAMDATA%\Epic\EpicGamesLauncher\Data\Manifests\*.item` | `/Users/Shared/Epic/UnrealEngineLauncher/LauncherInstalled.dat` |
| GOG | `HKLM\SOFTWARE\WOW6432Node\GOG.com\Games\*` | GOG Galaxy app support dir |
| Xbox / MS Store | package family names + install roots from the package registry | n/a |
| Battle.net | `product.db` under `%PROGRAMDATA%\Battle.net\Agent` | `/Users/Shared/Battle.net/Agent/product.db` |
| Riot | `RiotClientInstalls.json` | `/Users/Shared/Riot Games/` |
| — | — | `/Applications/*.app` bundle scan (see below) |

Steam ships first — [telemetry/steam.rs](../mello-core/src/telemetry/steam.rs) already
resolves libraries and app directories for GSI config installation, so the parsing is
proven, and the `.vdf`/`.acf` formats are byte-identical across platforms. The rest
follow in priority order.

**macOS gets a rung Windows doesn't have.** Every app is a bundle with an
`Info.plist` carrying a stable `CFBundleIdentifier` — a far better identity key
than a Windows exe basename, and it does not require a launcher manifest at all.
Scanning `/Applications` and `~/Applications` yields bundle id → app for
everything installed, launcher or not. Practically: on macOS, rung 3 (metadata)
is nearly as reliable as rung 1, which softens the loss from thinner launcher
coverage there.

`process_enum_macos.mm` already implements `enumerate_game_processes()` via
`NSWorkspace`, and `NSRunningApplication.launchDate` gives real process start
times directly — the macOS half of §6.2 is close to free.

The index is cached to `~/.mello/cache/library.json`, invalidated by manifest-directory
mtime, and refreshed on a filesystem watch of the launcher manifest dirs (plus a
periodic backstop). Scanning runs on a background thread at low priority after auth —
never on the startup path, which has a <3s budget.

### 5.2 Resolution ladder

First hit wins:

0. **Curated exe mapping** — the ~50 hand-mapped popular non-Steam titles (§2.4). Checked first precisely *because* it is launcher-independent: it catches Hearthstone whether it came from Battle.net, and League whether the Riot client installed it to a non-standard drive. Exact, tiny, shipped in the artifact.
1. **Library index → `catalogue.bin`** — process path has a prefix in the index → Steam appid (or other launcher id) → binary-search the resident appid index → name, cover hash, slug. Exact, offline, sub-millisecond, and handles a game's many shipping executables for free. This is the **tail** mechanism: 133k Steam titles at zero per-game cost.
2. **Crowd exe mapping cache** — local copy of `active` crowd rows, synced in the background.
3. **PE version metadata** — `FileDescription` / `ProductName` from the exe → `game_resolve` fuzzy search → auto-accept on a confident single match. Measurement supports leaning on this: only 1.98% of PC game names are ambiguous, and 16 of the top 5,000.
4. **User confirm** — the existing one-tap prompt ([callbacks/games.rs:76](../client/src/callbacks/games.rs)), now offering ranked candidates instead of just the exe's own name. Confirmation submits to `game_mapping_submit`.
5. **Unresolved-but-tracked** — the session is recorded regardless, under a provisional local id, named from PE metadata or window title, iconed from the extracted exe icon — which under §8.2 is the *primary* icon source anyway, so an unresolved game looks no worse than a resolved one.

Step 5 is the one that makes "ALL games they play we know about" literally true. When a
provisional id later resolves — the mapping gets promoted, or the user confirms — the
backfill rewrites both the local session history and the user's stats key.

### 5.3 Is-this-a-game classifier

A process inside a library-index path is definitively a game. Everything else runs the
classifier, whose job is to decide whether to bother a user with a confirm prompt:

- **Hard no:** existing exe denylist and path denylist ([game_sensing.rs:16](../mello-core/src/game_sensing.rs), [:205](../mello-core/src/game_sensing.rs)).
- **Positive signals:** engine fingerprints in the install dir (`UnityPlayer.dll`, `GameAssembly.dll`, `*-Win64-Shipping.exe`, `*_Data/` folders), exclusive-fullscreen presentation, sustained GPU usage, gamepad input, D3D/DXGI module load.
- **Debounce:** keep the existing 2-consecutive-scan rule, and never prompt more than once per exe per install.

The classifier only gates *prompting*. It never gates *tracking* — an unclassified
foreground process is still a candidate for step 5.

---

## 6. Client — the sensor and sessions

### 6.1 Multi-game sessions

`GameSensor` currently yields `Option<ActiveGame>` and churns start/stop pairs when two
games are open. v2 tracks a set:

```rust
pub struct SessionTracker {
    active: HashMap<u32 /*pid*/, GameSession>,   // every running game
    primary: Option<u32>,                         // foreground/fullscreen — drives presence
}
```

Every active game accrues its own session. `primary` (foreground, falling back to
fullscreen) is what presence and the NOW PLAYING bar display. `is_foreground` is
already plumbed through the FFI and currently unused by
[`pick_primary_game`](../mello-core/src/game_sensing.rs) — v2 uses it.

### 6.2 Honest durations

"ostkatt played Valorant for 4hrs" has to be true or the whole memory pitch erodes.

- **Real start time.** Add process creation time to `MelloGameProcess` (`GetProcessTimes` on Windows) — a small, additive change to the C ABI in [mello.h:537](../libmello/include/mello.h). Sessions then date from when the *game* started, not when Mello noticed.
- **Restart survival.** Persist active sessions to disk each scan. On startup, for each persisted session: if the pid is alive *and* its creation time matches, resume; otherwise close it out at the last recorded scan time.
- **Wall vs active.** Record both `duration_min` (wall) and `foreground_min`. AFK trimming uses `GetLastInputInfo`: a session with no input and no foreground time for a long stretch reports the trimmed figure. Feed copy uses wall time; the trim only guards the pathological "left it open overnight" case.

### 6.3 Scan cadence

Move to **event-driven with a polling backstop**: subscribe to Windows process
creation/termination events (WMI `Win32_ProcessStartTrace` or an ETW provider), keeping
a slow poll (30s) as a safety net. Detection becomes near-instant instead of up to 15s
late, and idle CPU drops. If event subscription proves unreliable in the field, fall
back to adaptive polling (5s for a minute after any change, 15s otherwise).

### 6.4 Presence — wire it up

`GamePresence` exists in Rust, the RPC accepts it
([presence.go:139](../backend/nakama/data/modules/presence.go)), and `crew_state.go`
already computes `active_games` from it — but nothing publishes it and
`Command::UpdatePresence` has no senders. v2 sends `{game}` / `{clear_game}` on session
start/stop. This alone lights up the crew sidebar's live game list, which today only
renders in dev-seeded data.

---

## 7. Data model and migration

**`game_id` stays a string slug**, and the slugs we already use *are* IGDB slugs
(`counter-strike-2`, `valorant`, `league-of-legends`), so existing ledger events,
`user_game_stats` keys, and all nine telemetry adapters keep working untouched.
`igdb_id` rides alongside as the numeric join key and finally gets populated — it is
currently hardcoded to `0` on every event
([crew_events.go:797](../backend/nakama/data/modules/crew_events.go)).

| Legacy | Migration |
|---|---|
| `games.json` (25 entries) | Its 29 exe patterns seed `exe_mappings` as `source='curated'` — they encode knowledge IGDB does not have. The file itself is deleted; `catalogue.bin` replaces it. |
| `custom-*` ids in user settings | Attempted resolution against the catalogue on first v2 launch; unresolved ones keep their id and stay local. |
| Embedded 24-PNG icon set | Deleted, along with `game_icons_gen.slint`. The exe's own icon (§8.2) beats a bundled set for the common case, and the curated head carries pre-baked icons for the rest. |
| `icon_url` / `cover_url` in `GameEntry` | Removed — never read at runtime today. |

**Co-play attribution.** `PlayerIDs` is a one-element stub today. With sessions carrying
`igdb_id` and timestamps, the backend can correlate overlapping crew sessions of the
same game into "you and kim played 2h of CS2" — the crew-feel payoff, and it needs no
new client data.

---

## 8. Assets — icons and names for everything

### 8.1 Icons and covers are different assets

Session cards, badges and the sidebar all want an **icon**: square, small, instantly
recognisable. IGDB does not publish icons — it publishes **covers** (portrait box
art), which is why v1 reached for SteamGridDB in
[download_game_icons.py](../scripts/download_game_icons.py). Treating "the game's
picture" as one asset is what made v1's asset story awkward.

| Asset | Shape | Used by |
|---|---|---|
| **Icon** | square, 128–256px | session cards, sidebar entries, badges, NOW PLAYING, HUD |
| **Cover** | portrait box art | large surfaces only — game profile, rich notable-session card |

Icons are the primary asset. Covers are a nice-to-have on the few big surfaces.

### 8.2 Icon ladder — the exe's own icon first

**The executable's icon is what the user already recognises.** It is what sits in
their taskbar and on their desktop, so it is the image that reads as "that game"
without a beat of thought. It is also local, instant, offline, needs no catalogue
entry, and costs no bandwidth.

Ladder, first hit wins:

1. **Extracted exe icon** — [`extract_exe_icon_rgba`](../client/src/platform/exe_icon.rs) already requests 128px via `SHDefExtractIconW` with an `ExtractIconExW` fallback; **bump the request to 256px**, since most modern games ship one. On macOS, `NSWorkspace.iconForFile:` on the `.app` bundle gives up to 1024px and is typically better than the Windows equivalent.
2. **Crew-shared icon** — another member already uploaded one for this `game_id` ([game_icons.go](../backend/nakama/data/modules/game_icons.go), built). Covers the case where a crewmate owns the game and you are only watching.
3. **SteamGridDB icon** — pre-baked for the curated head at ingest, not fetched live. This is the only source with real, curated *icons* at scale.
4. **Steam CDN** — the app's own icon hash where we have an appid.
5. **IGDB cover, centre-cropped** — a poor icon, but better than nothing.
6. **Coloured initials badge** — `GameBadge`, already built, always succeeds.

**Generic-host guard.** The running process's icon is wrong when the process is a
shared runtime host: `javaw.exe` shows Java's coffee cup, not Minecraft. Keep a
small denylist of such hosts, and for library-resolved games prefer the icon of
the launcher-declared primary executable in the install directory over the icon of
whatever process happens to be running.

### 8.3 Why this ordering is better than v1's

- **The tail gets first-class art.** An unresolved indie game gets a real, recognisable icon immediately — with no catalogue entry, no network, no confirm. "Every game looks right" stops depending on catalogue coverage at all.
- **Zero-latency and offline.** Rung 1 needs nothing but the file already on disk, so cards never render a placeholder that later pops.
- **It de-risks the IGDB terms question.** Cover-art redistribution (§4.1 open item) drops from blocking to cosmetic, because covers are no longer the primary asset. That removes a legal dependency from the critical path.
- **Less backend.** `game_asset` shrinks to serving covers for a few large surfaces plus pre-baked head icons, rather than being the source of every image in the UI.

**Canonicalisation.** Since multiple users can upload an icon for the same
`game_id`, the server keeps one canonical icon per game — first upload wins,
replaceable by a curated override — so a crew never sees two different pictures
for the same title.

Client caches under `~/.mello/cache/games/{game_id}/`. The 24 compile-time PNGs and
`game_icons_gen.slint` are deleted: rung 1 covers the common case better than a
bundled set ever could, and the curated head carries pre-baked icons for the rest.

---

## 9. Privacy

Scanning someone's whole installed library and reporting every game they run is a real
surface, and getting it wrong is a trust problem, not a bug. Requirements:

- **First-run consent**, separate for (a) library scanning and (b) sharing sessions with crews. Sensing works with either disabled.
- **Per-game hide** — excluded from presence, feed, and ledger; still counted in your own private stats. Discoverable from the game's own card, not buried in settings.
- **Invisible session** — a global "don't share what I'm playing right now" toggle.
- **Data minimization** — the library index never leaves the device. Resolution sends platform ids, not paths. Crowd mapping submissions send `exe_name` + a normalized `path_shape`, never a full user path (which can contain a username).

---

## 10. Build order

Each step is independently shippable and leaves the product better than it
found it. Named, not numbered — the ordering matters, the numbering does not.

| Step | Scope | Payoff |
|---|---|---|
| **Foundations** | Wire game presence; real process start times (libmello + FFI); session persistence across restarts; multi-game sessions | Sidebar goes live; durations become honest. No new infrastructure, no dependency on the rest. |
| **Catalogue pipeline** | Dump ingestion job + `schema_version` guard, Postgres tables, `catalogue.bin` packer, publish through the auto-updater | A daily-fresh 172k-game catalogue in the client |
| **Curated head** | ~50 hand-mapped `exe_mappings` for the popular **non-Steam** titles (Riot, Battle.net, Epic, Roblox, HoYoPlay, Mojang), seeded from the existing 25 in `games.json` | **The top 50 played games resolve on day one**, launcher-independent — including all five telemetry adapters a Steam scan would miss |
| **Steam library index** | Steam scanner, cached index, path-prefix resolution against `catalogue.bin` | The long tail: 133k titles resolve exactly, with zero per-game effort |
| **Sensor rewrite** | Classifier, unresolved-but-tracked, event-driven detection | "ALL games we know about" becomes true |
| **Assets** | Exe-icon-first ladder (§8.2), 256px extraction, macOS `.app` icons, canonical crew icon, drop the embedded PNG set | Every game looks right — including ones the catalogue has never heard of |
| **Surfaces** | Nightly session rollup card, co-play correlation, T0 stats in recap/profile | The crew-feel payoff |
| **Long tail** | Epic/GOG/Xbox/Battle.net/Riot scanners; crowd mappings with promotion thresholds and review queue | Better install paths and display names; then coverage compounds on its own |
| **Spec consolidation** | Rewrite specs 16–19 to describe what was actually built, and split them along the layer boundary (§13) | Cold AI sessions read one correct spec per concern instead of three overlapping drafts |

**Curating the head before scanning the tail is the correction from §2.3.** Curated mappings cover the
head cheaply and launcher-agnostically; the Steam scan covers the tail at scale.
Doing them the other way round would have shipped 133,000 obscure titles while
failing to detect Hearthstone.

Everything down to the sensor rewrite delivers the vision; the rest makes it
feel like the product in the tagline.

---

## 11. Stat tiers

The feed and cards must render across three tiers without ever showing an empty slot —
the same principle spec 19 §3.5 already establishes.

| Tier | Applies to | Data |
|---|---|---|
| **T0** | every game | session existence, honest duration, time of day, who else was playing, who was in voice, clips taken, streams hosted |
| **T1** | launcher-backed games | platform playtime, achievements, rich presence |
| **T2** | the nine spec-18 adapters | W/L, streaks, K/D, maps, per-match detail |

T0 is the tier that matters most strategically: it needs no cooperation from any game
publisher, it covers everything, and it is built from **m3llo's own signals** — voice,
clips, streams, crew presence. That is the tagline's "proof last night's clutch actually
happened," and no competitor has those signals to work with.

---

## 12. Risks

| Risk | Mitigation |
|---|---|
| IGDB terms may restrict proxying cover art | Largely de-risked by §8.2: icons come from the exe, not IGDB. Only large-surface covers are affected, and Steam CDN is a fallback. |
| IGDB changes the dump schema | `schema_version` is captured per endpoint at ingest; the job fails loudly on a change rather than importing a shifted schema |
| Data Partner access lapses | The artifact is built from dumps but *shipped* to clients — an access gap degrades freshness, not function. Existing installs keep working. |
| Classifier false positives prompt users about non-games | Prompting is gated hard (denylists + engine fingerprints + debounce); tracking is not gated, so a wrong classification costs a card, not a prompt |
| Library scan I/O on slow disks | Background thread, post-auth, cached with mtime invalidation; never on the <3s startup path |
| Crowd mapping poisoning | Distinct-user thresholds, curated override, rate limits, `blocked` state |
| Install-size budget (already 2MB over) | Installer grows ~57KB, not 5.8MB (§2.3). The 1MB index is fetched at runtime. The assets step *reclaims* space by deleting the embedded PNG set. The 86MB `mello.exe` is a separate problem this plan does not create or solve. |
| Catalogue staleness | The index is version-checked on startup and refreshed independently of app releases; `game_resolve` covers anything newer still. A miss is a round-trip, not a failure. |
| macOS launcher coverage is thinner than Windows | Bundle-id scanning of `/Applications` compensates: a stable `CFBundleIdentifier` is a better key than an exe name, and needs no launcher manifest |

---

## 13. Spec consolidation (do this last)

Specs 16–19 currently describe the same screens in three places, because 17 and 18
were both written before 19 existed and each grew its own UI section. The fix is
one rule:

> **If it renders, it's spec 19. If it detects or records, it's 16/17/18.**

### 13.1 The layer boundary

```
SENSE (17, 18) ──▶ RECORD (16 ledger, user_game_stats) ──▶ PRESENT (19)
```

Data flows one way. Nothing in 17/18 should know a feed card exists; nothing in 19
should care how a game was detected. The five contracts across those boundaries:

| # | From → To | Payload |
|---|---|---|
| 1 | 17 → presence (spec 11) | `game { igdb_id, name, started_at }` — live, ephemeral |
| 2 | 17 → 18 | `Started`/`Stopped { game_id }` — wakes the right adapter |
| 3 | 18 → 17 | `MatchResult[]` — accumulates into the open session |
| 4 | 17+18 → 16 | one `game_session` event at session end |
| 5 | 18 → `user_game_stats` | private streak store; only `streak_after` crosses into the public ledger |

### 13.2 What moves

| Spec | Action |
|---|---|
| **17** Game Sensing | Rewrite §2–§3 for v2 (catalogue, resolution ladder, library index, multi-session, honest durations). **Move §6 (sidebar game list), §7 (bottom bar now-playing/post-game), §9 (Slint components) → 19.** What remains: detect, identify, session facts, publish presence. |
| **18** Game Telemetry | **Move §6 "Crew-First Surfacing" → 19.** Reconcile §4.2 with shipped code (`Event::MatchEnded` carries `own_score`/`opp_score`, not `ct_score`/`t_score`; `SessionSummary` carries `draws`). Otherwise the most accurate of the four. |
| **16** Crew Event Ledger | Add `igdb_id` to `GameSessionData` (currently hardcoded `0`). Otherwise clean — it is the one spec already scoped to a single job. |
| **19** Feed Curation & Stats | Becomes the single home for all surfacing, absorbing the moved sections. Decides rollup-vs-per-session cards. |

### 13.3 Status headers — fix these early, not at the end

17, 18 and 19 all read `Status: Planned` while nine telemetry adapters, the You
strip, feed curation and the unknown-game prompt are shipped and running. That
actively misleads a cold session into distrusting or re-implementing working code.

Correcting the header block is metadata-only and costs minutes, so it should land
**first**, rather than waiting for the consolidation pass — otherwise every intervening
agent session reads the same wrong thing. Each spec gets an accurate `Status`, a
bumped `Version`, and where superseded, a one-line pointer to this plan.

---

## 14. Open questions

1. **Owned-but-not-installed** — the library index knows what the user owns. Surfacing that ("games your crew owns in common") is product scope beyond sensing. In or out?
2. **Presentation boundary** — spec 19 owns how sessions are surfaced (rollup vs per-session cards). This plan deliberately does not decide it; it only guarantees the data. See the spec-map discussion.
3. **Short-name overrides** — how many curated overrides are we willing to maintain? Derivation will get most, not all.
