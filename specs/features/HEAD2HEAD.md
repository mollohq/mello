# HEAD2HEAD

> **Component:** Crew Feed, Crew Memory
> **Version:** 1.0
> **Status:** shipping with feat/next-gen-feed (backend only; client rendering is the next task)
> **Depends on:** 16-CREW-EVENT-LEDGER.md (section 3.6, library documents), 19 (per-game stats), CLIPS.md (feed hero order)

---

## 1. Purpose

Head2head shows which crew member holds each title, such as the most hours in the top shared game. It gives a new crew a reason to open the feed before any session exists. It also shows the open seats, to push the crew to five members.

The server computes one holders doc per crew from data that exists now. The feed trims the doc for the viewer.

---

## 2. Titles

A title with fewer than two contenders is not contested. The server omits it.

On a tie, the previous holder keeps the title. With no previous holder, the lowest user id wins.

| Title id | Stat | Holder | Source | Value, unit |
|---|---|---|---|---|
| `the_carry` | Minutes in the top shared game | Most | `user_library` `playtime_min`, else `user_game_stats` `played_min_total` | hours, `h` |
| `locked_in` | Game minutes, 7 days | Most | `user_game_stats` `recent_days` | hours, `h` |
| `has_a_life` | Game minutes, 7 days | Fewest, with at least one session | `user_game_stats` `recent_days` | hours, `h` |
| `night_owl` | Latest local clock time of the last session | Latest | `user_game_stats` `last_played` and `last_tz_offset_min` | `"HH:MM"`, no unit |
| `the_collector` | Installed games | Most | `user_library` games | count, `installed` |
| `sampler` | Distinct games played, 14 days | Most | `user_game_stats` `recent_days` | count, `games` |
| `one_trick` | Share of lifetime minutes in one game | Highest | `user_library` and `user_game_stats` minutes | percent, `%` |

Rules per title:

- The top shared game is the game installed by the most members. A tie goes to the game with the larger total minutes. Only members with a minutes figure for that game contend.
- The 7-day window is the last seven UTC days, today included. The 14-day window is the last fourteen UTC days.
- `night_owl` ranks the clock from 18:00 local. So 01:15 is later than 23:30. Only sessions that ended in the last 14 days count. A member whose last session carried no `tz_offset_min` does not contend.
- `tz_offset_min` is an optional field on `game_session_end`. It is the UTC offset of the clock in minutes, east positive. The server ignores a value outside -840 to 840. The stats doc keeps the last value per game.
- `recentDaysCap` is 14, so `recent_days` holds 14 days.

Titles later: `mouth`, `marathon`, `last_one_up`, `clipper`, `rage_quit`, `winner`. They need the per-user stats doc of plan phase 5.

---

## 3. Holders doc

Collection `crew_head2head`, key `holders_<crew_id>`, owner is the system user.

Read and write permission are both 0, so only the server touches the doc. The feed RPC reads it as the system user after it checks crew membership, and returns only the trimmed payload for the viewer.

| Field | Meaning |
|---|---|
| `crew_id` | The crew |
| `members` | User ids with library or stats data, sorted |
| `titles` | List of contested titles (below) |
| `pairs` | One entry for every pair of `members` |
| `member_count` | Crew size when the doc was computed |
| `open_seats` | `max(0, 5 - member_count)` |
| `updated_at` | Unix milliseconds |

Title entry: `title_id`, `holder_user_id`, `value`, `unit`, `since_ms`, `previous_holder_user_id`, `moved_at_ms`.

- `value` is a number. For `night_owl` it is the string `"HH:MM"`.
- `since_ms` is when the current holder took the title.
- `previous_holder_user_id` and `moved_at_ms` are absent for a title that never changed hands.

### Pair tally

A pair entry is `{a, b, a_count, b_count}` with `a` before `b` in id order. The count of a member is the number of contested titles that member would hold if only the two members existed.

A title counts for a pair only when both members contend for it. The same tie rule applies.

A pair entry also has `values`: one `{title_id, a_value, b_value}` for each title that counts for the pair. `a_value` and `b_value` use the encoding of the title `value`. The list length equals `a_count + b_count`. The list is empty, never null, for a pair with no shared title.

---

## 4. Recompute

| Trigger | When | Debounce |
|---|---|---|
| Library report | After `library_report` stores the document, including an empty list | Never skipped |
| Session end | After `game_session_end` writes the stats and the ledger event, when `game_id` is set | Skipped when `updated_at` is under 60 seconds old |

Both triggers recompute every crew the user is in.

- The holder is unchanged: keep `since_ms`, `previous_holder_user_id` and `moved_at_ms`.
- The holder changes: set `since_ms` and `moved_at_ms` to now, and set `previous_holder_user_id` to the old holder.
- A title that was not contested and now is: `since_ms` is now. There is no move.
- A failure never fails the RPC. The doc is derived data.
- The feed RPC builds the doc once for a crew that has none.

A member who joins or leaves does not trigger a recompute. The next trigger corrects the doc. The feed payload counts members live.

---

## 5. Feed payload

The `this_week` section has one entry for every crew: `type` `head2head`, `id` `head2head`. It is not a ledger event.

| Field | Meaning |
|---|---|
| `crew_id` | The crew |
| `member_count` | Live crew size |
| `open_seats` | `max(0, 5 - member_count)`, live |
| `titles` | Every contested title |
| `pairs` | Only the pairs with the viewer. The viewer is `a`. The counts and the `values` follow the swap |
| `belt_moved_recent` | The title with the newest `moved_at_ms`, when under 24 hours old. Absent otherwise |
| `updated_at` | Doc time. Zero when no doc exists |

Role and size:

- When a belt moved in the last 24 hours, the entry is the hero (`hero`, `lg`). The hero order is: live stream, belt moved, stream preview from the last 24 hours, recap on Monday, best session.
- Otherwise the entry is `standard`, `lg`. It follows the hero and the recap. It never takes a filler slot.

The client decides if a card renders. A card with no titles and no open seats has no data.

### Deciding chip

Each pair row in the cell shows one chip: the title that decides the pair. The client picks it from `values`.

- The gap of a title is `abs(a_value - b_value) / max(a_value, b_value)`. For `night_owl` the values are minutes after 18:00.
- The title with the largest gap wins. A tie goes to the title that comes first in the display order.
- The chip shows the value of the member who would hold the title in the pair. Held days come from the crew holder, only when the crew holder is that member.
- A pair with no `values` shows no chip.

---

## 6. Later

- Session titles from the per-user stats doc: `mouth`, `marathon`, `last_one_up`, `clipper`, `rage_quit`, `winner`.
- The opened view with the round robin of all pairs.
- Push when a belt moves.
- A per-crew off switch.
- Recompute on member join and leave.
- The client sends `tz_offset_min` with `game_session_end`.
