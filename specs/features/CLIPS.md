# CLIPS

> **Component:** Crew Memory, Clips, Crew Feed  
> **Version:** 1.0  
> **Status:** v1 In Progress  
> **Depends on:** CREW-EVENT-LEDGER.md, 13-VOICE-CHANNELS.md  
> **Mockups:** m3llo-crew-feed-mockup-v4.html (populated), m3llo-crew-feed-cold-start-full.html (empty)

---

## 1. What This Is

Clips let crew members capture what just happened: someone saying something hilarious in voice chat, a clutch play while streaming, a heated argument. One tap, 30 seconds saved.

The crew feed is where clips live alongside session summaries, catch-up cards, weekly recaps, and game activity. Together they form the crew's memory.

When nobody is streaming, the center view of the app shows this feed. When someone goes live, the stream takes over. When the stream ends, the feed comes back with the session added to it. m3llo always has something to show.

This is also the foundation for m3llo+ monetization. Free tier retains clips for 7 days. m3llo+ keeps them permanently.

---

## 2. Why This Matters

Discord dumps you into a text channel. m3llo greets you with what your crew has been up to.

The goal is to make m3llo feel alive even when nobody is online. Every time someone opens the app, there's something to see: a clip from last night, a session summary, a catch-up card showing who played what. The crew has a pulse.

Clips are also m3llo's primary organic growth channel. Every clip shared externally is an ad for m3llo that costs nothing. No other Discord alternative has anything like this. Revolt, Guilded, Element: all text channel clones. Crew memory is m3llo's product differentiator.

---

## 3. Clip Types

### 3.1 Voice Clip (v1)

Captured during a voice session. Audio only. Contains:

- Last 30 seconds of mixed voice audio from all participants
- Played back with a waveform visualization and crew member avatars

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
    │
    ▼
MP4 saved to local disk immediately
    │
    ▼
Clip metadata sent to backend via RPC: post_clip
    (clip_id, crew_id, clip_type, game, participants, timestamp, duration)
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
Backend updates clip event's media_url in the ledger
    │
    ▼
Clip is now playable by all crew members via public media_url
```

The R2 bucket (`mello-clips`) has public read access. `media_url` points directly to the public endpoint — no signed download URLs needed.

**Free tier:** Clips available for 7 days. After 7 days, cloud copy deleted (lifecycle rule). Clipper still has local copy.

**m3llo+ (future):** Clips stored permanently.

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

Center shows the **Crew Feed**: a bento grid layout with mixed-size cards, newest content at top. Infinite scroll with cursor-based pagination from the `crew_timeline` RPC.

Card types in the bento grid:

- **Hero clip card** — spans 2 columns and 2 rows. Most recent or highest-ranked clip. Waveform visualization with play button for voice clips. Shows clip badge, duration, who clipped it, game tag, participant avatars, timestamp.
- **Clip card (standard)** — single cell. Waveform, clip badge, metadata. Tap to play.
- **Session card** — "Your crew played Valorant for 3 hours." Game icon, participant avatars, duration, clip count. From event ledger.
- **Catch-up card** — "ostkatt and FaiL hung out in General for 45m while you were away." Green label. Promoted from sidebar to center stage.
- **Now playing card** — real-time game sensing. Game icon, who's playing, duration. Live data.
- **Recent games card** — list of games played recently across the crew.
- **Weekly recap card** — stats summary for the week. Red label. Generated weekly by backend job.
- **Skeleton cards** — placeholder cards for cold start (see section 8).
- **Invite card** — CTA card in brand red tint (see section 8).

Streaming is initiated via the broadcast icon in the action bar (control bar, always visible).

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

A brand new crew with zero activity sees a full grid of skeleton cards.

### 8.1 Skeleton Card Types

Each skeleton is a ghost version of its real counterpart: dashed borders, placeholder shapes, brief text explaining what will appear.

- **Hero clip skeleton** — dashed clip badge, ghost play button, ghost duration badge. "Your first clip goes here. Tap ✂ during a voice call to save the last 30 seconds."
- **Voice clip skeleton** — barely-visible waveform bars at low opacity. "Voice clips land here. Funny moment in voice? Clip it."
- **Weekly recap skeleton** — gray placeholder bars where stats would be. "Your crew's stats will appear here after your first week together."
- **Now playing skeleton** — "Games your crew is playing show up here automatically."
- **Session skeleton** — ghost game icon and placeholder bars.
- **Catch-up skeleton** — ghost text bars in catch-up layout.
- **Recent games skeleton** — ghost game icon rows.
- **Stream clip skeleton** — "Stream clips coming soon. Clip highlights while watching a crewmate's stream."
- **Invite card** — the only non-skeleton card. Subtle CTA in brand red. "Invite your crew. Everything here comes alive when your friends join. Get them in here."

### 8.2 Progressive Replacement

As the crew uses m3llo, skeleton cards get replaced by real content:

1. First voice session → session card and catch-up card replace their skeletons
2. First game detected → now playing and recent games replace their skeletons
3. First clip captured → hero clip and voice clip skeletons replaced
4. First week complete → weekly recap skeleton replaced

### 8.3 Visual Treatment

Skeleton cards use: dashed borders (#262629), slightly darker background (#1b1b1f), all text and icons at very low opacity (#333 to #444), no animation or pulsing. They are empty slots waiting to be filled, not loading indicators.

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

A backend job runs weekly (Monday 00:00 UTC) and generates a recap card from event ledger data.

### 10.1 Contents

- Total crew hangout time (voice + stream hours combined)
- Top game played
- Longest session (who, what game, how long)
- Number of clips captured
- Most active member
- Most clipped member

### 10.2 Presentation

Appears as a card in the crew feed. NOT posted to chat. Tapping expands to a full-screen stats view.

---

## 11. Crew Feed Data

### 11.1 Event Ledger (Existing)

Voice sessions, stream sessions, game sessions, chat activity summaries. These generate session cards, catch-up cards, now playing cards, and recent games cards.

### 11.2 Clips (New)

Clip metadata stored alongside event ledger data:

- clip_id, clip_type (voice, stream in future)
- media_url (populated after S3 upload)
- local_path (clipper's machine)
- thumbnail_url (waveform image for voice clips)
- participants (user IDs at clip time)
- game (if detected via game sensing)
- duration_seconds, clipped_by (user ID)

### 11.3 Retrieval

`crew_catchup` RPC extended to include clips (ranked higher than passive events).

New `crew_timeline` RPC returns paginated timeline data with cursor-based pagination.

---

## 12. Open Questions

- **Clip playback for offline clippers.** Show "waiting for upload" or skip until available?
- **Clip deletion.** Clipper can delete own clips in v1. No crew-wide moderation yet.
- **Voice clip consent.** Joining a voice channel implies consent for v1. Consider showing a notification ("ostkatt is clipping") when clip button is tapped. Legal review needed before wide EU launch.
- **Clip audio mixing.** Buffer captures clipper's playback. Muted users are not in the clip. Two people clipping the same moment get different audio. This is correct behavior.
- **Bento grid layout algorithm.** For v1: most recent clip gets hero position (2x2), everything else fills chronologically as 1x1. More sophisticated ranking later.
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
- All card types: hero clip, clip, session, catch-up, now playing, recent games, weekly recap
- Cold-start skeleton cards for all types
- Invite CTA card
- Contextual center view (feed vs active stream)

**Backend:**
- `post_clip` RPC storing clip metadata in event ledger
- `crew_timeline` RPC returning paginated feed data
- S3 integration for clip upload and playback URLs
- Weekly recap generation job (Monday 00:00 UTC)

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
