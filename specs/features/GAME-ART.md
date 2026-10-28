# GAME-ART

> **Component:** Client game art, art Worker, catalogue build
> **Status:** shipped
> **Depends on:** `specs/22-GAME-UI-SURFACES.md` (section 6.1)

The client shows art for each game in the lists, tiles and cards. The art comes from the
machine of the player when possible, and from the art Worker when not. This file defines the
slots, the order of the sources, the Worker interface and the client cache.

## 1. Slots

Each frame shape uses one art form. A game never shows in two forms side by side.
The per-surface table is in spec 22 section 6.1.

| Slot | Frame | Use |
|---|---|---|
| `icon` | Square, any size | Lists, pills, strips, small cards |
| `header` | Wide, 460:215 | Tiles in recap cards and the recent games grid |
| `hero` | Poster | The wide slot of the live session card |
| `capsule` | None | Not used in any frame |

`GameArt` enforces the rule. A frame with `art-width` set draws the `header` art and never the
`icon` art. A frame without it draws the `icon` art.

## 2. Resolution order

The first source that has the art wins. A frame with no art draws the monogram.

| Slot | 1 | 2 | 3 |
|---|---|---|---|
| `icon` | Exe icon shared by the crew | Worker `icon` | Monogram |
| `header` | Local Steam art | Worker `header` | Monogram |
| `hero` | Local Steam art | Worker `hero` | Monogram |

The client does not wait for the Worker. A frame draws the monogram until the image arrives.

## 3. Worker interface

The client reads from `https://art.m3llo.app`. A test sets `MELLO_ART_BASE` to another base.

| Request | Answer |
|---|---|
| `GET /<game id>/<slot>` | The image, with an `ETag` |
| `GET /health` | JSON with `status` set to `ok` |

- The game id has a store prefix and a store id: `igdb-<id>`, `steam-<id>`, `epic-<id>` or `gog-<id>`.
- The slot is `icon`, `header`, `hero` or `capsule`.
- A game with no art, or an id that has no match, gives 404. The answer may be cached for one day.
- A bad id, a bad slot or a wrong path gives 400.
- Other methods than `GET` and `HEAD` give 405.
- A request with a matching `If-None-Match` gives 304.

The Worker serves artwork from the game database provider under licence. It caches the images
for a short time at the edge and stores no image of its own. The About page credits the
provider (`specs/01-CLIENT.md`, ABOUT tab).

## 4. Client cache

The cache is in `mello-core/src/art.rs`.

| Rule | Value |
|---|---|
| Disk | One file for each game id and slot, with its `ETag` beside it |
| Revalidation | A file older than seven days is checked again with `If-None-Match` |
| Negative cache | A 404 or an error is kept in memory for 24 hours. The client does not ask again in that time. |
| Concurrency | At most four fetches in flight |
| Size limit | A body over 8 MB is refused |

## 5. Privacy

A read carries no user data: no account, no crew, no cookie. The request holds a game id and a
slot. The Worker logs counts of outcomes. It logs no ids and no addresses.

## 6. Catalogue build

`scripts/build_catalogue.py --export-art-index DIR` fetches the game database dumps and writes
the CSV files that the Worker index build reads. It reads the API credentials of the provider
from the environment. The step builds no catalogue files, and it exits when the CSV files are written.

## 7. Tests

- Unit tests in `mello-core/src/art.rs` cover the cache rules against a fake Worker.
- The journey `feed.stage-two` runs against a local Worker with a fixture index. It declares the
  need `art`, so the driver checks the Worker health first (`TESTING.md`).
