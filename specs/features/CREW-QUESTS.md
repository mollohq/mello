# CREW-QUESTS

> **Component:** Crew quests (server `crew_quests.go`, `crew_gifts.go`,
> `entitlements.go`; client `handlers/quests.rs`, `callbacks/quests.rs`,
> `feed_quests.slint`)
> **Status:** built
> **Related:** `CREW-FEED-ZONES.md` (the Stage, Crew history), `SEALED-REVEAL.md`
> (the sealed gift), `TEXT-CHAT.md` (crew emoji), `CREW-INVITES.md` (the custom
> link, the banner on the join page and the link card), `16-CREW-EVENT-LEDGER.md`
> (the stores)

A new crew gets a quest card on the Stage for its first 14 days. The card has
five steps. Each step unlocks a gift. The owner opens a gift with the sealed
reveal.

## 1. Steps and gifts

| # | Step | Done when | Row action | Gift |
|---|---|---|---|---|
| 1 | Crew made | `create_crew` | none | Crew banner |
| 2 | A friend joins | The first member who is not the owner joins | Invite (opens the invite modal) | Custom invite link |
| 3 | First call | A second person joins a voice room of the crew. A browser guest counts | Start voice (joins the default channel) | Five crew emoji |
| 4 | First clip | The first clip posted to the crew, voice or game | The clip key | Founding week |
| 5 | First session | The first `game_session` event of the crew | Settings, when game activity is off | m3llo+ for 30 days for every member |

- Crew made is done at the start, so the card opens at 1 / 5.
- Step 3 has no length. A re-join of the channel a user is in does not count.
- A step done after the 14 days still unlocks its gift. Only the card ends.
- A crew made before quests has no record and no card. Its banner, invite
  link and crew emoji are open in Crew settings. The founding week and m3llo+
  stay quest gifts.

## 2. The quest record

`crew_quests/{crew_id}`, system-owned, written by the server only.

| Field | Content |
|---|---|
| `owner_id` | The crew's creator, who opens the gifts |
| `made_ms`, `ends_ms` | The crew's creation, and 14 days later |
| `steps.friend` | `done_ms`, `user_id`, `name`, `invited_by`, `invited_by_name` |
| `steps.call` | `done_ms`, `user_ids`, `names`, `guests` (a count), `channel_id`, `channel_name` |
| `steps.clip` | `done_ms`, `clip_id`, `clip_type`, `clipper_id`, `clipper_name`, `game` |
| `steps.session` | `done_ms`, `game_id`, `game_name`, `game_igdb_id`, `minutes`, `user_ids`, `names` |
| `plus_granted` | The members who got m3llo+ from step 5 |
| `invite_copied_ms` | The first copy of an invite link. Not a step |

- A step that is not done is absent. A done step never changes.
- The writers sit where the events happen: `recordMemberJoined`, `joinVoiceRoom`,
  the clip store, the `game_session` writer.
- A write uses the storage version and retries on a conflict.
- After a write the server sends `feed_changed` (reason `quest`) to the
  members who have the crew open, so the card updates at once.

## 3. The gifts on the server

`giftOpen(crew, gift)` decides if a gift works:

| Gift | Open when |
|---|---|
| banner | Always |
| link, emoji | The crew has no quest record, or its step 2 (link) or step 3 (emoji) is done |
| week, plus | The crew has a quest record and its step 4 (week) or step 5 (plus) is done |

Opening is the owner's moment on the device. The server checks only that the
step is done, so a gift works on a device where it was never opened.

### 3.1 Crew banner

| Item | Value |
|---|---|
| Kinds | `game`: the wide hero art of a game, from the art Worker (`GAME-ART.md`). `upload`: a JPEG the client made |
| Upload | The client crops to 1200 by 630 and sends a JPEG of at most 400 KB. Key `crew-banners/{crew_id}/{id}.jpg` on the media bucket |
| Stored | `crew_profile/{crew_id}`: `banner {kind, game_id, key, url, version}`. The version changes on every save |
| RPCs | `crew_banner_upload_url`, `set_crew_banner {crew_id, kind, game_id \| key}`, `clear_crew_banner`. Owner only |
| Shows | The client crew header (88 px with a banner), the join page head and the link card (`CREW-INVITES.md`) |

A new upload or a clear deletes the replaced object from the bucket.

### 3.2 Custom invite link

| Item | Value |
|---|---|
| Name | 3 to 24 lower-case letters, digits or dashes. It starts and ends with a letter or a digit |
| Not allowed | A short list (m3llo, admin, support, join and the like), and a name in the shape of a generated code |
| Stored | A key in `invite_codes` with `{crew_id, custom: true}`, and `crew_profile.invite_name` |
| RPCs | `check_invite_name {crew_id, name}` answers `free`, `taken` or `not_allowed`. `set_invite_name {crew_id, name}`. Owner only |
| Change | The old name stays with the crew for 30 days, then any crew can take it |

The random code keeps working. The invite modal shares the custom link first.

### 3.3 Crew emoji

| Item | Value |
|---|---|
| Slots | 5 for every crew |
| Who | Any member adds. The adder and the owner remove |
| Image | The client sends a square PNG of 128 px, at most 256 KB. Key `crew-emoji/{crew_id}/{id}.png` |
| Name | 2 to 24 lower-case letters, digits or underscores, unique in the crew. Written `:name:` |
| Stored | `crew_emoji/{crew_id}`: `emoji [{id, name, url, key, added_by, added_by_name, added_ms}]` |
| RPCs | `crew_emoji_upload_url`, `crew_emoji_add`, `crew_emoji_remove` |

How chat uses them is in `TEXT-CHAT.md`.

### 3.4 Founding week

The Crew history weeks that overlap the first 7 days after `made_ms` never
lock, for every member, once step 4 is done (`CREW-FEED-ZONES.md` section 7).
That is one week, or two when the crew was made late in a UTC week. The
payload marks such a week `founding`.

### 3.5 m3llo+ for 30 days

- `user_plus/{user_id}`: `{until_ms, source, quest_grant_ms, quest_crew_id}`.
  `IsUserPremium` reads only this record.
- When step 5 is done, every member at that moment gets
  `until_ms = max(until_ms, now + 30 days)`.
- One quest grant per user, ever. A user in a second new crew gets no second
  grant.
- When it ends, the user is back to free.

### 3.6 Reports

`report_crew_media {crew_id, kind, name?}` records a report of an uploaded
banner or a crew emoji in `media_reports/{id}`. A member reports from the
client. The join page reports through the server key, with no user.

## 4. Payload

`crew_feed` adds `quests` to the zones while `ends_ms` has not passed. It is
absent for a crew with no record and after `ends_ms`.

| Field | Content |
|---|---|
| `owner_id`, `made_ms`, `ends_ms` | From the record |
| `steps` | From the record |
| `gifts` | What each gift holds now: `banner_url`, `banner_game_id`, `invite_name`, `emoji_count`, `first_emoji_url`, `founding_weeks`, `plus_granted` |

The server still picks its `stage` while `quests` is present, and keeps the
Stage entry in Clips, so the Stage is ready the moment the card goes.

## 5. The quest card

The card stands on the Stage after a live stream and ahead of the server's
Stage (`CREW-FEED-ZONES.md` section 2).

- A gift track of five tiles on top, then the five step rows.
- A gift that is not open shows `DROP 1` to `DROP 5`. An open gift shows its
  name: BANNER, INVITE LINK, CREW EMOJI, FIRST WEEK, M3LLO+ 30 D.
- A gift is `locked` (its step is not done), `ready` (done, not opened) or
  `open`.
- Only the owner opens a gift. A member sees a gift open once it holds
  content (a banner, a name, an emoji), and ready before that.
- A done row shows what happened: the crew name, who joined, who was in the
  call and the channel, who clipped and the game, the session length.
- An open row has its action: Invite, Start voice, the clip key, or Settings.

The card goes when every step is done and, for the owner, every gift is
opened. It goes for everyone when the server stops sending `quests`.

## 6. The sealed gift

A ready gift opens with the sealed reveal in the centre of the feed
(`SEALED-REVEAL.md`, kind `quest`).

| Gift | Label | The moment | The gift | Actions |
|---|---|---|---|---|
| Banner | `QUEST · CREW MADE` | The crew name, the date | A banner preview | Game art, Upload |
| Link | `QUEST · A FRIEND JOINS` | Who joined, who invited them | The suggested link | Set the link |
| Emoji | `QUEST · FIRST CALL` | Who was in the call, the guests, the channel | Five empty slots | Add emoji |
| Week | `QUEST · FIRST CLIP` | Who clipped, the game | The week, `W45` | none |
| m3llo+ | `QUEST · FIRST SESSION` | The length and the game, who played | `30 DAYS`, centred, in red | none |

- The opened state is per device and per gift: the key
  `quest:{step}:{crew_id}` in the sealed store (`SEALED-REVEAL.md` section 4).
- An action on the face closes the view and opens its screen: the banner
  picker, the invite name screen, the add emoji screen.

## 7. Client screens

- **Banner picker.** Tab "Game art": the crew's games first, then a search of
  the game catalogue. Tab "Your image": a file, cropped to the link card's
  shape. Save sets it.
- **Invite name screen.** The field starts with a name from the crew name. The
  state (free, taken, not allowed) shows as you type. Save sets it.
- **Add emoji screen.** Choose image, a name, Save. The list of the crew's
  emoji, each with Remove for the adder and the owner.
- **Crew header.** 88 px with the banner, a band of it at `center 30%` under a
  black wash, the crew name over it.

## 8. Test hooks

`dev_fault` (`MELLO_ENABLE_DEV_FAULT=1`) has three quest actions:

| Action | Effect |
|---|---|
| `quests_reset` | A new record for `crew_id`, made now, `user_id` the owner, no step done |
| `quests_clear` | Deletes the record: the crew is like one made before quests |
| `quests_expire` | Moves `ends_ms` into the past: the card is gone |

An `e2e` build reads the image pickers' file from `MELLO_E2E_PICK_FILE`
instead of a dialog.

## 9. Tests

- Go: `crew_quests_test.go` (the record, each writer, the window, the call
  rules), `crew_gifts_test.go` (each gift, `giftOpen`, the invite name rules,
  the m3llo+ grant, reports), `admin_media_reports_test.go`.
- Harness: `client/src/flow_tests/quests.rs`.
- Previews: `PreviewFeedQuests1-4`, `PreviewRevealQuest1-5`.
- Journey: `qa/journeys/feed-quests.ts` (flow FEED-13).

## 10. Out of scope

- iOS shows no quest card. The iOS feed skips the Stage kind.
- A crew emoji inside a sentence shows as `:name:` text.
- A push or a desktop notification when a step is done.
