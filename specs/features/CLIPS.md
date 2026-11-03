# CLIPS

> **Component:** Crew Memory, Clips, Crew Feed  
> **Version:** 1.0  
> **Status:** v1 In Progress (durable crew memory landed: clips and recaps persist outside the ledger trim; dot-matrix waveform clip cards landed; next-generation feed phase 2 landed: hero priority, empty-card rule, skeleton trio; game clips (F8) built on Windows, section 15)  
> **Depends on:** CREW-EVENT-LEDGER.md, 13-VOICE-CHANNELS.md  
> **Mockups:** m3llo-crew-feed-mockup-v4.html (populated), m3llo-crew-feed-cold-start-full.html (empty)

---

## 1. What This Is

Clips let crew members capture what just happened: someone saying something hilarious in voice chat, a clutch play while streaming, a heated argument. One tap, 30 seconds saved.

The crew feed is where clips live alongside session summaries, daily and weekly recaps, and game activity. Together they form the crew's memory.

When nobody is streaming, the center view of the app shows this feed. When someone goes live, the stream takes over. When the stream ends, the feed comes back with the session added to it. m3llo always has something to show.

This is also the foundation for m3llo+ monetization. Clip cards and media now persist for everyone (no 7-day deletion); m3llo+ will add unbounded clip history beyond the per-crew retention cap.

---

## 2. Why This Matters

Discord dumps you into a text channel. m3llo greets you with what your crew has been up to.

The goal is to make m3llo feel alive even when nobody is online. Every time someone opens the app, there's something to see: a clip from last night, a session summary, a daily recap showing who played what. The crew has a pulse.

Clips are also m3llo's primary organic growth channel. Every clip shared externally is an ad for m3llo that costs nothing. No other Discord alternative has anything like this. Revolt, Guilded, Element: all text channel clones. Crew memory is m3llo's product differentiator.

---

## 3. Clip Types

### 3.1 Voice Clip (v1)

Captured during a voice session. Audio only. Contains:

- Last 30 seconds of mixed voice audio from all participants
- Played back with a dot-matrix waveform (`DotWaveform`) and crew member avatars

Good for capturing funny conversations, heated arguments, singing, whatever happens in voice.

**Requires:** An active voice session. The client maintains a rolling disk buffer of mixed audio output.

**Hotkey:** the clip key captures a voice clip from any app, a game included. It does what the clip button does, and only when the clip button shows: in voice, with microphone access.

- The key is F7 on a new install (`DEFAULT_CLIP_KEY`). Settings, KEYBINDS changes it (specs/01-CLIENT.md, Settings tabs). It is stored as `Settings::clip_key`, in the format of `ptt_key` ("F7", "Ctrl+F7").
- The modifiers must match the binding. F7 with Shift does not clip when the key is F7.
- A held key makes one clip. Auto-repeat does not make more.
- KEYBINDS refuses a key that push-to-talk uses. When push-to-talk is on and gets the clip key later, the key stays push-to-talk.
- The clip button shows the key after "CLIP 30S", in a plain box. With no key, the box is not there.
- The listener is `client/src/platform/hotkeys.rs` (`register_clip`, `clip_key_step`).

### 3.2 Stream Clip (Future)

Captured by a viewer while watching a crew member's stream. Contains:

- Last 30 seconds of stream video (H.264)
- Mixed voice chat audio from all participants during those 30 seconds
- Game audio from the stream

This is the hero clip type. A friend hits a clutch play, you tap clip, and you have a 30-second video with the gameplay AND everyone's reactions.

**Requires:** An active stream being viewed. The viewer's client maintains a rolling disk buffer of compressed video and mixed audio.

**Not in v1.** Ships in a later phase once streaming is stable.

### 3.3 Game Clip (F8)

The user's own game: the last 30 seconds of the game video, the game audio,
the crew voice and the user's mic, in one MP4. A replay buffer records while
the user plays and is in voice, also without a stream. Clip type `game`.
Built on Windows. Section 15 has the whole design.

---

## 4. Rolling Disk Buffer

Both clip types rely on a rolling buffer written to disk. The buffer holds the last 60 seconds of content, and when the user taps clip, they get the last 30 seconds from that buffer. Zero RAM overhead.

This is the same approach ShadowPlay, Medal, and OBS Replay Buffer use. Proven pattern.

### 4.1 Voice Buffer (v1)

Voice written to temp files as Opus packets, in two streams. 60 seconds of both is 1.5 MB on disk. Essentially free.

- `ClipBuffer` (`libmello/src/audio/clip_buffer.cpp`) keeps two streams, each with a 2 s RAM queue that never waits on the disk:
  - the crew mix (all voices, stream game audio), from the render thread, with the time it plays (render time plus output latency) and m3llo's volume in the Windows volume mixer;
  - the local mic after the echo canceller, from the capture thread, with its capture time.
- A writer thread encodes each queue every 20 ms as 20 ms Opus frames (48 kHz mono, 64 kbps) into a ring file of fixed 256-byte slots: a 2-byte length, the 8-byte time of the frame, the volume as a 16-bit fraction, then the packet. 60 s is 3000 slots, 768 KB per stream.
- The ring files (`voice_ring_*`, `voice_mic_*`) are in the `mello_clips` folder of the OS temp folder. They are opened delete on close (Windows) or unlinked after open (macOS), so a crash leaves no file.
- A write error turns the buffer off.

On clip capture: the buffer encodes what is still in the queues, reads the last 30 seconds of slots of both streams, decodes them, and mixes them at their own times (the Opus lookahead taken out) into a WAV. The clip flow muxes that into an MP4 container. A game clip (F8) reads the same window with `read_window`, and the mux applies the mixer volume to the crew mix (section 15.4).

### 4.2 Stream Buffer (Future)

The viewer already decodes video frames for display and receives mixed voice audio. The buffer intercepts the compressed data *before* decode and writes it to a temp file:

- Video: raw H.264 NAL units (pre-decode). 60 seconds at ~5Mbps = ~37MB on disk. Buffering compressed packets avoids re-encoding on clip capture; the clip muxes the original data directly into MP4.
- Audio: mixed voice chat + game audio, encoded as Opus packets. Negligible disk usage on top of the video stream.

Total disk footprint: ~40MB temp file.

Trade-off: the clip's start point may not align with a keyframe, so the first fraction of a second might show artifacts. Acceptable for a 30-second clip.

### 4.3 Disk I/O

Sequential write throughput needed: ~0.6MB/s for stream buffers, negligible for voice-only. Any SSD handles this without impact. Spinning HDDs handle sequential writes fine too.

### 4.4 Buffer Lifecycle

- Buffer temp file created when the user joins a voice channel or starts watching a stream
- Written to continuously as a fixed-size ring file, oldest data overwritten
- Deleted immediately when the user leaves voice or stops watching
- Location: the `mello_clips` folder of the OS temp folder. It is not a setting: the app picks the folder.
- If disk is critically full (< 100MB free), disable buffering gracefully and hide the clip button

### 4.5 Memory Impact

Near zero. The client holds a small write buffer in memory (1-2 seconds, a few hundred KB) and flushes to disk continuously. Fully consistent with m3llo's "under 80MB RAM" target.

### 4.6 GPU-Accelerated Clip Rendering (Future)

VRAM is too expensive for buffering. Disk is the right storage layer.

GPU acceleration fits the clip *render* step for share export:

```
Disk buffer (H.264 packets)
  → NVDEC / AMF / D3D11VA (GPU decode)
  → GPU compositing (branded frame, overlays, 9:16 reframe)
  → NVENC / AMF / QSV (GPU encode)
  → Final MP4 on disk
```

Not needed for v1. Becomes essential when share export ships.

---

## 5. Clip Capture Flow (v1)

v1 is quick clip only. No trim UI. User taps the clip button, last 30 seconds are grabbed immediately.

```
User taps clip button (scissors icon, bottom bar) or presses F7
    │
    ▼
Client reads last 30 seconds from disk buffer
    │
    ▼
Client muxes into MP4 (audio-only for voice clips)
mello-core computes the 64-bucket peak waveform from the WAV
    │
    ▼
MP4 saved to local disk immediately
    │
    ▼
Clip metadata sent to backend via RPC: post_clip
    (clip_id, crew_id, clip_type, game, participants, timestamp, duration,
     waveform = 64 u8 peaks, base64)
    │
    ▼
Backend stores metadata in crew event ledger
Pushes notification to crew: "ostkatt clipped that"
Clip card appears in chat and crew feed
    │
    ▼
Background upload begins: MP4 → S3
    │
    ▼
Upload complete → backend updates clip record with storage URL
Clip is now playable by all crew members, not just the clipper
```

The clip button sits in the bottom bar alongside mic, deafen, and settings. Scissors icon in clip amber (#F59E0B) with a tinted background. Always visible when in a voice session. Not visible when not in voice. All clip-related UI uses amber/gold (#F59E0B) to visually differentiate clips from the rest of the app's accent color.

---

## 6. Clip Storage & Retention

### 6.1 Local

Every clip is saved to disk immediately on the clipper's machine. Always free, always permanent. The local file is the source of truth until upload completes.

The capture pipeline writes a 16-bit PCM WAV to the OS temp directory, then encodes it to MP4/AAC-LC (64 kbps) in the background before upload. Platform encoders:

- **Windows:** Media Foundation (`IMFSinkWriter` / `IMFSourceReader`)
- **macOS:** AudioToolbox (`ExtAudioFile` API)

A 30-second voice clip encodes to ~240 KB MP4. Encoding takes <200 ms on any modern CPU.

### 6.2 Cloud (S3-Compatible — Cloudflare R2)

Production uses **Cloudflare R2** (S3-compatible, zero egress fees). Local dev uses **RustFS** in Docker.

Upload flow:

```
Client captures WAV → encodes to MP4/AAC locally
    │
    ▼
Client calls `clip_upload_url` RPC (clip_id, crew_id), after `post_clip`
    │
    ▼
Backend checks: the caller is a crew member and the clipper of this voice clip
    │
    ▼
Backend generates presigned PUT URL (15 min TTL) via AWS SDK
Object key: crews/{crew_id}/{clip_id}.mp4
    │
    ▼
Client HTTP PUTs MP4 directly to R2/RustFS (no backend proxy)
    │
    ▼
Client calls `clip_upload_complete` RPC
Backend updates the clip's media_url in the durable clips store
    │
    ▼
Clip is now playable by all crew members via public media_url
```

The R2 bucket (`mello-clips`) has public read access. `media_url` points directly to the public endpoint — no signed download URLs needed.

**Retention:** Clip cards are durable. They live in a dedicated per-crew document `crew_clips/{crew_id}`, outside the 7-day ledger trim, capped at the most-recent 250 entries (the cap keeps the single Nakama storage object under its 256 KB limit). Clip media in R2 persists for everyone too — there is no longer a 7-day lifecycle deletion. Full unbounded clip history is future m3llo+ work.

### 6.3 Storage Costs

A 30-second voice-only MP4/AAC clip ≈ 240 KB. Per active crew per month (~20 clips): ~5 MB. Negligible at any scale. R2 has zero egress fees, so playback costs nothing regardless of how often clips are replayed. Stream clips (future) will be larger but R2's pricing remains favorable.

### 6.4 Environment Variables

| Variable | Local Dev | Production | Purpose |
|----------|-----------|------------|---------|
| `S3_ENDPOINT` | `http://s3:9000` | `https://<account>.r2.cloudflarestorage.com` | S3 API endpoint (internal) |
| `S3_PRESIGN_ENDPOINT` | `http://localhost:9000` | *(not set, falls back to S3_ENDPOINT)* | Endpoint used in presigned URLs (client-reachable) |
| `S3_BUCKET` | `mello-clips` | `mello-clips` | Bucket name |
| `S3_ACCESS_KEY` | `minioadmin` | R2 API token access key | S3 credentials |
| `S3_SECRET_KEY` | `minioadmin` | R2 API token secret key | S3 credentials |
| `S3_PUBLIC_URL` | `http://localhost:9000/mello-clips` | `https://clips.m3llo.app` | Public base URL for media_url construction |

`S3_PRESIGN_ENDPOINT` exists because in Docker, Nakama reaches RustFS via `s3:9000` (internal DNS), but presigned URLs must be reachable by the client on the host. In production (R2), the endpoint is the same for both, so this var is omitted.

See [05-GETTING-STARTED.md](../05-GETTING-STARTED.md) for R2 setup instructions.

---

## 7. Center View: Crew Feed

The center area of the app becomes contextual based on crew activity.

### 7.1 State: No Active Stream (Default)

Center shows the **Crew Feed**: a bento grid layout with mixed-size cards, newest content at top. The feed is server-curated via the `crew_feed` RPC (see section 11.4). Deep scroll falls back to the raw `crew_timeline` / `crew_clips` / `crew_recaps` RPCs.

Card types in the bento grid:

- **Clip card** — single cell, always the standard size. Grey label, ring and dots; an unread clip wears red. Dot-matrix waveform, play ring, clip badge, duration, who clipped it, game tag, participant avatars, timestamp. Tap the wave to play from that point; drag to scrub. A voice clip is never the hero and never takes the wide cell.
- **Game clip card** — footage of the clipper's own game, with the m3llo play button. See section 15.6. A game clip with a poster can be the hero.
- **Stream preview card** — a finished stream with snapshots. Feed type `session-preview`, drawn by `SessionPreviewCard`. See [STREAM-SNAPSHOTS-CLIENT.md](./STREAM-SNAPSHOTS-CLIENT.md) sections 3.1 and 4. It leads the feed for 24 hours (see hero priority).
  - As the hero (frame C) the card is footage edge to edge in two columns and two rows, cut at the top right. A STREAM tag sits at the top left. A play ring (56 px, white outline, black disc) sits at 46 % of the height. The snapshot plays from the ring, so the card has no filled white button.
  - Bottom left: the time word in capitals ("YESTERDAY"), the title in Oxanium 28 px, then the streamer octagon, the streamer name, the game and "N watched". Bottom right: the length ("1 h 12 min") on a black chip.
  - A standard preview keeps the poster on top and a text strip under it.
- **Live stream card** — one person streaming now, drawn by `StreamCard`. A live stream is client state (`crew_state`), not a feed entry. The client shows it above the `this_week` cards, so it is always first.
- **Session card** — "Your crew played Valorant for 3 hours." Game icon, participant avatars, duration, clip count. From event ledger. A game session with a record shows the time under the game name, the record, and a footer with the octagons of who played, "Bob, solo" when one person did, and the length at the right.
- **Daily recap card** — one day of the crew in a compact card, a smaller version of the weekly recap. Feed type `daily-recap`, drawn by `DailyRecapCard`. Spec 19 section 3.5a has the data. A day with sessions to list is the day card: two columns, the numbers left and the day's sessions right, with the belts that moved under them. A voice session is a row of the day card and never a card of its own.
  - Chrome and type are the weekly recap's: a card with the bottom left corner cut, the label DAILY RECAP in grey, Oxanium for the numbers, Barlow 600 uppercase for the labels.
  - Under the label, the day in Oxanium 22 px: "Today so far" for the day that is not over, "Yesterday", then the day name ("Tuesday"). The octagons of the members who took part that day stand at the right of the day, most active first, as many as fit and six at most.
  - A keyline, then the numbers two by two: the hangout time (voice, stream and game minutes) and CLIPS, then SESSIONS and TOP GAME. TOP GAME shows the game art tile and the name. A day with no game session has no top game.
  - Under the numbers, up to two belts that moved that day, each a title chip beside the octagon of the new holder. A day with no belt move has no belt row.
  - The card is a standard cell until the day has a session; then it is the day card, two columns wide. It stands after the recap and the head2head entry and before the sessions. The server sends one entry for each of the last three days. The client draws the newest one.
  - A day with no minutes and no clip has no entry. A belt alone does not make a day. This is the empty-card rule below.
  - A day also needs to add something to the weekly recap: at least one clip, or a belt that moved that day, or three or more active members. A day of one or two members with minutes only has no entry. The server applies the rule (`dailyRecapHasContent`). The client draws only the newest entry.
  - A member who joined has the joined card. The other ledger events a catch-up card showed (a moment, a leave, chat activity) have no card.
- **Live session card** (`live-session`, `LiveSessionCard`) — a game in progress. Label NOW PLAYING with the red live pip. Red is only the pip. It shows the game art, the game name, the member octagons and "since N min". The client builds it from presence (game id, name, `started_at`) and the sensor of the viewer. It is not a ledger event.
  - One card for each game. Crewmates in the same game share one card and count from the earliest start.
  - A live stream is above the feed and comes first.
  - With no hero, the live card takes the wide slot of the first row. Head2head is always two columns wide, so it takes the wide slot of row 2. A voice clip is never wide.
  - A hero (a versus card or a hero clip) keeps its place. The live card is the first cell of the side column.
  - More games are cells behind the first row.
  - When the game ends, the live card leaves. The session card from the ledger replaces it. The two never show together.
  - The skeleton trio does not count it as a real card.
- **Recent games card** — the games the crew played or has installed, as tiles keyed by game id, newest played first. It replaces the Crew play rollup card. Data path: `crew_feed`, entry type `recent-games` (see [22-GAME-UI-SURFACES.md](../22-GAME-UI-SURFACES.md) section 3.4). The server sends `games` (up to six, each with `member_ids`, the members who have it installed or played it), `session_count`, `total_min` and `pc_count`, the members behind the tiles.
  - One PC (the viewer is the only owner): a narrow list of three rows, beside the wide head2head card of a crew of one. The header carries the week's line ("14 sessions, 31 h this week"), or nothing when the card has no sessions.
  - Two or more PCs: a poster grid, two columns wide. The header reads "RECENT GAMES" left and the week's line right. Five capsule tiles sit in one row (460 by 215 art, two corners cut), the game name in Barlow 600 under each tile, and under the name a 16 px octagon for each member who owns the game. The row is capped at five. In a one-column slot the card stays the list.
  - From three PCs each poster carries when the game was last played ("Yesterday", "3 days ago") under the owner octagons. In a one-column slot a list row shows when and the hours on one line ("2 days ago, 4 h"), with the owner octagons at the right. A list row has no TRACKED label.
  - Three to five posters share the row, so a quiet week gets larger posters, not a gap.
  - The grid leads row 2 under a versus hero (frame B2). Row 2 holds the grid and one card, so the session card sits beside it. Rows after it move up by one card (`feed_grid.rs`, `Layout::r2_wide`, and `row3-base` in `crew_feed.slint`).
- **Joined card** (`joined`, `JoinedCard`) — a member who joined in the last 24 hours, as the smallest card the grid has (frame B2). The grid has one cell size that is a standard cell, so the card is a standard cell and fills it: the joiner's octagon at 72 px, "Jonas joined" in Oxanium 20 px, "invited by b0bben" and "Today 23:14" in Barlow 300 (or "Yesterday 23:14" for a join before midnight), and "Say hi" at the bottom. One card for each member who joined in the last 24 hours, newest first, at most three. Without an inviter the second line is absent. The viewer's own join and a member who left do not show.
  - "Say hi" is a white outline action, not filled: the surface may already hold its one filled white button, and red stays on the invite. The click sends a greeting to the crew chat at once. It does not fill the input first. The greeting is one of six short lines (`say_hi.rs`), chosen by the joiner's id, that mention the member in the mention form of the chat (`@Jonas`, with a mention reference). Its label is "Say hi to Jonas".
  - After the send the button reads "Said hi" and is off (`accessible-enabled: false`). It is once for each joiner. The client stores it by crew in the local settings (`said_hi`), so a restart does not switch it on again.
  - The card is in the grid flow, not above it. It stands in the first card place after the first row of the lead (the hero row, row 2 in a plain grid after the first three cells) and before the sessions. Under the versus hero (frame B2) it is the one card beside the poster grid. It never takes a wide slot (`feed_grid.rs`, `joins_at`). It is not a real card for the skeleton trio and the cold start.
  - Data path: the `joined` array of the `crew_feed` response (`user_id`, `name`, `invited_by`, `invited_by_name`, `ts`), read from the `member_joined` events of the ledger (`buildFeedJoins` in `crew_feed.go`). The invite code RPC (`join_by_invite_code`) adds the caller with system authority, which runs no join hook, so it writes the `member_joined` event itself with `invited_by` set to the member whose code was used. The client builds the cards in `join_cards` (`handlers/clip.rs`) from `joined_line.rs`. The state port reports them as `joined`.
- **Weekly recap card** — stats summary for the week. Generated weekly by backend job. Tall or wide, never one cell (see Grid layouts).
- **Skeleton trio** — three dashed placeholders for an empty crew (see section 8).
- **Invite line** — one line and one red button under the skeleton trio (see section 8).

A card carries no note that explains it. A card shows data ("14 sessions, 31 h this week"), a label, a state or an action. "From this PC", "Monday" and "Head2head" at the right of a header are notes and do not show.

A card with no content is never emitted. One rule in `crew_feed.go` (`cardHasContent`) applies to `this_week` and `memory`: a session with no reportable minutes, a recap with nothing in it, a stream preview with no snapshots and a recent games card with no game minutes do not appear.

#### Hero priority

The server picks the hero of `this_week` in this order. The first item that exists wins.

1. A live stream. The client shows it above the feed. The server order starts at the next item.
2. A stream preview from the last 24 hours. The best one by quality score.
3. A game clip with a poster from the last 24 hours. The newest one (`freshGameClipHeroIndex`). A voice clip is never the hero.
4. A belt that moved in the last 24 hours. The head2head entry is the hero (HEAD2HEAD.md section 5). `beltMovedHeroIndex` stays an empty hook: the client builds the card.
5. A real recap, on Monday (UTC), while the recap is under 24 hours old.
6. The best session: the best stream preview of any age.

If no item exists, the feed has no hero. The latest recap then takes its own pinned slot as before.

#### Grid layouts

`crew_feed.slint` lays cards out by position. `feed_grid.rs` (`Lead`) mirrors every rule, and its tests keep the two in step. The grid has three columns and rows 232 px high.

| Lead | Cards | Layout |
|---|---|---|
| Hero | first card | two columns and two rows, a side column of two cells, or one tall recap |
| Wide | head2head (wide or pairs), or a live session with poster art | two columns, one cell beside it |
| CellWide | a session, then head2head wide | frame B: the session is the cell, then two columns |
| FullRow | a versus card, or a hero clip | one full row |
| Plain | invite card, a join, or any card after a stream | rows of three cells |
| Belt | belt hero, head2head, two cells | frame E: the hero is two columns with a cell beside it, then head2head, two columns, with a cell beside it |
| TallTrio | head2head, recap, two cells | frame F: head2head is two columns, the two cells sit under it, the recap is tall beside both |

- A crew of one with a session puts the session first (CellWide). Frame A has no session, so the wide card stays first.
- After a live stream or the two-stream hero the grid is plain rows. A card there is a cell, never a second hero.
- The poster grid of recent games takes the first wide slot after head2head, the recap and the day card: row 2, or row 3 when the day card holds row 2 (and under a hero, frames C and D). It is always on the first screen. The one-PC list stays in the side column.
- Head2head is always two columns wide, at every crew size. It leads a feed with no hero. Behind any other lead it takes the first wide slot after the lead (row 2, else row 3), never the one-column side slot. After one live stream it leads the grid beside the stream. A feed with no hero, head2head and a recap with content puts the recap second, tall (TallTrio): two plain cells sit under head2head.
- Row 2 is the last row and has free cells: the stream and voice clip skeletons fill them at the card height, and the recap skeleton is the slim row under them (frame B). A full last row keeps the trio as one row.
- A recap holds the whole side column beside the two-stream hero.
- The weekly recap with content always stands top right: the tall card, one column and two rows, in the side column of the first two rows, whatever leads. The lead block beside it is two columns for those two rows: a wide card alone (head2head, a versus hero, a hero clip, a live session with poster art fills both rows), or a cell with the next card, then one wide card or two cells. The rows after the block are three columns (`feed_grid.rs`, `Lead::RecapSide`; `recap-side` in `crew_feed.slint`). The recap is never three rows and never a wide card.

#### Two streams (frame D)

Two members who stream together share the wide hero. Each stream is a strip with its own play ring, streamer octagon and game. One tag says LIVE and how many stream ("2 STREAMING"). The card has no filled white button.

The client holds one remote streamer today. `CrewFeed.hero-streams` takes the two strips and nothing fills it yet. The data path for a second stream is a later phase. `two_stream_start` in `feed_grid.rs` is the grid rule for that phase.

#### Quiet and dormant feeds (frames E and F)

A feed with nothing new must not look broken. The rules live in `client/src/quiet_row.rs`.

- **Quiet row.** A slim row with a moon, in the place of the recap. It shows when the crew has two or more members, nothing is live, no stream preview or belt move is under 24 hours old, no recap is under seven days old, and something happened in the last 14 days. The text is "No crew time last week." and, when members played, "Bob and Sara still played." The row has no note at the right.
- **Belt hero.** With no hero, the newest belt move that involves the viewer leads, at any age up to 14 days. The card is two columns wide. It shows who held the title, the title, who holds it now, a headline ("Sara took Night Owl from Bob") and the values ("03:40 beats 03:14."). The head2head card is the wide card under it, with a cell beside each.
- **Idle voice card.** When the quiet row shows and nobody is in the default voice channel, a card shows the channel, "Empty", the date of the last call and the one filled white button, Start voice. The red invite stays on the head2head card, next to the open seat.
- **Dormant strip.** Nothing happened for 14 days or more. A strip across the top, with the label QUIET, says "Nothing new since 2 October. Leo holds The Carry." and how long ago, in weeks. The feed keeps the last real recap, its sessions and the head2head card. No skeletons show and no empty card shows. A feed that is dormant has no daily recap: its last three days are empty. A recap is a summary of past weeks, so it does not count as activity.
- A recap from the current week has no age at the right of its label. An older recap says "Last week", then "3 weeks ago" (`feed_age::recap_age_label`). "Week 4" shows when the server sends `week_number`. Until then the card says "This week".
- A session older than a week shows its date ("2 October"), not "9d ago".

The STREAM button in the control bar starts a stream. While the user streams, the button is a red END STREAM button, and a click ends the stream.

#### Clip card waveform

Clip cards render audio as a dot-matrix waveform (`DotWaveform` in `crew_feed.slint`): a 64-column grid of dots — 15 rows on the hero card, 9 on the standard card — driven by the stored peak data. Columns up to the playhead light with a phosphor falloff and a white snap column marks the playhead; idle cards show a ghost hairline plus a mono-font time chip. Paused playback breathes (~1.4s cycle); dot state changes fade over 120ms. Dragging scrubs with column snapping and a hover time chip; releasing on an idle card starts playback at that position. Seeks are absolute milliseconds (`seek-clip-ms`) computed against each card's own duration — the global clip duration zeroes when playback finishes and must not be used for scrubbing.

### 7.2 State: Active Stream(s)

**Single stream:** Stream takes over center, full size. Crew feed accessible via a tab/icon showing "X new clips" badge.

**Multiple streams:** One stream is always "main" and fills the center at full size. Other active streams appear as small thumbnails along the bottom of the center panel. Click a thumbnail to swap it into main. The crew feed tab/icon sits alongside the stream thumbnails.

```
┌──────────────────────────────────────┐
│                                      │
│          MAIN STREAM (full)          │
│          ostkatt · CS2               │
│                                      │
│                                      │
├──────────┬──────────┬────────────────┤
│ FaiL     │ b0bben   │               │
│ Valorant │ LoL      │  📋 Feed (3)  │
└──────────┴──────────┴────────────────┘
```

Stream thumbnails are semi-static: they update with a keyframe roughly every 10 seconds. No continuous decode, no extra CPU/GPU cost for streams you're not actively watching. Only the main stream is fully decoded and rendered.

When the user first opens the center while multiple streams are live, the most recently started stream is main by default. The user's own stream (if they're streaming) is never shown as main to themselves.

Clip button captures from whatever the user is currently watching (the main stream for stream clips, voice audio regardless).

### 7.3 State: Stream Ends

When the main stream ends: if other streams are still live, the next thumbnail auto-promotes to main. If no streams remain, center transitions back to crew feed. A new session card appears at top with the stream summary.

When a non-main stream ends: its thumbnail disappears. No other change.

### 7.4 Scroll Behavior

The feed scrolls down to load older content. When user is scrolled down and new content arrives, a "X new clips" banner appears at top (tap to scroll up). Feed does not yank the user to top automatically.

Future: ambient slow upward drift when at top and idle (screensaver mode). Pauses on interaction. Not in v1.

---

## 8. Cold Start

A new crew with no activity sees three skeleton cells, the skeleton trio, and one invite line.

### 8.1 Skeleton Trio

Exactly three cells, in this order. Each is a dashed outline with an icon and one line.

| Cell | Line | Real card that replaces it |
|---|---|---|
| Stream | "Your streams land here" | A stream preview or a live stream |
| Voice clip | "Voice clips land here" | A clip |
| Weekly recap | "Your week, every Monday" | A weekly recap |

Rules:

- The trio comes after the real cards. It never takes the hero slot or a card cell.
- A cell hides when its real card type exists in the feed.
- All three cells hide from three members on, with or without real cards. At two members the trio stays under the real cards, and each cell still hides when its own card type exists.
- The `locked` upsell is not a real card.
- A new crew shows no "Crew history" heading and no m3llo+ card (section 8.5).
- The trio is client state. It is not in the card model. Component: `SkeletonTrio` in `client/ui/components/skeleton_trio.slint`. Rule: `skeleton_trio_for` in `client/src/handlers/clip.rs`.

### 8.2 Invite Line

One line under the trio: "Your crew is not here yet. Send them the link." A red "Share invite link" button follows it. A "Hide" link removes the line for that crew. The line shows only while at least one skeleton cell shows.

### 8.3 Progressive Replacement

1. First stream that ends with snapshots, or first live stream → the stream cell goes.
2. First clip captured → the voice clip cell goes.
3. First week complete → the recap cell goes.
4. Second member joins and one real card exists → the remaining cells and the invite line go.

### 8.4 Visual Treatment

Dashed outline, one cut corner, a dark ground, icon and text at low contrast. No animation or pulsing. A cell is an empty slot, not a loading indicator.

### 8.5 Crew History and the m3llo+ Card

The `memory` section of the feed has the heading "Crew history" and the older cards of the crew. The server pins the `locked` m3llo+ upsell ("Unlock your crew's full story") at the end of it for a viewer who is not premium.

- The section shows only when it holds at least one real card. A crew with no history (day 0, or a feed with nothing old) shows no heading and no cards of it.
- The m3llo+ card is not a real card. It shows only at the end of a section that has history, and only when the client flag `plus_upsell_enabled` is on. The flag defaults to off, because m3llo+ does not exist yet. It is in the local settings (`client/src/settings.rs`).
- The rule is `memory_for_display` in `client/src/handlers/clip.rs`. The server keeps sending the `locked` entry.

---

## 9. Chat Integration

When a clip is captured, a special message card is posted to crew chat automatically:

```
┌─────────────────────────────────┐
│ ✂ ostkatt clipped that          │
│                                 │
│ [Waveform visualization]        │
│                                 │
│ Voice · 30s · ostkatt, FaiL, b0 │
│                          ▶ Play │
└─────────────────────────────────┘
```

Playable inline in chat. Tap to expand and play without switching to the crew feed.

---

## 10. Weekly Recap (v1)

A backend job runs weekly (Monday 00:00 UTC) and generates a recap from ledger data (hangout time, top game, longest session, top members) and the durable clips store (clip counts). Recaps are durable: stored in a dedicated per-crew document `crew_recaps/{crew_id}` (no cap, one per week), outside the ledger trim, so past recaps persist.

### 10.1 Contents

- Total crew hangout time (voice + stream hours combined)
- Top game played
- Longest session (who, what game, how long)
- Number of clips captured
- Most active member
- Most clipped member

### 10.2 Presentation

Appears as a card in the crew feed. NOT posted to chat. It arrives sealed and opens in the centre with the reveal (SEALED-REVEAL.md); the card in its slot is the opened view, there is no full-screen view.

The tall card (one column, two grid rows, with or without belts) shows, top to bottom: the label and the week; hangout, clips, longest and top game with its art, two by two; Most active and Most clipped, one line each; Belts moved (two rows, "+N more" in the section header); Belts held (octagon, count, "belts"); Games played (the art tiles of the week's games, the hours in the section header). A week with no belts drops both belt sections. The card carries no control. The server sends `most_clipped`, `most_clipped_user_id`, `most_clipped_count`, `clip_of_week` (the longest clip of the week: id, title, duration, game, media) and `games_played` with `minutes` for each game. A card carries no note that explains it: no day, no source label.

The centre view shows the same card at up to 1.1 times its slot size, with the numbers counting up. A click on the scrim sends it back to its slot, open.

---

## 11. Crew Feed Data

### 11.1 Event Ledger (Existing)

Voice sessions, stream sessions, game sessions, chat activity summaries. These are the ephemeral pulse events, stored in the 7-day rolling ledger `crew_events/{crew_id}`. They generate session cards, daily recap cards, now playing cards, and recent games cards. Clips and recaps are not ledger events; stream sessions are also mirrored to a durable store (§11.3a) so replays survive the trim.

### 11.2 Clips (Durable)

Clip metadata is durable, stored in its own per-crew document `crew_clips/{crew_id}` (capped at most-recent 250). Each `StoredClip` carries:

- event_id (time-sortable, used as the pagination cursor), clip_id, clip_type (voice, game; stream in future)
- media_url (populated after S3 upload by `clip_upload_complete`)
- local_path (clipper's machine). Only the clipper's client uses it: the client compares `actor_id` with the viewer (`game_clip_fields` in `client/src/handlers/clip.rs`). Two clients on one PC share the temp folder, so a file that exists is not proof.
- a game clip also: width, height, has_voice, poster_url (section 15.8)
- participants + participant_names (at clip time)
- game (if detected via game sensing)
- duration_seconds, actor_id (clipper), ts, score
- waveform (base64 of 64 u8 peaks computed from the captured WAV at post time; empty for legacy clips, which render a placeholder pattern)

The waveform is computed once on the clipper's machine: mello-core parses the captured PCM WAV into 64 max-amplitude buckets (`waveform.rs`) and posts them base64-encoded (~88 chars) on `post_clip`. The backend validates exactly 64 decoded bytes (`validateClipWaveform`) and stores them on `StoredClip.Waveform`.

### 11.3 Recaps (Durable)

Weekly recaps are durable, stored in `crew_recaps/{crew_id}` (no cap). See section 10.

### 11.3a Stream Replays (Durable)

Stream sessions stay in the ledger for the recent window, but a lean projection (`StoredStreamSession`: streamer, title, game, duration, peak viewers, snapshot URLs; viewer IDs dropped) is also mirrored into `crew_stream_sessions/{crew_id}` (capped at most-recent 150). It is upserted when a stream ends and again from the snapshot backfill job, so late-arriving snapshots land on the durable copy and snapshot URLs only grow. The backfill job also recreates a missing durable copy when ledger snapshots are already current. This keeps stream replays (a `session-preview` when snapshots exist, else `session`) available in the `memory` section long after the 7-day trim.

### 11.4 Retrieval

- `crew_feed` RPC is the curated primary feed. Curation lives server-side: the server decides which items appear, their order, and each item's role and size. It returns two sections:
  - `this_week`: the curated recent feed (replaces the client-side bento ordering). Hero precedence is in section 7.1 (`heroIndex` in `crew_feed.go`). A voice clip is never the hero and never gets the wide size. A game clip can be the hero (section 7.1). The latest recap is pinned, then the daily recaps, then diversity/priority fillers capped at 7 slots with one wide/lg promotion. Empty cards are dropped first (`cardHasContent`).
  - `memory`: the durable spine of older recaps, clips, and stream replays not already shown this week, returned as a bounded first page (currently 60 entries), with the server-emitted `locked` m3llo+ upsell pinned at the end when the viewer is not premium.
  - Each entry is `{ id, type, role, size, ts, data }`. `type` is the data kind (clip | recap | daily-recap | session-preview | session | recent-games | head2head); `role` (hero | standard | quiet | recap | locked) and `size` (sm | md | lg) are the curation knobs tuned server-side. `data` is the existing typed payload; clients keep their own text formatting and ignore unknown type/role/size values. The role/size weights and the quiet-type set are a server config block (`crew_feed.go`), changeable without a client release.
- `crew_timeline` RPC is the raw paginated source (merged 7-day ledger + recent clips + latest recap, newest-first, cursor-based). Used for deep scroll.
- `crew_catchup` RPC merges recent clips (newer than `last_seen`) into its candidate set, ranked higher than passive events.
- `crew_clips` and `crew_recaps` RPCs paginate the full durable history (newest first) for the deeper memory surfaces.

---

## 12. Open Questions

- **Clip playback for offline clippers.** Show "waiting for upload" or skip until available?
- **Clip deletion.** Clipper can delete own clips in v1. No crew-wide moderation yet.
- **Voice clip consent.** Joining a voice channel implies consent for v1. Consider showing a notification ("ostkatt is clipping") when clip button is tapped. Legal review needed before wide EU launch.
- **Clip audio mixing.** Buffer captures clipper's playback. Muted users are not in the clip. Two people clipping the same moment get different audio. This is correct behavior.
- **Feed scroll position.** New content appears at top. "X new clips" banner for users scrolled down. Never auto-scroll.

---

## 13. v1 Scope

### Ships in v1

**Client:**
- Rolling disk buffer for voice audio in libmello
- Clip button (scissors, amber #F59E0B) in bottom bar during voice sessions
- Quick clip: single tap grabs last 30 seconds, no trim UI
- Mux voice audio to MP4, save locally
- Background upload to S3
- Clip card in chat with waveform and inline playback
- Crew feed center view with bento grid layout
- All card types: clip, stream preview, session, daily recap, now playing, recent games, weekly recap
- Cold-start skeleton trio and invite line
- Contextual center view (feed vs active stream)
- Game clips (F8) on Windows: replay buffer, MP4, upload, card, streamed playback (section 15)

**Backend:**
- `post_clip` RPC storing clip metadata in the durable `crew_clips/{crew_id}` store (capped at most-recent 250)
- `crew_feed` RPC: server-curated `this_week` + `memory` sections with role/size knobs
- `crew_timeline` RPC merging the 7-day ledger with recent durable clips and the latest recap (raw source for deep scroll)
- `crew_clips` and `crew_recaps` RPCs paginating the full durable history
- S3 integration for clip upload and playback URLs
- Weekly recap generation job (Monday 00:00 UTC) writing to the durable `crew_recaps/{crew_id}` store

### Does NOT ship in v1

- Stream clips
- Trim / clipping UI
- Share export (vertical video, branded frames, GPU rendering)
- m3llo+ paywall, retention enforcement, crew boost
- Ambient drift animation on bento grid
- Year-end recap

---

## 14. Future Phases

### Stream Clips
Rolling video+audio disk buffer on viewer side. Mux H.264 + mixed audio into MP4.

### Trim UI
60-second timeline scrubber, default 30-second selection. Quick clip remains as tap-and-hold.

### Share Export
Social-ready vertical video (9:16). Branded frame, m3llo watermark. GPU-accelerated rendering.

### m3llo+
Subscription flow. Permanent cloud retention. Crew boost. Retention expiry notifications. Full crew feed history.

---

## 15. Game Clips (F8)

Built on Windows. macOS and the other open items are in section 15.14.

### 15.1 What F8 Does

F8 saves the last 30 seconds of the user's own game as one MP4:

- the game video,
- the game audio,
- the crew voice that the user heard, and the user's own mic.

A replay buffer records while the user plays and is in voice, also when the
user does not stream. It works like a ShadowPlay replay buffer. A game clip is
clip type `game`. It is not the stream clip of section 3.2: a stream clip is a
viewer's clip of a crew member's stream.

| Key | Clip | Needs |
|---|---|---|
| F7 | Voice clip, audio only (section 3.1) | Voice, mic access |
| F8 | Game clip: video, game audio, voice | Voice, and the replay records (section 15.2) |

The key is the only trigger. Auto highlights (a clip that game telemetry
starts) need a way to ask the user first. They are a later iteration
(section 15.14).

### 15.2 When the Replay Runs

The replay worker (`mello-core/src/client/replay.rs`) runs one replay at a
time. It records when all of these are true:

| Condition | Source |
|---|---|
| The setting "Game clips" is on. Default: on. | `Settings::game_clips_enabled` |
| The user is in voice. A game clip always has the crew voice. | The voice session |
| Game sensing reports a game | The primary game: the foreground game, else a fullscreen game, else the oldest game |
| The OS temp folder has 2 GB free | Checked every 10 s. The replay's own ring files count as free (`RING_ALLOWANCE`, 128 MB), so a start at just over 2 GB does not stop itself. |
| A hardware encoder exists | `EncoderFactory`. No software encoder (14-VIDEO-PIPELINE.md §6.7). |
| WGC window capture delivers frames of the game | Section 15.3. The exclusive-fullscreen check does not turn the replay off: WGC window captured CS2 in exclusive fullscreen. |

| Event | Action |
|---|---|
| A game starts and the rules hold, or the user joins voice while a game runs | Start the replay. |
| The user leaves voice, the game exits, or the setting goes off | Stop. The ring files go. |
| Another game is the primary game for 10 s | The replay moves to it (`SWITCH_HOLD`, `GameHold`). An alt-tab between two games does not restart it. An exit takes effect at once. |
| The game is minimized | The capture holds and the ring keeps its content. The state is Waiting. A clip shows the last frame for the gap. |
| The free space goes under 2 GB | Stop. The state is DiskLow, and the HUD shows "Game clips are off: the disk is almost full" once. The replay starts again when the space is back. |
| The clip quality changes | The replay starts again at the new quality. |

Known limit: with two games and a window that is not a game in focus, the
primary game falls to the fullscreen or the oldest game, and after 10 s the
replay moves to it.

**The key** (`client/src/game_clip.rs`, `client/src/platform/hotkeys.rs`):

- F8 on a new install (`DEFAULT_GAME_CLIP_KEY`). Settings, KEYBINDS has a
  Game clip row. It is stored as `Settings::game_clip_key`, in the format of
  `ptt_key`.
- KEYBINDS refuses a game clip key that push-to-talk or the voice clip key
  uses, and the voice clip row refuses the game clip key.
- F7 and F8 keep their own held state. A held key makes one clip.
- The key makes a clip only while the replay records. Otherwise the HUD
  toast says why: "Game clips are off in Settings", "No game to clip", "Join
  voice to clip your game", "Clips are not available for this game".
- A clip shows "Game clip saved". A capture that fails shows "No game clip".
- The clip goes to the active crew. A user is always in a crew.

### 15.3 Capture

- **One capture per game.** The replay is a consumer of a `CaptureManager`
  (14-VIDEO-PIPELINE.md §4.8), with its own preprocessor and its own
  encoder. The stream encoder follows the network and drops its size and
  rate on a bad uplink. A clip must not get worse because of that, so the
  replay encodes with the recording intent (14-VIDEO-PIPELINE.md §6.8).
- **The ladder.** WGC window only. Never WGC monitor or DXGI: they record
  the whole monitor, with notifications and other windows. The hook only
  when a stream of the same game already uses it. The replay never injects
  the hook.
- **The client area.** WGC window frames are cut to the client area, so a
  windowed game has no title bar in a clip (14-VIDEO-PIPELINE.md §4.4).
- **The border.** Windows 11 hides the WGC border. Windows 10 always shows
  it, and game clips run there with it.
- **With a stream** of the same game: 12-STREAMING.md §3.7.

### 15.4 Rings and A/V Sync

Every sample carries a time on the steady clock (`mello_clock_now_us`). The
mux puts all sources on that clock.

| Source | Time of a sample |
|---|---|
| Video | The capture time of the frame |
| Game audio | The arrival time at `GetBuffer` minus the duration of the packet, as in the stream host |
| Crew mix | The render time plus the output latency: the time the user heard it. m3llo's session volume travels with it. |
| Mic | Its capture time. The voice ring keeps the mic apart from the crew mix, each with its own time anchors (section 4.1). |

| Ring | File | Content | Size |
|---|---|---|---|
| Video | `replay_video_<pid>_<n>.bin` | Annex-B access units. One segment is one GOP (1 s) and starts with an IDR plus SPS and PPS. An index per segment: byte offset, size, capture time and keyframe flag of each unit. | 40 slots of 2 s of the peak rate: 62.5 MB at 720p60, about 100 MB at 1080p60 |
| Game audio | `replay_audio_<pid>_<n>.bin` | 48 kHz stereo int16 PCM, a time anchor per 10 ms chunk | 40 s, 7.7 MB |
| Voice | The voice clip ring (section 4.1) | The crew mix and the mic as two Opus streams | 1.5 MB |

- The ring files are in the `mello_clips` folder of the OS temp folder,
  preallocated, and opened delete on close (Windows) or unlinked after open
  (macOS). A crash leaves no file.
- A GOP that is larger than its slot ends its segment early, and the ring
  asks the encoder for an IDR.
- The encode thread hands each unit to a bounded RAM queue (2 MB) and a
  writer thread writes it. A full queue drops the unit and every unit after
  it up to the next keyframe, counts them, and asks for an IDR. A disk stall
  never blocks the encoder.
- A capture pins the segments it reads. The writer skips a pinned segment.
  Two captures inside 30 s pin up to 32 of the 40 segments.

**Capture and mux:**

```
F8 ─▶ client gate ─▶ Command::CaptureGameClip ─▶ the replay worker
      ─▶ mello_replay_capture(ctx, 30 s, mp4_path, poster_path)
         1. T0 = the last keyframe at or before now minus 30 s.
         2. Pin the segments from T0. Read the voice window (read_window).
         3. Mux: video passthrough, game audio and voice mixed, AAC.
         4. Unpin. Write the poster JPEG.
      ─▶ Event::GameClipCaptured ─▶ Command::ShareGameClip ─▶ upload (15.7)
```

- Video: H.264 passthrough, no decode and no encode. Sample durations come
  from the capture times.
- Audio: one stereo AAC-LC track at 160 kbps. Game audio and voice go on a
  48 kHz timeline from T0. The voice is mono, center.
- Drift: each source gets a linear resample ratio from its own time anchors.
- Gaps (a minimized game, a dropped chunk) become silence at their own
  place. The audio after a gap does not move.
- Mix: game and voice at unity gain, then a limiter. Process loopback
  captures after the game's own volume and after its slider in the Windows
  volume mixer. The mux applies m3llo's session volume to the crew mix, so
  the balance is what the user heard.
- A clip is 30 to 31 s long, because it starts at a keyframe. A 30 s mux at
  5 Mbps takes under 1 s (the budget is 3 s).

### 15.5 The MP4 and the Poster

| Item | Rule |
|---|---|
| Muxer | `IMFSinkWriter`: H.264 input type equal to output type (passthrough), the AAC encoder MFT. No new dependency. |
| moov first | `mp4_faststart` after `Finalize` moves the moov box before mdat and moves the chunk offsets with it, so playback starts before the download ends. `MF_MPEG4SINK_MOOV_BEFORE_MDAT` wrote chunk offsets that point before the media data, so it is not used. |
| Poster | The first captured frame after the press: a copy to a staging texture on the capture thread (a submit, no wait), then the map and a JPEG (WIC) on the mux worker. A minimized game delivers no frame: the poster is then the clip's last IDR, decoded once. `MelloReplayClip::poster_live` says which. |
| Files | The MP4 and the poster are in `mello_clips` of the OS temp folder. They stay as the clipper's copy. |

### 15.6 The Feed Card

`GameClipCard` (`client/ui/panels/game_clip_card.slint`). A game clip card is
footage, like the stream preview card, not the waveform card.

- The poster edge to edge, one corner cut. Black washes under the text.
- Top left: the CLIP tag. An unread clip wears red.
- The m3llo play button at the center: a cut tile (top left and bottom
  right), a black wash, a white keyline, the play triangle with its back
  corners cut. Hover fills it white. It replaces the play ring over footage.
- Bottom left: the clipper octagon, the clipper name, the game. Bottom
  right: the length on a black chip ("0:31").
- The poster is up and the video is not: the card shows "uploading".
- A game clip with a poster from the last 24 hours can be the hero (section
  7.1).
- `FeedCardData` fields: `clip-kind` ("game"), `clip-poster-path`,
  `clip-uploading`, `clip-can-play`.
- Only the clipper's client uses `local_path` and the local poster
  (section 11.2). The others use `poster_url` and `media_url`.
- A click opens the player (section 15.9).

### 15.7 Upload

`mello-core/src/clip_upload/`. One worker thread at the lowest priority, one
request at a time: `post_clip`, the poster, the MP4 parts, the complete.

| Item | Rule |
|---|---|
| Order | The poster first, at once in every state, at 1 Mbps or more. The card shows the poster and "uploading" as soon as it is up. Then the MP4. |
| Body | Read from disk in writes of 16 KB. The file is never read into RAM. |
| Parts | S3 multipart in 5 MiB parts, each a presigned `UploadPart` with a signed `Content-Length`. |
| Pace | No game: no cap. A game: 3 Mbps. A stream: the stream's budget (12-STREAMING.md §3.7). A budget of 0 pauses the upload. A pause of 10 s ends the part in flight. |
| Queue | `game_clip_uploads.json` (`game_clip_uploads.<MELLO_SESSION_KEY>.json` for a second client), in the state folder (`session_store::store_dir`). One entry per clip with its upload id and its done parts, saved after each step, so a restart goes on from the last step. At most 50 entries and 7 days. Only the clipper's session uploads a clip. |
| Errors | A part that gets 403 asks for new URLs once (with `upload_id`). A 404 (the upload is gone) starts a new upload once. A refusal drops the clip from the queue. The local file stays. |

### 15.8 Backend

`backend/nakama/data/modules/game_clips.go` and `clips.go`. The voice clip
calls do not change. An empty `clip_type` is `voice`. A game clip uses three
calls in this order: `post_clip`, `clip_upload_url`, `clip_upload_complete`
(once for the poster, once for the video).

**`post_clip`.** `clip_type` is `voice` or `game`. Other values are refused.
A game clip adds `width` (1 to 7680), `height` (1 to 4320) and `has_voice`.
`clip_id` matches `[A-Za-z0-9_-]{1,64}`, because it is part of the object
keys. `StoredClip` gets `width`, `height`, `has_voice` and `poster_url`.
`post_clip` is idempotent on `clip_id` and the clipper: a retry gets the first
`event_id`. The same `clip_id` from another user is code 6.

**`clip_upload_url`** with `clip_type: "game"`:

| Request field | Rule |
|---|---|
| `clip_id`, `crew_id` | The clip is a game clip in the crew's clips document, and the caller is its clipper. |
| `poster_bytes` | 1 to 1 MiB |
| `video_bytes` | 1 to 64 MiB |
| `upload_id` | Optional. Set: new part URLs for this multipart upload, no new upload (a resume, or URLs that expired). |

| Response field | Content |
|---|---|
| `poster_upload_url` | A presigned PUT for `crews/{crew_id}/{clip_id}.jpg`. Signed headers: `Content-Type: image/jpeg`, `Content-Length` = `poster_bytes`, the `Cache-Control`. |
| `poster_url` | The public URL of the poster |
| `upload_id` | The multipart upload of `crews/{crew_id}/{clip_id}.mp4`, with `Content-Type: video/mp4` and the `Cache-Control` |
| `part_size` | 5 MiB. Every part has this size, except the last. R2 needs equal parts. |
| `parts` | One `{part_number, url, bytes}` per part. Each `url` is a presigned `UploadPart` with a signed `Content-Length`. |
| `media_url` | The public URL of the MP4 |
| `cache_control` | `public, max-age=31536000, immutable`: a clip object never changes |

The URLs expire after 1 hour. With no S3, every field is empty, as for a
voice clip.

**`clip_upload_complete`** with `clip_type: "game"`:

| `part` | Backend |
|---|---|
| `poster` | HEAD the poster: it exists, at most 1 MiB, `image/jpeg`. Set `poster_url` and push `feed_changed`. |
| `video` | `CompleteMultipartUpload` with `upload_id` and `parts` (`{part_number, etag}`). HEAD the MP4: it exists, at most 64 MiB, `video/mp4`. Set `media_url` and push `feed_changed`. An object over the cap is deleted. An upload that is gone (`NoSuchUpload`) goes on to the HEAD, so a retry after a lost answer works. |

**Cleanup.** `StartGameClipUploadCleanupJob` runs at start and each hour. It
aborts each multipart upload under `crews/` that started more than 24 h ago.
It lists with no prefix, because MinIO lists the uploads of one exact key
only.

The per-user daily clip limit and the retention of section 6.2 apply to game
clips too.

### 15.9 Playback

Playback streams the MP4 from its first bytes. It never downloads the whole
file first. The MP4 is progressive with the moov box first, so the first
range gives the index and the first frames. The same URL plays as is in a
browser `<video>` and in `AVPlayer`. There is no HLS: a single rendition
needs no playlist, and m3llo has no server transcode.

**The player in the client.** `ExpandedClipPlayer`
(`client/ui/panels/clip_player_view.slint`) opens over the feed area: play and
pause, the scrubber, BUFFERING, Space pauses, Escape closes. The picture fills
the panel. The native presenter is one per client, so a clip plays only while
no stream is watched, and a watched stream closes the clip. Commands:
`PlayGameClip`, `SetGameClipPaused`, `SeekGameClip`, `CloseGameClip`. Events:
`GameClipPlayback`, `GameClipPlaybackClosed`.

**The range reader** (`mello-core/src/clip_play/range_reader.rs`):

| Item | Rule |
|---|---|
| Ranges | 1 MiB (`CHUNK`), into a cache file in `mello_clips/play` of the OS temp folder, with a map of the ranges it has |
| Ahead | 4 ranges after the last read (`AHEAD_CHUNKS`): about 6 s at 720p60 |
| Order | A range that a read waits for comes first. |
| Give way | A range ahead in flight stops when a read waits for a range more than 4 ranges away (a seek). A range that a read waits for always completes. |
| Seek back | A read far behind the head moves the ranges ahead back to it. |
| Retries | 3. Connect timeout 10 s, request timeout 30 s. |
| Local file | The clipper plays the local file with no fetch (`LocalClip`). |

**The player in libmello** (`libmello/src/clip/`, 03-LIBMELLO.md §3 and §7):

- `ClipByteStream` is an `IMFByteStream` over the read callback. It reports
  `MFBYTESTREAM_IS_SEEKABLE`, its length and the content type `video/mp4`.
  `BeginRead` returns at once and reads on Media Foundation's work queue, so
  the source gives the first sample after the first range.
- The sound is the clock. A video frame is released on it, through the
  stream viewer's decoder and presenter (14-VIDEO-PIPELINE.md §8.4).
- Buffering: with sound, the state says buffering only when the sound
  stalls (`reports_buffering`). A read that waits while the sound plays on
  is a read ahead. Without sound, a read that waits is a stall. In a stall
  the picture holds and the sound pauses.
- A seek plays from the keyframe before the position.

Measured with the stream gate's replay lane (a 20 Mbps link with 50 ms delay,
an empty cache):

| Metric | cpu lane (720p60) | hw lane (1080p60) | Target |
|---|---|---|---|
| First frame from the start call | 528 ms | 607 and 611 ms | 1000 ms |
| Buffering after the first frame | 0 ms | 0 ms | 0 |
| Seek to 20 s | 944 ms | 1784 ms | 1000 ms |

### 15.10 Settings

Settings, Games, GAME CLIPS. Hidden on macOS.

| Control | Values | Default |
|---|---|---|
| Game clips (F8) | On, off | On |
| Clip quality | 1080p60 (8000 kbps), 720p60 (5000 kbps), 720p30 (3000 kbps) | 720p60 |

KEYBINDS has the Game clip row (section 15.2). The app picks the ring folder
(the OS temp folder). It is not a setting. There is no switch for game clips
while streaming: the rules of 12-STREAMING.md §3.7 keep the stream first.

### 15.11 Cost While the User Plays

Estimates at 720p60. The perf scenario that measures them is not built yet
(section 15.14).

| Resource | Estimate |
|---|---|
| GPU | One capture copy and one BGRA to NV12 convert per frame (0.1 to 0.3 ms), one hardware encode on the encoder engine |
| CPU | Under 2 % of one core. The threads wait on events. The game audio is PCM, with no encode. |
| Process RAM | The ring index, a 2 MB write queue, the encoder buffers. At most 10 MB more than without the replay. |
| Disk | About 72 MB of ring files, fixed after the start. About 0.8 MB/s of writes. |
| Install size | No new binary. Media Foundation and WIC are OS parts. |

### 15.12 Windows Parts

| Part | Rule |
|---|---|
| Game audio | WASAPI process loopback on the game's process tree (Windows 10 2004 or later), shared with a stream of the same game (12-STREAMING.md §3.7, F7). Endpoint loopback is not used: it records the crew voice and every other app twice. Measured on CS2 and Hearthstone (10 min each): a signal for the whole run, after the game's own volume and after its slider in the volume mixer. CS2 plays no sound in the background by default, so a clip of an alt-tabbed CS2 has no game audio, as the user heard. |
| Encoder | NVENC, tested. AMF, built and not tested on AMD hardware. QSV is off unless `MELLO_ENABLE_QSV=1`, so a PC with Intel graphics only has no game clips. |
| Exclusive fullscreen | WGC window captured CS2 in exclusive fullscreen. A game that WGC cannot see delivers no frames and gets no clips. |
| Consent | No borderless consent dialog shows: m3llo is signed. The replay asks for borderless access at its first WGC start, as the stream does. |

### 15.13 Tests

| Level | Tests |
|---|---|
| Unit (`check.sh`) | The game clip key and KEYBINDS rules (`hotkeys.rs`), the client flow tests (`flow_tests.rs`: the key, the toasts, the settings, the card), the replay worker rules (`replay.rs`), the upload worker against a local stub, the queue and the pace (`clip_upload/`), the upload budget (`upload_budget.rs`), the range reader, the Go tests (`game_clips_test.go`, `crew_feed_test.go`) |
| libmello ctest (`check-full.sh`) | `ReplayVideoRing`, `ReplayAudioRing`, `ReplayMux`, `ReplayMuxTime`, `ReplayMix`, `ReplayAvClock`, `ReplayGaps`, `ReplayLoad`, `ReplaySession`, `CaptureManager`, `SharedGameAudio`, `EncoderIntent`, `WindowCrop`, `ClipPlayer`, `ClipAudioStream` |
| Replay lane | `scripts/replay-gate.sh` (`stream-gate replay`, `tools/stream-gate/src/replay.rs`). The reference clip goes through the replay, with presses at 35, 47, 60 and 62 s. Gates in `benchmarks/baselines/replay/gates.json`: the first frame is T0 within one GOP, no frame is missing, a length of 30 to 31 s, PSNR, SSIM and VMAF floors, the game audio markers within ±20 ms of their frames. `--playback` adds the streamed playback row (`playback.rs`, `http_shim.rs`): the first frame within 1 s, no buffering, every frame in order, a seek to 20 s within 1 s. The QA harness runs the lane, not `check.sh`. TESTING.md has the commands. |
| Journey | `qa/journeys/feed-game-clip.ts` (FEED-12). An `e2e-game` build gives a fake game (`mello-core/src/e2e_game.rs`) whose replay records the reference clip. bob joins voice and presses F8 through `POST /game-clip-key` on the state port. bob's card shows the local poster. alice waits for the upload, plays the MP4 from local S3, pauses, seeks and closes the player. |

### 15.14 Not Built Yet

| Item | State |
|---|---|
| macOS | ScreenCaptureKit window capture with app audio, VideoToolbox, `AVAssetWriter`. Until then the Settings section is hidden and F8 does nothing on macOS. |
| The hw seek | 1784 ms against 1000 ms at 1080p60 (section 15.9). Before the seek the MPEG-4 source makes one or two more reads, and the keyframe spans two 1 MiB ranges. A fix needs a seek that starts at once (or a demux of our own) and a smaller first range. |
| Unread first frames on hw | In 2 replay lane runs the first frames of a play did not read. The cause is not known. |
| R2 and the signed `Content-Length` | MinIO refuses a part of the wrong length. R2 is not checked. |
| Gates | The hw lane floors, the real loopback row (`tone_player.cpp` is built and not run), the remote voice check of the journey (a tone burst from the second client within ±20 ms; the mic path removes a tone, so the burst needs a path past the noise suppression), the stream and replay together case (12-STREAMING.md §3.7, F8 and G7), the perf scenario (section 15.11), the QA harness checks. |
| The upload budget | 1 Mbps and 25 % of the headroom are starting values. The G7 gate confirms or moves them. |
| Auto highlights | A second iteration. A clip that the user did not choose needs a way to ask the user first (opt in, per user or per crew). Then: a trigger and a tag on `CaptureGameClip` and `post_clip`, the poster at the marker time, 45 ring segments (a marker at round end needs 35 s back), and the CS2 adapter (`telemetry/cs2_gsi.rs`) as the first marker source. |
| Stream clips, trim, share export | Sections 3.2 and 14. |
| iOS and the web lounge | The MP4 plays in `AVPlayer` and `<video>` as is. The cards there are follow-ups. |

---

*See CREW-EVENT-LEDGER.md for the underlying data layer. See SIDEBAR-REDESIGN.md for the companion sidebar changes.*
