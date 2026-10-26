# HEAD2HEAD

> **Component:** Crew Feed, Crew Memory
> **Version:** 1.1
> **Status:** shipping with feat/next-gen-feed (backend only; client rendering is the next task)
> **Depends on:** 16-CREW-EVENT-LEDGER.md (section 3.6, library documents), 19 (per-game stats and recap), CLIPS.md (feed hero order)

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
| `one_trick` | Share of lifetime minutes in one game. Only members with at least two games with playtime and at least ten hours in all contend. A member with one game has no value | Highest | `user_library` and `user_game_stats` minutes | percent, `%` |
| `mouth` | Voice minutes, 14 days | Most | `user_activity_stats` `voice_min` | minutes, `min` |
| `marathon` | Longest single game session | Longest, lifetime | `user_activity_stats` `longest_session_min` | minutes, `min` |
| `last_one_up` | Latest local clock time of a session end, 14 days | Latest | `user_activity_stats` `latest_end_local_min` | `"HH:MM"`, no unit |
| `clipper` | Clips posted | Most, lifetime | `user_activity_stats` `clips` | count, `clips` |
| `rage_quit` | Game sessions of 2 to 10 minutes, 14 days | Most | `user_activity_stats` `short_sessions` | count, `sessions` |
| `winner` | Win moments posted, 14 days | Most | `user_activity_stats` `win_moments` | count, `wins` |

Rules per title:

- The top shared game is the game installed by the most members. A tie goes to the game with the larger total minutes. Only members with a minutes figure for that game contend.
- The 7-day window is the last seven UTC days, today included. The 14-day window is the last fourteen UTC days.
- `night_owl` ranks the clock from 18:00 local. So 01:15 is later than 23:30. Only sessions that ended in the last 14 days count. A member whose last session carried no `tz_offset_min` does not contend.
- `tz_offset_min` is an optional field on `game_session_end`. It is the UTC offset of the clock in minutes, east positive. The server ignores a value outside -840 to 840. The stats doc keeps the last value per game.
- `recentDaysCap` is 14, so `recent_days` holds 14 days.

Rules for the session titles. They read the activity doc of each crew member (section 3.1).

- A member contends for a title only with a figure above zero. A member with no figure does not count.
- The counters in the activity doc are per user, not per crew. A member has the same figures in every crew.
- `last_one_up` ranks the clock from 18:00 local, like `night_owl`. It reads the session ends of the last 14 days. A session end without `tz_offset_min` has no clock and does not count.
- `mouth` counts a member's own minutes in voice (section 3.1). The 14-day window is the last fourteen UTC days.
- `marathon` and `clipper` use the lifetime counters. They do not expire.
- A short session is a game session of 2 to 10 minutes, ends included.
- A win moment is a moment posted with the sentiment `win`.

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

### Size

The pair count grows with the square of the crew. Measured with 13 values for each pair (`head2head_scale_test.go`):

| Members | Pairs | Compute | Doc | Feed payload |
|---|---|---|---|---|
| 50 | 1,225 | under 10 ms | 0.8 MB | 35 KB |
| 100 (the member list cap) | 4,950 | under 10 ms | 3.3 MB | 69 KB |

The doc is not capped. A crew of 50 reads and writes it in well under one second. The feed returns only the viewer's pairs. A test fails when 50 members take over one second or over 1 MB.

### Pair tally

A pair entry is `{a, b, a_count, b_count}` with `a` before `b` in id order. The count of a member is the number of contested titles that member would hold if only the two members existed.

A title counts for a pair only when both members contend for it. The same tie rule applies.

A pair entry also has `values`: one `{title_id, a_value, b_value}` for each title that counts for the pair. `a_value` and `b_value` use the encoding of the title `value`. The list length equals `a_count + b_count`. The list is empty, never null, for a pair with no shared title.

---

### 3.1 Activity doc

Collection `user_activity_stats`, key `stats`, owner is the user. Read and write permission are both 0, so only the server touches the doc. Head2head reads it as the system user and publishes only the title holders, never the counters.

The doc has lifetime counters and 14 daily buckets. A bucket key is a UTC date.

| Field | Lifetime | Per day | Written by |
|---|---|---|---|
| `voice_min` | Yes | Yes | Voice leave |
| `game_min` | Yes | Yes | `game_session_end` |
| `sessions` | Yes | Yes | `game_session_end` |
| `short_sessions` | Yes | Yes | `game_session_end` |
| `longest_session_min` | Yes, the maximum | Yes, the maximum | `game_session_end` |
| `latest_end_local_min` | Yes, with `latest_end_date` | Yes | `game_session_end`, when `tz_offset_min` is valid |
| `win_moments` | Yes | Yes | `post_moment` with the sentiment `win` |
| `clips` | Yes | Yes | `post_clip` |

- Every write drops the buckets older than 14 days, today included. The lifetime counters stay.
- `latest_end_local_min` is the minute of the local day (0 to 1439). The latest end is the one latest from 18:00, so 01:15 is later than 23:30. `latest_end_date` is the local date of that session.
- A session of under one minute adds nothing. A duplicate `game_session_end` (same `session_id`) adds nothing.
- Every `game_session_end` counts, with or without `game_id`.

### 3.2 Voice minutes

The server keeps the join time of each voice member (`joined_at`). A reconnect to the same channel keeps it. On leave, the server adds the whole minutes between `joined_at` and the leave time to `voice_min`.

- A member counts only when the voice session had two or more members at some time. Time alone in a channel nobody joined does not count.
- A guest has no stats doc and adds nothing.
- One stint is capped at 24 hours.
- Every leave path uses the same bookkeeping: the leave RPC, a disconnect, a channel delete, a crew exit, the voice GC and the SFU reconcile.
- The voice GC passes the last heartbeat of a ghost member as the leave time. So the ghost's minutes, and the `voice_session` duration when the ghost is the last member, end at the last sign of life and not when the GC finds it.
- The leave path also finds a member that sits in a room with no user to channel entry. Before this, the GC could not remove it.
- The crew `voice_session` event is unchanged. It is written once, when the last member leaves a session of two or more.

## 4. Recompute

| Trigger | When | Debounce |
|---|---|---|
| Library report | After `library_report` stores the document, including an empty list | Never skipped |
| Session end | After `game_session_end` writes the stats and the ledger event | Skipped when `updated_at` is under 60 seconds old |
| Clip post | After `post_clip` stores the clip and counts it | Same as session end |
| Win moment | After `post_moment` with the sentiment `win` | Same as session end |
| Voice leave | After a leave adds voice minutes. Recomputes the crew of the voice channel | Same as session end |
| Recap | When the weekly recap is generated for a crew with activity | Never skipped |

The library, session end, clip post and win moment triggers recompute every crew the user is in.

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
| `own_values` | The viewer's own value in every title their data can compute, contested or not. Each entry has `title_id`, `value`, `unit` and `display`. A title with no data for the viewer is absent. Never null |

`own_values` is computed from the viewer's library, game stats and activity doc at read time. It does not use the holders doc. It lets a crew of one show chips. The `display` field is the value as text: whole hours from 100 up, one decimal below, thousands separated, `HH:MM` for the two clock titles.

Role and size:

- When a belt moved in the last 24 hours, the entry is the hero (`hero`, `lg`). The hero order is: live stream, belt moved, stream preview from the last 24 hours, recap on Monday, best session.
- Otherwise the entry is `standard`, `lg`. It leads a feed with no hero. Behind a hero it takes the first wide slot after it. It never takes a filler slot.
- The card is always two grid columns wide, at every crew size. There is no one-column form (`feed_grid.rs`, `place_wide_cards`). A crew of one has the chips (section "Solo card"), two or more have the pair rows.
- A quiet feed has no hero (CLIPS.md 7.1, frame E). The client then leads with the newest belt move that involves the viewer, up to 14 days old, and the head2head card is the wide card under it. The payload does not change: the client reads `moved_at_ms` and `previous_holder_user_id` of every title, and the values of the pair. The card has its own headline: "Sara took Night Owl from Bob".

The client decides if a card renders. A card with no titles, no `own_values` and no open seats has no data.

### Solo card

A crew of one gets the wide card (frame A of the mockup). The card shows the viewer as an octagon with the full name, then four chips in a 2 by 2 grid, a keyline divider, "Go head2head with your crew", four open seats each labelled "Open", and the red invite button.

- The chips come from `own_values`: the first four in display order (The Carry, Night Owl, The Collector, Locked In, Has a Life, Sampler, One Trick). They wear the viewer's identity hue. There is no holder octagon.
- On a new crew the chips are sealed. A click opens one in the centre of the feed, with the reveal of a belt that moved and the viewer as "You" (plans/SEALED-REVEAL.md section 9).
- The mockup also shows an "Old Guard" chip (the year of the Steam account). The client has no Steam account year, so the card does not show it. The next title in the display order takes its place.

### Pair rows on the wide card

The card shows the viewer's closest pairs (smallest gap first) in two columns, up to four. The first column fills first: two members show one pair on each side, three pairs show two and one, five members show two rows of pairs side by side (frame D). A "+N more" line shows only when pairs remain, from six members. Under five members the card also has the deciding chips, the open seats and the red invite. From five members the seats and the button are gone, and the invite card stands beside the head2head card, for 5 to 9 members. From 10 members there is no invite card.

### Unread dot on a pair row

- A pair row is unread when a title held by the viewer or by the other member has `moved_at_ms` less than 24 hours old and after the last time the viewer had the crew feed open.
- An unread row shows a small red dot before the "You vs Name" label (red is state, "unread"). The client derives this from `titles`. The payload has no field for it.
- Unread rows sort first. The other rows keep the closest-tally order.
- The versus hero shows only when the moved title involves the viewer, as holder or as previous holder. A move between two other members has no hero. The wide card carries it as the dot.
- The client stores the time of the last feed view for each crew, locally. Opening a crew takes the stored time as the baseline for that visit, and every feed load of the visit updates the stored time. The dot shows for the whole visit and clears on the next open.
- The state port reports `head2head.pair_rows_dot`: the number of pair rows with the dot.

### Deciding chip

Each pair row on the card shows one chip: the title that decides the pair. The client picks it from `values`.

- The gap of a title is `abs(a_value - b_value) / max(a_value, b_value)`. For `night_owl` the values are minutes after 18:00.
- A title where both values are equal, or the gap is zero, decides nothing and is skipped.
- Of the rest, the title with the largest gap wins. A tie goes to the title that comes first in the display order.
- When every contested title of the pair ties, the row shows the first contested title in the display order, without the deciding emphasis: the chip loses the holder's hue (`chip_ties`).
- Rows pick from the top down. A title that is the deciding chip of a higher row is skipped when the pair has another contested title. When every deciding title of the pair is used, the largest gap wins. This keeps a count title with small values (6 against 1) from deciding every row.
- The chip shows the value of the member who would hold the title in the pair. Held days come from the crew holder, only when the crew holder is that member.
- A pair with no `values` shows no chip.

---

## 6. Recap sections

The weekly recap (spec 19 section 3.5) has two belt sections. The recap job reads them from the holders doc when it generates a recap.

- The job recomputes the doc first, with the library trigger, so the debounce cannot leave it stale.
- A recap with no activity has neither section. Holding a title is a standing state and does not count as activity.

| Field | Content |
|---|---|
| `belts_moved` | Titles whose `moved_at_ms` is in the recap week. Each entry has `title_id`, `holder_user_id`, `holder_name`, `previous_holder_user_id`, `previous_holder_name`, `value`, `unit`, `moved_at_ms` |
| `belts_held` | One entry per member who holds a title, most titles first, ties by user id. Each entry has `user_id`, `display_name`, `count`, `title_ids` |

- A title that changed hands twice in the week appears once, with the last holder and the last previous holder.
- The recap fields that the RecapCard already exports are filled from these. `awards` gets one entry of kind `belt` for each moved belt, after the existing game awards. `leaderboard` has the same entries as `belts_held`.
- The `belt` award has `title` (the English title name), `name` (the new holder) and `detail` (`took it from <name>`, absent without a previous holder).

## 7. Pushes

A belt move sends a push to the previous holder. A new member sends a push to the crew. Both use the push queue and sender of the weekly recap push. The delivery rules below are the contract; the Worker accepts the types `belt` and `crew_join` with the shape of a mention. Collapse ids are `belt-<crew>-<recipient>` and `join-<crew>-<day>`, with the ids shortened to 12 hex characters to stay under the 64 bytes that APNs allows.

| Push | Trigger | Copy |
|---|---|---|
| Belt moved | A recompute changes the holder of a title | To the previous holder: `<Name> took <Title> from you. You hold <n>.` To the new holder, when they did not cause the recompute: `You took <Title> from <Name>. You hold <n>.` |
| Friend landed | A member joins the crew | Members 2 to 5: `<Name> joined <Crew>. Say hi.` Member 6 on: `<Name> joined <Crew>.` |

- Quiet hours: no push before 09:00 or from 22:00 local. The offset is `tz_offset_min` on the activity doc, else UTC. The push waits for 09:00.
- Belt rate: one push for each member and crew in 6 hours. Moves inside the window leave as one push, `<Name> took <Title> and <m> more. You hold <n>.`
- Join coalescing: from member 6, joiners inside 1 hour leave as one push, `<Name> and <m> others joined <Crew>.`
- The joiner never gets a join push. The member who causes a recompute never gets a push for a belt they took.
- Code: `event_push.go` (queue, quiet hours, belt), `crew_join_push.go` (join). Tests: `event_push_test.go`, `crew_join_push_test.go`, and `belt_and_join_pushes_reach_the_push_worker` in `tools/e2e/tests/head2head_feed.rs`.

## 8. Later

- The opened view with the round robin of all pairs.
- A per-crew off switch.
- Recompute on member join and leave.
- The client sends `tz_offset_min` with `game_session_end`.
