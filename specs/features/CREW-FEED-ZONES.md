# CREW-FEED-ZONES

> **Component:** Crew Feed (server `crew_feed`, client `CrewFeed`)
> **Status:** built (protocol 2)
> **Related:** `CLIPS.md` (clip cards, game clips), `HEAD2HEAD.md` (belts),
> `SEALED-REVEAL.md` (sealed belts and recap), `CREW-QUESTS.md` (the quest card),
> `19-FEED-CURATION-PERSONAL-STATS.md`
> (notability, recaps, the You strip), `22-GAME-UI-SURFACES.md` (game cards)

The crew feed is five fixed zones and Crew history. The server picks what
each zone holds. The client draws the zones and adds what is client state. A
zone with nothing to show collapses, and the zones under it move up. There is
no other layout rule.

## 1. Zones

| Zone | Holds | Shows when |
|---|---|---|
| Stage | One thing to watch: a live stream, the quest card of a new crew, a game clip, a stream replay, the belt hero, or the own chips of a crew of one | One of them exists |
| Board | The crew in numbers: the sealed weekly recap, or Today and Week with the belts | Always, for a crew with data |
| Clips | Every clip of the free window, game and voice mixed, in one carousel | A clip exists |
| People | The first five, the joins of the last 24 hours, voice | An open seat, a join, or voice to show |
| Sessions | Live sessions, then every game, voice and stream session of the free window, in one carousel | A session exists |
| Crew history | One row for each past week, newest first | Past weeks exist |

The feed has cards only. There are no headings or label lines outside a card.
The You strip (spec 19 section 2.3) stays pinned above the zones.

Red is state. A live stream on the Stage wears the red LIVE tag. A clip never
wears a red border; an unread clip keeps its red tag.

## 2. Stage

The Stage holds one card. The first of these that exists wins:

1. A live stream. It is client state (`crew_state`), so the client puts it on
   the Stage ahead of the payload. While you watch it, the Stage is at least
   as high as the watched card (spec 12 §7.9).
2. The quest card (`quests`), in a crew's first 14 days, until every step is
   done and every gift opened (`CREW-QUESTS.md`). It is beside the server's
   `stage` in the payload, so the client puts it ahead. While it shows, the
   server keeps the Stage entry in Clips too, and the own chips of a crew of
   one stand on the solo Board.
3. The newest game clip with a poster in the free window (`game_clip`).
4. The best stream replay in the free window (`stream_replay`).
5. The belt hero (`belt`): a move that involves the viewer, at most 14 days
   old, in a crew of two or more. The client shows it only once the belt is
   open: a belt that is sealed on the board keeps its new holder hidden
   (`SEALED-REVEAL.md`).
6. The own chips (`own_belts`): a crew of one. Up to four chips of the
   viewer's own values (`HEAD2HEAD.md`, "Solo card"), sealed on a new crew.
   An opened chip shows its title and value.

A voice clip is never on the Stage. The clip on the Stage is not repeated in
the Clips zone.

## 3. Board

The board is two columns wide. `mode` comes from the server; the client adds
the sealed state.

| Mode | Board |
|---|---|
| `sealed` (client) | The weekly recap the viewer has not opened, at its own size. A click opens it in the centre (`SEALED-REVEAL.md`). There is no Today card on that day: Today is a tab of the opened board, so opening the recap stays the occasion of the week. |
| `tabs` | Today and Week tabs, then the belts. Today: hangout, clips, sessions, the members who took part, the top game. Week: the numbers of the weekly recap. |
| `quiet` | No crew time last week: "No crew time last week." and who still played, then the belts. |
| `past` | Dormant, 14 days or more: the last real week with its age, then the belts. A dormant strip stands above the zones: "Nothing new since 2 October. Leo holds The Carry." |
| `solo` | A crew of one: the viewer's games. |

While something is live, a quiet or dormant feed is not quiet: the board
shows its tabs.

The belts are rows: the holder's octagon, the holder, the title and the
value. A belt that moved and is not opened is sealed: a muted octagon with
"?", and the sealed chip in place of the holder and value. A moved belt the
viewer has not seen wears the unread mark. Belts that moved or are sealed come
first. The board draws seven and counts the rest ("+6 MORE").

## 4. Clips

Every clip of the free window, game and voice in one paged carousel: the
server's order (game clips first, then newest), unread first. The carousel
ends in a Crew history tile when there are past weeks. A click on it scrolls
to Crew history.

## 5. People

- The first five: an octagon for the viewer, then the crew in roster order,
  an open seat for each free one, and the red Invite button while a seat is
  open. Crews have no member cap;
  the seats are the first five.
- Joins of the last 24 hours: "Elin, Noah and Saga joined", who invited and
  when, and Say hi. Say hi sends a greeting that mentions the member to the
  crew chat at once (CLIPS.md, "Say hi"). Its accessible name is "Say hi to
  Elin". After the send the button goes. It is once for each member, stored
  locally (`said_hi`).
- Voice: a call that is on (who is in it, Join), or on a quiet feed the empty
  default channel with the date of the last call and Start voice.

The parts share one card when they fit. Otherwise each part is a card, under
the one before it:

| Parts | Full width | Column beside the board |
|---|---|---|
| One part | One card | One card |
| The first five and another part | The first five, then a card with the rest | A card for each part |
| Joined and voice | One card | Two cards |

One card is 120 px high. A card in a stack of cards is 88 px high.

## 6. Sessions

Live sessions (client state from presence, `LiveSessionCard`) lead, then the
server's sessions newest first: game sessions (`GameSessionCard`), voice
sessions and streams without snapshots (`SessionCard`). A live session and the
session it ends in never show together. On a quiet week with no session the
zone shows the games most members have installed (`installed`).

## 7. Crew history and the free window

The free window is the last `free_days` days (default 7). It is a server
setting (`feed_config/config`, 1 to 90 days) that the server applies on every
feed load. Clips, sessions and streams younger than the window are free.
Everything older is Crew history, which is m3llo+.

- One row for each past week, newest first. A week is a UTC week from Monday.
- m3llo+ is per user, never per crew. Two members of one crew can see the
  same week differently.
- A viewer without m3llo+ gets a locked week: the sealed recap field, the
  counts and up to two teasers (who, clip or stream, length). The server sends
  no media, poster or snapshot URL for a locked week.
- A viewer with m3llo+ gets the week open: its numbers and its clips and
  streams.
- The founding week of a crew is open for every member once its quest step 4
  is done: the weeks that overlap the first 7 days of the crew
  (`CREW-QUESTS.md` section 3.4). Such a week carries `founding: true`.
- Without m3llo+ an m3llo+ strip stands above the weeks. Every m3llo+ action
  opens a "coming soon" modal until a purchase flow exists.
- History pages by week. Near the end of the feed the client asks for the
  page of weeks before `history_next` (`Command::LoadCrewHistory`). A week is
  never split across pages.

## 8. Payload

`crew_feed {crew_id, history_before?}` returns `{crew_id, zones}`. There is no
other feed RPC.

| Field | Content |
|---|---|
| `free_days` | The free window |
| `plus` | The viewer has m3llo+ |
| `stage` | `kind` and `entry` (section 2) |
| `board` | `mode`, `recap`, `today`, `last_activity_ms`, `players_last_week` |
| `head2head` | The head2head data of the crew for the viewer (`HEAD2HEAD.md` section 5): the belts, the pairs, the own values |
| `clips` | Every clip of the free window |
| `people` | `member_count`, `open_seats`, `joined` |
| `sessions` | Every session of the free window, newest first |
| `installed` | The most installed games, on a quiet week with no session |
| `history` | One page of past weeks |
| `history_next` | The `history_before` of the next page. Zero at the end |
| `quests` | The quest card of a new crew (`CREW-QUESTS.md` section 4). Absent for a crew with no quest record and after its 14 days |

An entry is `{id, type, ts, data}`. `type` is `clip`, `recap`, `daily-recap`,
`session-preview` or `session`. The client skips a type it does not know. A
card with no content is never sent (`cardHasContent`).

Protocol 2 is the zone feed. A client of protocol 1 reads a feed that no
longer exists, so the server's `min_client_protocol` is 2 and an older client
must update.

## 9. Client

| Part | File |
|---|---|
| Zones from the payload | `client/src/handlers/zones.rs` |
| Panel | `client/ui/panels/crew_feed.slint` (`CrewFeed`) |
| Zone parts | `client/ui/components/feed_board.slint` |
| Previews (every crew case) | `client/ui/previews/feed_board_preview.slint`, `scripts/render-feed-zones.sh` |
| State port | `client/src/e2e_state.rs`, `feed_zones` |

- The client draws the zones of the active crew only. Before the feed loads,
  the zones still draw the client state: a live stream, live sessions, voice.
- Every change of client state (an avatar, a live session, the roster, a belt
  that opened, game art) builds the zones again from the last feed. A poster
  on a card stays through a rebuild (`keep_posters`).
- The snapshot loader puts posters on the cards of the zones.

## 10. Tests

- Server: `crew_feed_zones_test.go` (each zone, the gate, paging, the
  config).
- Client: `client/src/flow_tests/zones.rs`, `flow_tests/sealed.rs`.
- Journeys: `qa/journeys/feed-*.ts` (FEED-03 to FEED-12 in `qa/flows.yaml`).
