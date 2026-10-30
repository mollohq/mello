# CLIPS

> **Component:** Crew Memory, Clips, Crew Feed  
> **Version:** 1.0  
> **Status:** v1 In Progress (durable crew memory landed: clips and recaps persist outside the ledger trim; dot-matrix waveform clip cards landed; next-generation feed phase 2 landed: hero priority, empty-card rule, skeleton trio)  
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

### 3.2 Stream Clip (Future)

Captured by a viewer while watching a crew member's stream. Contains:

- Last 30 seconds of stream video (H.264)
- Mixed voice chat audio from all participants during those 30 seconds
- Game audio from the stream

This is the hero clip type. A friend hits a clutch play, you tap clip, and you have a 30-second video with the gameplay AND everyone's reactions.

**Requires:** An active stream being viewed. The viewer's client maintains a rolling disk buffer of compressed video and mixed audio.

**Not in v1.** Ships in a later phase once streaming is stable.

---

## 4. Rolling Disk Buffer

Both clip types rely on a rolling buffer written to disk. The buffer holds the last 60 seconds of content, and when the user taps clip, they get the last 30 seconds from that buffer. Zero RAM overhead.

This is the same approach ShadowPlay, Medal, and OBS Replay Buffer use. Proven pattern.

### 4.1 Voice Buffer (v1)

Mixed audio output written to a temp file as Opus packets. 60 seconds of voice audio is well under 1MB on disk. Essentially free.

On clip capture: read back the last 30 seconds and mux into an MP4 container.

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
- Default location: OS temp directory
- Configurable in settings (for gamers who want it on a specific drive)
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
User taps clip button (scissors icon, bottom bar)
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

Production uses **Cloudflare R2** (S3-compatible, zero egress fees). Local dev uses **MinIO** in Docker.

Upload flow:

```
Client captures WAV → encodes to MP4/AAC locally
    │
    ▼
Client calls `clip_upload_url` RPC (clip_id, crew_id)
    │
    ▼
Backend generates presigned PUT URL (15 min TTL) via AWS SDK
Object key: crews/{crew_id}/{clip_id}.mp4
    │
    ▼
Client HTTP PUTs MP4 directly to R2/MinIO (no backend proxy)
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
| `S3_ENDPOINT` | `http://minio:9000` | `https://<account>.r2.cloudflarestorage.com` | S3 API endpoint (internal) |
| `S3_PRESIGN_ENDPOINT` | `http://localhost:9000` | *(not set, falls back to S3_ENDPOINT)* | Endpoint used in presigned URLs (client-reachable) |
| `S3_BUCKET` | `mello-clips` | `mello-clips` | Bucket name |
| `S3_ACCESS_KEY` | `minioadmin` | R2 API token access key | S3 credentials |
| `S3_SECRET_KEY` | `minioadmin` | R2 API token secret key | S3 credentials |
| `S3_PUBLIC_URL` | `http://localhost:9000/mello-clips` | `https://clips.m3llo.app` | Public base URL for media_url construction |

`S3_PRESIGN_ENDPOINT` exists because in Docker, Nakama reaches MinIO via `minio:9000` (internal DNS), but presigned URLs must be reachable by the client on the host. In production (R2), the endpoint is the same for both, so this var is omitted.

See [05-GETTING-STARTED.md](../05-GETTING-STARTED.md) for R2 setup instructions.

---

## 7. Center View: Crew Feed

The center area of the app becomes contextual based on crew activity.

### 7.1 State: No Active Stream (Default)

Center shows the **Crew Feed**: a bento grid layout with mixed-size cards, newest content at top. The feed is server-curated via the `crew_feed` RPC (see section 11.4). Deep scroll falls back to the raw `crew_timeline` / `crew_clips` / `crew_recaps` RPCs.

Card types in the bento grid:

- **Clip card** — single cell, always the standard size. Grey label, ring and dots; an unread clip wears red. Dot-matrix waveform, play ring, clip badge, duration, who clipped it, game tag, participant avatars, timestamp. Tap the wave to play from that point; drag to scrub. A voice clip is never the hero and never takes the wide cell.
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
3. A belt that moved in the last 24 hours. The head2head entry is the hero (HEAD2HEAD.md section 5). `beltMovedHeroIndex` stays an empty hook: the client builds the card.
4. A real recap, on Monday (UTC), while the recap is under 24 hours old.
5. The best session: the best stream preview of any age.

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
- Under a hero the poster grid of recent games takes the wide slot of row 3 (frames C and D). The one-PC list stays in the side column.
- Head2head is always two columns wide, at every crew size. It leads a feed with no hero. Behind any other lead it takes the first wide slot after the lead (row 2, else row 3), never the one-column side slot. After one live stream it leads the grid beside the stream. A feed with no hero, head2head and a recap with content puts the recap second, tall (TallTrio): two plain cells sit under head2head.
- Row 2 is the last row and has free cells: the stream and voice clip skeletons fill them at the card height, and the recap skeleton is the slim row under them (frame B). A full last row keeps the trio as one row.
- A recap holds the whole side column beside the two-stream hero.
- The weekly recap is never a one-cell card. It has two shapes: the tall card (one column, two rows) in the side column beside a hero, beside one live stream or in frame F, and the wide card (two columns, one row: the week, the belts moved and the belts held side by side). When the side column is taken, the recap takes the first wide slot of the next row, and the poster grid moves to the wide slot of the row after it (`feed_grid.rs`, `place_wide_cards`).

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

Streaming is initiated via the broadcast icon in the action bar (control bar, always visible).

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

Appears as a card in the crew feed. NOT posted to chat. Tapping expands to a full-screen stats view.

The tall card (one column, three grid rows with belts, two without) shows, top to bottom: the label and the week; hangout, clips, longest and top game with its art, two by two; Most active and Most clipped, one line each; Belts moved (two rows, then "+N more"); Belts held (octagon, count, "belts"); Games played (four small art tiles with hours); and "See the week" in a row of its own. A week with no belts drops both belt sections. The server sends `most_clipped`, `most_clipped_user_id`, `most_clipped_count`, `clip_of_week` (the longest clip of the week: id, title, duration, game, media) and `games_played` with `minutes` for each game. A card carries no note that explains it: no day, no source label.

"See the week" opens the recap in the feed panel, min(880 px, panel less 48 px) wide and centred: this week (hangout, top game, longest, clips, most active, most clipped), every belt that moved with "was held by" and "now held by", every member with rank, count and titles, the clip of the week with a play ring that plays it through the clip player, and every game with its hours. A section the week did not have is left out.

---

## 11. Crew Feed Data

### 11.1 Event Ledger (Existing)

Voice sessions, stream sessions, game sessions, chat activity summaries. These are the ephemeral pulse events, stored in the 7-day rolling ledger `crew_events/{crew_id}`. They generate session cards, daily recap cards, now playing cards, and recent games cards. Clips and recaps are not ledger events; stream sessions are also mirrored to a durable store (§11.3a) so replays survive the trim.

### 11.2 Clips (Durable)

Clip metadata is durable, stored in its own per-crew document `crew_clips/{crew_id}` (capped at most-recent 250). Each `StoredClip` carries:

- event_id (time-sortable, used as the pagination cursor), clip_id, clip_type (voice, stream in future)
- media_url (populated after S3 upload by `clip_upload_complete`)
- local_path (clipper's machine)
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
  - `this_week`: the curated recent feed (replaces the client-side bento ordering). Hero precedence is in section 7.1 (`heroIndex` in `crew_feed.go`). A clip is never the hero and never gets the wide size. The latest recap is pinned, then the daily recaps, then diversity/priority fillers capped at 7 slots with one wide/lg promotion. Empty cards are dropped first (`cardHasContent`).
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

*See CREW-EVENT-LEDGER.md for the underlying data layer. See SIDEBAR-REDESIGN.md for the companion sidebar changes.*
