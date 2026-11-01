---
name: Game clips (F8)
overview: "F8 saves the last 30 seconds of the user's own game: video, game audio and the crew voice, in one MP4. A local replay buffer runs while the user is in voice and a game is detected, also when the user does not stream. On by default. Windows first (WGC window capture, NVENC/AMF), macOS after Windows."
todos:
  - id: capture-hub
    content: "libmello: one game capture, many consumers. Refactor PR with no behavior change, proven by the stream gate"
    status: pending
  - id: loopback-check
    content: "Windows PC: measure WASAPI process loopback on a game process (signal present, before or after the game volume) before the ring is built on it"
    status: pending
  - id: replay-ring
    content: "libmello: disk ring of encoded video and game audio PCM, voice tap with heard times, pinning during a capture"
    status: pending
  - id: replay-session
    content: "libmello: mello_replay_* C API, recording encoder profile, replay ladder (the hook only when it is already the active capturer, WGC window, no monitor capture)"
    status: pending
  - id: mp4-mux
    content: "libmello: MP4 mux on Windows (IMFSinkWriter, H.264 passthrough, AAC, moov first) and the poster JPEG"
    status: pending
  - id: core-lifecycle
    content: "mello-core: replay manager (setting, foreground game, disk, encoder), CaptureGameClip, post_clip with clip_type game"
    status: pending
  - id: upload
    content: "mello-core: streamed and paced upload of the MP4 and the poster, no full-file read into RAM"
    status: pending
  - id: backend
    content: "Nakama: validate clip_type, game clip fields, presign video/mp4 and image/jpeg with a size cap, Go tests"
    status: pending
  - id: client-key-settings
    content: "Client: F8 listener and gate, Settings section, HUD toast, testkit tests"
    status: pending
  - id: feed-card-playback
    content: "Client: game clip card with poster, playback that streams the MP4 from R2 with HTTP range requests, through the viewer pipeline in the expanded view"
    status: pending
  - id: gates
    content: "Replay lane in tools/stream-gate, perf harness scenario, QA harness checks, one mello-driver journey"
    status: pending
  - id: specs
    content: "Move the shipped parts into CLIPS.md, 03-LIBMELLO.md, 12-STREAMING.md and 14-VIDEO-PIPELINE.md"
    status: pending
  - id: macos
    content: "macOS: ScreenCaptureKit window capture with app audio, VideoToolbox, AVAssetWriter"
    status: pending
isProject: false
---

# Game Clips (F8)

> **Status:** Plan. Nothing is built. Bob answered the open questions on 2026-10-31 (section 13).
> **Specs:** [CLIPS.md](../specs/features/CLIPS.md) sections 3 to 6, [03-LIBMELLO.md](../specs/03-LIBMELLO.md), [12-STREAMING.md](../specs/12-STREAMING.md), [14-VIDEO-PIPELINE.md](../specs/14-VIDEO-PIPELINE.md), [00-ARCHITECTURE.md](../specs/00-ARCHITECTURE.md) section 2.
> **Builds on:** F7 voice clips (CLIPS.md 3.1, branch `fix/playtest-fixes`).

---

## 0. Start here (Windows)

Steps 1, 2 and the gates run on the Windows PC: the capture code builds only on Windows (`if(WIN32)` in `libmello/CMakeLists.txt`), and the stream gate's reference source is Windows only. Work in this order.

### 0.1 Set up

1. Read the workspace `CLAUDE.md` and `mello/CLAUDE.md`. Read `specs/03-LIBMELLO.md` in full before you touch `libmello/src/audio/` or `libmello/src/video/`. Then `specs/14-VIDEO-PIPELINE.md`, `specs/12-STREAMING.md` sections 3, 9 and 13, and `TESTING.md` ("Streaming quality gate").
2. Make a worktree on `feat/game-clips` (one branch for the whole feature). Run `git submodule update --init --recursive` in it. Do not link vcpkg from another checkout.
3. Build: `libmello/build.ps1`, then `./scripts/check-full.sh`. It must be green before you change anything.
4. Use the local stack only (Nakama, MinIO and the SFU in Docker). Never point a test or a tool at production.

### 0.2 First: the voice clip ring on Windows

Commit `570e8ec` moved the voice clip buffer to a ring file on disk. It ran only on macOS. Test it on Windows before other work:

1. `ctest --test-dir libmello/build-ci -R ClipBuffer --output-on-failure`, 8 tests.
2. Run the client against the local stack. Join voice with a second client that talks. Press F7. The clip card plays the voice.
3. While in voice, `%TEMP%\mello_clips` holds one `voice_ring_<pid>_<n>.bin` of 768000 bytes. Windows deletes it on close, so it shows while the buffer runs.
4. Leave voice: the file is gone. End the client process from Task Manager while in voice: the file is gone too.
5. Report the result to Bob. He decides whether the fix gets its own PR from this branch.

### 0.3 Step 2a: measure process loopback (a report, no code)

The CS2 recording got digital silence from a process loopback capture (section 14). Measure it before the ring is built on it:

1. Use the existing loopback code (`capture_wasapi_loopback.cpp`, `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`) through `tools/stream-host --capture-backend process` on the game window, or a small test that opens it on a pid and writes a WAV.
2. Games: CS2 and one other (a game with a launcher process, so the process tree matters). 10 minutes each, in a match, with m3llo in voice.
3. Record per game: is there a signal for the whole run (RMS per second, the share of silent seconds), does the level follow the in-game volume, the Windows volume mixer slider for the game, or neither.
4. Write the result into this plan, section 9.1. If the signal is not reliable, stop and ask Bob before step 2b. Endpoint loopback is not a fallback (section 9.1).

### 0.4 Step 1: write the capture hub design, then build it

Section 3.2 gives the goal, not the design. Read `capture_process.cpp`, `video_pipeline.cpp`, `capture_screencapturekit.mm` and the stream host start in `mello.cpp` first. Then add a section 3.5 to this plan that answers:

- Which thread hands a frame to each consumer, and what a slow consumer does to the others (it must not stall the capture or the other consumer).
- How a consumer is added and removed while frames are in flight (the stream starts and stops while the replay runs).
- How the per-capture events reach every consumer: a method change (a keyframe for each encoder), `state()` for the pause card, `target_exited`, the exclusive-fullscreen restart, a source size change.
- Who owns the `GraphicsDevice` and the hub, and what happens when the device is poisoned (12-STREAMING.md 3.1).
- Whether the macOS ScreenCaptureKit path moves into the hub now or in the macOS phase.
- What does not change: the stream C API (`mello_stream_*`) and the stream behavior.

Get Bob's OK on section 3.5, then build it as its own PR.

"Stream gate unchanged" means:

1. On `main`, run `./scripts/stream-gate.sh --lane cpu --repeat 3` and `--lane hw --repeat 3`, on a quiet machine (the load guard). Keep `results.json` of each.
2. On the branch, the same runs. Every profile passes `benchmarks/baselines/stream/gates.json` and `audio.json`.
3. Put the main and branch values side by side in the PR. A change outside the spread of the main runs is a regression, also inside the gates.

### 0.5 Then

Step 2b and the rest follow section 11. Each todo id in the front matter is one piece of work. Mark it done in the front matter when it lands.

---

## 1. What F8 does

F8 saves the last 30 seconds of the user's own game as one MP4:

- the game video,
- the game audio,
- the crew voice that the user heard, and the user's own mic.

It works like a ShadowPlay replay buffer. The buffer runs while the user plays and is in voice. It does not need a stream. The crew voice is part of every game clip, so the buffer does not run out of voice.

F8 is a new clip type, `game`. It is not the stream clip of CLIPS.md 3.2. A stream clip is a viewer's clip of a crew member's stream. A game clip is the user's own game.

| Key | Clip | Needs |
|---|---|---|
| F7 | Voice clip (audio only) | In voice, mic access |
| F8 | Game clip (video, game audio, voice) | In voice, and the replay buffer runs (section 4) |

---

## 2. Current state (verified on this branch)

| Item | State |
|---|---|
| F7 | `client/src/platform/hotkeys.rs`: `register_clip`, `clip_key_step`, `poll_clip`. `client/src/poll_loop.rs`: `clip_key_pressed` runs the CLIP 30S callback. |
| Voice clip buffer | `libmello/src/audio/clip_buffer.hpp`. It was a 60 s PCM ring in RAM (5.76 MB), against CLIPS.md 4.1. Fixed on this branch: a 60 s ring file of Opus frames on disk, with a small RAM queue between the render thread and a writer thread. The render path writes the mix plus the local mic (`local_clip_ring_`). |
| Voice clip API | `mello_clip_buffer_start/stop/active`, `mello_clip_capture` (WAV out), `mello_clip_encode` (WAV to MP4/AAC, WMF or AudioToolbox). |
| Voice clip key | Bindable in Settings, KEYBINDS (`Settings::clip_key`, F7 by default, commit `2363f12`). |
| Video clip buffer | None. |
| Host capture and encode | Exists only inside a stream host (`MelloStreamHost`, `VideoPipeline::start_host`). The capture ladder, the preprocessor and the encoder factory are reusable code. |
| Game audio capture | Exists only inside a stream host: WASAPI process loopback on the game pid (12-STREAMING.md 9.0), ScreenCaptureKit audio on macOS (9.2). |
| Upload | `mello-core/src/client/clip.rs` `handle_upload_clip` reads the whole MP4 into RAM and PUTs it with `Content-Type: audio/mp4`. |
| Backend | `clips.go`: `clip_type` defaults to `voice` and is not validated. `clip_upload_url` presigns `crews/{crew_id}/{clip_id}.mp4` with `audio/mp4` and no size limit. A per-user daily clip limit exists. |
| F8 | Not bound anywhere in the client. |

---

## 3. Where the rolling video buffer lives

### 3.1 Decision: same code, own consumer

The replay buffer uses the host path code: `ProcessCapture`, `VideoPreprocessor`, `EncoderFactory`. It does not use the stream's encoder output. It has its own preprocessor and its own encoder session.

Reasons:

- The stream encoder follows the network. The quality controller drops it to 540p, 480p or 360p and lowers the frame rate under congestion (12-STREAMING.md 8.1.1). A clip must not get worse because the uplink is bad.
- The stream encoder is tuned for latency (ULL, 4 s GOP). A clip needs a short GOP so it starts close to 30 s back.
- The replay runs when the user does not stream. Most of the time there is no stream encoder.

### 3.2 One capture, two consumers

A game can have one capture only. The hook takes one client. Two WGC sessions on one window double the copy cost. So the capture moves out of `VideoPipeline` into a shared object:

```
                        ┌─▶ stream consumer: preprocessor ─▶ encoder (ULL, adaptive) ─▶ RTP
ProcessCapture (one) ───┤
                        └─▶ replay consumer: preprocessor ─▶ encoder (recording) ─▶ disk ring
```

- `GameCaptureHub` (name to confirm) owns one `ProcessCapture` for one game pid, on the shared `GraphicsDevice`.
- Each consumer registers a frame callback. The hub hands each consumer the same BGRA texture and capture time. Each consumer has its own NV12 ring (3 slots), so the two encoders never alias.
- The stream host becomes a consumer. Its behavior does not change.
- Step 1 is the hub refactor alone, in its own PR. The stream gate (CPU and hw lanes) must show no change. This follows the rule "do not refactor working code as part of a feature PR".

### 3.3 Replay capture ladder

The replay buffer records in the background for hours. It must not show the desktop, and it must not show a border around the game.

| Ladder step | Replay | Reason |
|---|---|---|
| Hook | Only when the hook is already the active capturer: a stream that uses the hook shares its capture through the hub. The replay never injects the hook itself. | Bob, Q4. |
| WGC window | Yes | Captures only the game window. |
| WGC monitor | **No** | Records the whole monitor: notifications, other windows, private messages. |
| DXGI desktop duplication | **No** | The same reason. It also wedges on some NVIDIA drivers (12-STREAMING.md 3.1). |

When no allowed step delivers frames, the replay buffer is off for that game. F8 then shows "Clips are not available for this game" in the HUD toast.

The border: Windows 11 hides the WGC border with `IsBorderRequired(false)` when the user allows it. Windows 10 always shows the border (`plans/STREAM-CAPTURE-QUALITY.md` 1.1). Game clips run on Windows 10 with the border (Bob, Q3).

### 3.4 The recording encoder profile

| Setting | Stream (today) | Replay |
|---|---|---|
| Codec | H.264 | H.264, High profile |
| Size | Preset height, source aspect, adaptive rungs | Clip quality height (section 7), source aspect (`encode_size_for`), fixed |
| Frame rate | Preset, adaptive | 60 or 30, fixed |
| GOP | 4 s plus IDR on demand | 1 s, no IDR on demand |
| Rate control | VBR 1.25x, 0.5 s VBV | VBR 1.25x, 1 s VBV |
| NVENC tuning | P4 ULL, then P1 | P4 high quality tuning, no ULL |
| Cost tiers | Yes | Yes. The same overload signal. Below tier 2 the replay drops to 30 fps. |
| Bitrate controller | Yes | No |

The 1 s GOP sets the start error: a clip starts at the last keyframe at or before T minus 30 s, so a clip is 30 to 31 s long.

---

## 4. When the buffer runs

### 4.1 Rules

The replay buffer runs when all of these are true:

| Condition | Source |
|---|---|
| The Settings toggle "Game clips" is on | `client/src/settings.rs`. Default: on (Q1). |
| The user is in voice | The voice session. The voice clip ring runs (Q5). |
| Game sensing reports a running game with a window | `mello-core/src/game_sensing.rs` (`ActiveGame.pid`) |
| The game window is in the foreground, or was the last game window in the foreground | Foreground monitor (the HUD already uses one) |
| A hardware encoder exists | `EncoderFactory`. No software encoder in the product (14-VIDEO-PIPELINE.md 6.7). |
| A replay ladder step delivers frames | Section 3.3 |
| The buffer folder has at least 2 GB free | CLIPS.md 4.4 uses 100 MB for audio. Video needs more headroom. |

Voice is a condition (Q5). A game clip always has the crew voice in it.

### 4.2 Lifecycle

| Event | Action |
|---|---|
| A game starts and the rules hold, or the user joins voice while a game runs | Start the replay consumer. Make the ring folder. |
| The user leaves voice | Stop. Delete the ring. |
| The foreground game changes | Stop the old consumer, delete its ring, start one for the new game. One replay at a time. |
| The game is minimized | The capture holds (no frames). The ring keeps its content. A clip shows the last frame for the gap. |
| The game exits | Stop. Delete the ring. |
| The user turns the setting off | Stop. Delete the ring. |
| Disk free goes under 2 GB | Stop. Delete the ring. HUD toast once. |
| The client starts | Delete leftover ring folders of a crash. |

### 4.3 F8 key

F8 follows the voice clip key pattern in `hotkeys.rs` and `poll_loop.rs` (`register_clip`, `clip_key_step`, `poll_clip`):

- `DEFAULT_GAME_CLIP_KEY = "F8"`. A second row in Settings, KEYBINDS: Game clip, stored as `Settings::game_clip_key` in the `ptt_key` format.
- The listener matches each registered clip key with its modifiers and sends which clip it saw. F7 and F8 keep separate `held` state.
- `game_clip_key_pressed(app)` runs the new `game-clip-captured` callback only when the replay state in the UI is `recording`.
- A held key makes one clip. KEYBINDS refuses a key that push-to-talk or the voice clip key uses.
- A press when the buffer is off: no clip. The HUD toast says why (not in voice, off in Settings, no game, not available for this game).
- Two presses inside 30 s make two clips. Both clips pin the segments they need (section 5.2).

### 4.3a The key is the only trigger in this iteration

A clip starts only from the game clip key. Auto highlights (a clip that game telemetry starts) are the second iteration (section 12). They need a way to ask the user first. The capture call takes a time window, so the second iteration adds a trigger without a new capture path.

### 4.4 Which crew gets the clip

The active crew (Q6). A user is always in a crew.

---

## 5. Buffers and A/V sync

### 5.1 One clock

Every sample carries a time on the steady clock (`mello_clock_now_us`). The mux places all three sources on that clock.

| Source | Time of a sample | Notes |
|---|---|---|
| Video | The capture time of the frame | WGC `SystemRelativeTime`, or the hook's present time. The pipeline already carries it. |
| Game audio | The loopback device position of the buffer (`GetBuffer` QPC position), as steady clock | Process loopback captures what the game plays, at the time it plays it. |
| Voice | The render time of the 10 ms mix plus the output latency | The time the user heard it. The same sum that `AudioPipeline` gives the echo canceller (03-LIBMELLO.md 4, stream-delay hints). |

### 5.2 Rings

| Ring | Where | Content | Size at the default quality |
|---|---|---|---|
| Video | Disk, in the buffer folder | Annex-B access units in segment files. One segment is one GOP (1 s) and starts with an IDR plus SPS and PPS. A small index per segment: capture time, size, offset, keyframe flag. | 40 segments, about 25 MB at 5 Mbps (720p60) |
| Game audio | Disk, same folder | 48 kHz stereo int16 PCM in a ring file, with a time anchor every 10 ms chunk | 40 s, 7.7 MB |
| Voice | Disk, the existing `ClipBuffer` ring file | Opus frames of the 48 kHz mono mix, 60 s. Add a time anchor per frame. The mux decodes the window. | 0.8 MB (exists on this branch) |

- Segment files are preallocated and reused in turn. No file is created or deleted while the game runs.
- 40 segments: the 30 s window, one GOP for the keyframe before it, and spare segments for pinning. The segment count is one constant. The second iteration raises it to 45 (a marker at round end needs 35 s back, section 14).
- A capture pins the segments it reads. The writer skips a pinned segment and takes the next free one.
- The writer thread is not the encode thread. The encode thread hands each access unit to a bounded queue. A full queue drops the oldest unit and counts it. Disk stalls never block the encoder.
- The game audio is PCM, not Opus. PCM needs no encode while the game runs, and the mux needs no decode. The cost is disk space, not CPU.

### 5.3 Capture and mux

```
F8 ─▶ client gate ─▶ Command::CaptureGameClip ─▶ mello-core worker thread
      ─▶ mello_replay_capture(ctx, start_us, end_us, mp4_path, poster_path)
         1. end = now, start = end minus 30 s.
         2. T0 = the last keyframe at or before start.
         3. Pin the segments from T0 to end. Copy the voice ring window.
         4. Mux: video passthrough, audio mix, AAC encode.
         5. Unpin. Write the poster JPEG.
      ─▶ Event::GameClipCaptured { clip_id, path, poster_path, duration, width, height }
      ─▶ post_clip (clip_type "game") ─▶ upload (section 6.3)
```

- The mux runs on a worker thread, never on the command loop.
- Video: H.264 passthrough. No decode, no encode. Sample durations come from the capture times, so a game that renders at a variable rate stays in time.
- Audio: one stereo AAC-LC track, 160 kbps. The mux places game audio and voice on a 48 kHz timeline from T0. The voice is mono, center.
- Drift: each source gets a linear resample ratio from its own time anchors over the window. 300 ppm over 30 s is 9 ms. The ratio removes it.
- Gaps (minimized game, a dropped audio chunk) become silence on their own timeline position. Nothing moves the audio that follows a gap.
- Mix: game and voice at unity gain, then a limiter (Q9). The levels are what the user heard only if loopback captures after the game's own volume. To measure on the Windows PC.
- Poster: the newest captured frame at the press, copied to a staging texture once, written as JPEG (WIC on Windows). It costs one copy, only at a press.

### 5.4 The MP4 file

| Item | Windows | macOS |
|---|---|---|
| Muxer | `IMFSinkWriter`, H.264 input type equal to output type (passthrough), AAC encoder MFT | `AVAssetWriter`, video input with `outputSettings: nil` (passthrough) |
| moov first (web and phone playback start before the download ends) | `MF_MPEG4SINK_MOOV_BEFORE_MDAT` | `shouldOptimizeForNetworkUse = true` |
| New dependency | None. Media Foundation is already linked (`clip_encoder_wmf.cpp`). | AVFoundation framework (OS, no install size) |

---

## 6. Feed, playback, upload and storage

### 6.1 The card

A game clip is a `clip` entry with `clip_type: "game"`. The card is footage, like the stream preview card (CLIPS.md 7.1), not the waveform card.

- The poster edge to edge, one corner cut. Black washes under the text, no graphite ramp over the footage.
- Top left: the CLIP tag. An unread clip wears red, as every clip card does.
- A play ring (white outline, black disc) at the center.
- Bottom left: the clipper octagon, the clipper name, the game. Bottom right: the length on a black chip ("0:31").
- No filled white button. The play ring is the action.
- Size: a standard cell, or the hero. A game clip can be the hero. Its place in the hero priority is after a stream preview from the last 24 hours (Q7). A voice clip stays never the hero.
- The skeleton trio cell "Voice clips land here" also hides when a game clip exists.
- Chat: the clip message card shows the poster in place of the waveform.

The design follows `designs/design-system.html`. A mockup comes first, and Bob approves the render before any push.

### 6.2 Playback: stream from R2, no download first

A viewer never waits for a whole file. Playback starts from the first bytes and fetches the rest while it plays. A 30 s clip at the default quality is about 20 MB, but the rule is the same at any size.

**Delivery: progressive MP4 over HTTP range requests.**

- The MP4 has the moov box first (section 5.4). The first range request (about 1 MB) gives the index and the first second of video, so playback starts after one round trip and one chunk.
- A seek is a range request at the byte offset of the keyframe before the seek time. The index in the moov box gives the offset.
- R2 serves range requests on the public bucket through the `clips.m3llo.app` custom domain, and the Cloudflare cache holds the ranges. The upload sets `Cache-Control: public, max-age=31536000, immutable`: a clip object never changes.
- The same URL plays as is in a browser `<video>` (web lounge) and in `AVPlayer` (iOS). Both stream progressive MP4 with range requests.

**Why not HLS.** HLS helps when a clip has several renditions and the player picks one by bandwidth. That needs a transcode on a server, which m3llo does not have. A single-rendition HLS clip is the same bytes as the MP4, cut into many objects with a playlist: more uploads, more objects, and Chrome needs hls.js. Revisit HLS when clips get a second rendition or get much longer.

**Desktop client path.** The client reuses the viewer pipeline (decode thread, 3-slot ring, present thread) and the DComp presenter, with a clip source in place of the RTP receive timeline.

| Step | Windows | macOS |
|---|---|---|
| Fetch | `ClipRangeReader` in mello-core: range requests of 1 MB, kept 3 s ahead of the play position, written into the clip cache file at their offsets (disk, size cap, oldest clip out). A replay reads the cache. The clipper plays the local file. | Same |
| Demux | Media Foundation source reader over an `IMFByteStream` that reads from the range reader. A read of bytes not yet fetched waits for that range, with a timeout. | `AVAssetReader` over a local file does not stream. Use `AVURLAsset` with an `AVAssetResourceLoaderDelegate` that answers from the range reader. |
| Decode | The existing decoder factory (NVDEC, AMF, OpenH264) | The existing VideoToolbox decoder |
| Present | The existing present thread and the DComp visual | The existing RGBA path to the Slint timer |
| Audio | AAC from the same demux, decoded and played through the clip playback path. `mello_clip_play_mp4` decodes a whole local file first, so it gets a streaming variant. | Same |
| Sync | The audio is the master. The present thread shows the frame whose time is at or before the audio position. | Same |
| Stall | The fetch falls behind: the picture holds, the audio pauses, a buffering state shows on the player, and play goes on when 1 s is ahead again. | Same |

- The own byte stream, not Media Foundation's HTTP source: mello-core then owns the cache, the retries and the logs, and both platforms read the same reader.
- Time to first frame is a gate (section 10.3): under 1 s from the click on a 20 Mbps link to the local MinIO with 50 ms added delay.

Playback opens in the expanded view of the feed area, as `ExpandedStreamCard` does. Escape closes it. Inline playback in the card is not in scope: the DComp visual is an overlay (12-STREAMING.md 14), so cards that scroll over it need the geometry sync for each card.

### 6.3 Upload

| Item | Rule |
|---|---|
| Size of a 30 s clip | 720p60 at 5 Mbps (default): about 20 MB. 1080p60 at 8 Mbps: about 31 MB. 720p30 at 3 Mbps: about 12 MB. Audio adds 0.6 MB. |
| Body | Stream the file from disk. Do not read the whole file into RAM. |
| Pace | The upload runs in the background while the game runs, at low resource use (Q8): a rate cap of 3 Mbps, one request at a time, a low-priority thread, a small read buffer. With no game, no cap. |
| Order | The poster first (small), then the MP4. The card shows the poster as soon as the poster is up. |
| Retry | A failed upload retries with backoff. It resumes after a client restart from the local file. |
| Local file | Stays on disk as the clipper's copy (CLIPS.md 6.1). |

### 6.4 Backend and storage

| Change | Detail |
|---|---|
| `post_clip` | Accept `clip_type` in `voice`, `game`. Reject others. A game clip carries `width`, `height`, `has_voice`. The waveform stays optional. The second iteration adds a `trigger` and a `tag` (section 12). |
| `clip_upload_url` | For `game`: presign `crews/{crew_id}/{clip_id}.mp4` as `video/mp4` and `crews/{crew_id}/{clip_id}.jpg` as `image/jpeg`. Bind `Content-Length` with a cap of 64 MB for the MP4 and 1 MB for the JPEG, and sign the `Cache-Control` header of section 6.2. |
| `clip_upload_complete` | Sets `media_url` and `poster_url` on `StoredClip`. |
| Storage | The existing `mello-clips` bucket. Local dev: MinIO. |
| Retention | Game clip media is kept forever, as voice clip media is (Q10). |
| Limits | The existing per-user daily clip limit applies to game clips. |

---

## 7. Settings

A "Game clips" section in Settings:

| Control | Values | Default |
|---|---|---|
| Game clips (F8) | On, off | On (Q1) |
| Clip quality | 1080p60, 720p60, 720p30 | 720p60 (Q2) |
| Buffer folder | A folder | The OS temp folder (CLIPS.md 4.4) |

A control that cannot work shows why, in the label register: "No hardware encoder".

---

## 8. Cost while the user plays

The figures below are estimates. The perf gate (section 10.4) measures them on the Windows PC before the feature ships. The budgets are in 00-ARCHITECTURE.md section 2 and CLAUDE.md: idle RAM under 50 MB, in voice under 80 MB, watching a stream under 100 MB, install under 100 MB.

| Resource | Estimate (720p60, the default) | Gate |
|---|---|---|
| GPU 3D engine | The capture copy and the BGRA to NV12 convert: 0.1 to 0.3 ms per frame (12-STREAMING.md 3.2). About 1 % of a 16.7 ms frame. | Game fps loss at most 3 %, p99 frame time up at most 1 ms |
| GPU encoder engine | One 720p60 encode. NVENC and AMF are separate engines from the 3D engine. | No encode queue drops at tier 0 |
| VRAM | 3 NV12 slots, about 4 MB, plus the encoder session (driver dependent) | Measure, report |
| CPU | Capture and encode threads wait on events. Disk writes 0.8 MB/s. Loopback PCM, no Opus encode. | Under 2 % of one core |
| Process RAM | Ring index, a 2 MB write queue, encoder bitstream buffers | At most 10 MB more than without the buffer |
| Disk space | About 34 MB (video 25 MB, game audio 7.7 MB, voice 0.8 MB) | Fixed after start |
| Disk writes | About 0.8 MB/s, 2.9 GB per hour of play | Report. SSD wear is a known cost of every replay buffer. |
| Install size | No new binary. OS frameworks only. | No change |

The voice ring is on disk now, so the voice clip buffer frees 5.76 MB of RAM in voice. The replay adds at most 10 MB. Process RAM in voice must stay under 80 MB with the buffer on.

With a stream and the replay at the same time, the GPU does one capture, two converts and two encodes. NVIDIA GeForce drivers allow several encode sessions at once. The perf gate measures this case too.

---

## 9. Platforms

### 9.1 Windows (first)

| Part | Rule |
|---|---|
| Capture | WGC window. The hook only when it is already the active capturer (Q4). No monitor or desktop capture. |
| Encoder | NVENC (tested in the lab). AMF compiled and untested (Bob's AMD check). QSV is off unless `MELLO_ENABLE_QSV=1` (12-STREAMING.md 3.4), so an Intel-only PC has no game clips until QSV is tested. |
| Game audio | WASAPI process loopback, include the game's process tree. Windows 10 2004 or later. Endpoint loopback is **not** a fallback for the replay: it records the crew voice and every other app twice. **Not yet proven for this use:** in the CS2 recording, an OBS application audio capture of `mello.exe` (the same mechanism) attached and gave digital silence for the whole session, while the endpoint mix had the voice. The cause is not known. Step 2 starts with a measurement on a game process (section 11). |
| Border | Windows 11: hidden with consent. Windows 10: always shown. Game clips run there with the border (Q3). |

### 9.2 macOS (after Windows)

The same design with the existing macOS parts:

| Part | macOS |
|---|---|
| Capture | ScreenCaptureKit, one window (`SCContentFilter` with the game window) |
| Game audio | The same `SCStream`, filtered to the game app, `excludesCurrentProcessAudio = true` |
| Encode | The existing VideoToolbox encoder |
| Mux | `AVAssetWriter` passthrough |
| Playback | VideoToolbox decode, the RGBA path to Slint |

Risks to check on a Mac before the work starts:

- The menu bar shows the screen recording indicator for the whole game session.
- Recent macOS versions ask the user again, at intervals, to allow an app that records the screen for a long time.
- The macOS host has no geometry path. The replay does not need one (fixed size).

macOS gets game clips (Q11), after the Windows release. Until then the Settings section is hidden on macOS and F8 does nothing there.

---

## 10. Tests

### 10.1 Unit tests (`check.sh`, fast)

| Area | Test |
|---|---|
| `hotkeys.rs` | The voice and game clip keys at once: separate held state, modifiers, a held key, PTT on the game clip key. KEYBINDS refuses a key that another binding uses. |
| `poll_loop.rs` | `game_clip_key_pressed`: runs the callback only in the `recording` state. Tested in the testkit harness with the real `wire_all`: inject the replay state event, press F8, assert the `CaptureGameClip` command. |
| Replay manager (mello-core) | The lifecycle of section 4.2 as a pure state machine: setting, game start and exit, foreground change, disk low, no encoder, ladder failed. |
| Upload | Streamed body, pacing on and off, resume after restart. Against a local HTTP stub. |
| Card | `build_feed_card` for a game clip: poster, length, game, unread. Render check of the card with `slint-viewer --screenshot` on its `Preview`. |
| Backend (Go) | `clip_type` validation, game clip fields, presign content type, length cap and cache header. |

### 10.2 libmello ctest (`check-full.sh`)

| Test | Proves |
|---|---|
| `ReplayRing` | Segment rotation, a pinned segment is never overwritten, the queue drop counter, the crash cleanup. |
| `ReplayMux` | Synthetic access units and PCM in, MP4 out, demux back: first sample is a keyframe, durations match the capture times, moov before mdat. |
| `ReplayAvClock` | Three sources on synthetic clocks, one 300 ppm fast: after the mux, a marker in each source lands within 1 ms of its video frame. |
| `ReplayGaps` | A gap in video or audio keeps the samples after it at their own time. |
| `ClipByteStream` | The `IMFByteStream` over a range reader: a read of a missing range waits and then returns the bytes, a timeout returns an error, a seek reads from the new offset. |
| `CaptureHub` | Two consumers get every frame. Removing one does not stop the other. |

### 10.3 Stream gate: a replay lane

`tools/stream-gate` already has what the replay test needs: the reference clip with burned-in frame codes, the reference game audio with tone bursts tied to frames, the CPU lane (OpenH264 encode, `MELLO_SOFT_ENCODER=1`) and the hw lane. The replay lane needs no SFU and no shim: it is a file.

| Step | Detail |
|---|---|
| Source | `MELLO_CAPTURE_REFERENCE` through the capture hub to the replay consumer. Voice: marker tones through the test audio backend (`mello_voice_inject_capture` and the test output pull). |
| Capture | `mello_replay_capture` at fixed clip times: 35 s, 47 s (across the scene cut), and twice inside 30 s (pinning). |
| Read back | Demux and decode the MP4. Read the frame code of every frame. |
| Gates | First frame index is T0 within one GOP. No missing frame index. VMAF, PSNR and SSIM against the clip frames, with floors per lane in `benchmarks/baselines/replay/`. Game audio burst to video frame offset within +-20 ms for every marker. Voice marker to video offset within +-20 ms. Length 30 to 31 s. |
| Lanes | `cpu` (any machine), `hw` (the machine's encoder: NVENC on Bob's PC, AMF on an AMD box). |
| Streamed playback | Serve the captured MP4 from local MinIO through a delay and rate shim (20 Mbps, 50 ms). Play it through `ClipRangeReader` and the viewer pipeline. Gates: first frame under 1 s, no stall at 20 Mbps, every frame code in order, a seek to 20 s shows the frame of 20 s within 1 s. |
| Stream and replay together | The `clean` profile of the stream gate with a replay consumer on the same hub. The stream gates must still pass. This lane also proves the capture hub refactor. |

The gate runs from the QA harness, not from `check.sh`.

### 10.4 Perf gate

A scenario in the perf harness (20-PERF-HARNESS.md), on the Windows PC:

- A GPU-bound game load (Unigine Heaven, as in 12-STREAMING.md 3.1), at 1920x1080 and 3440x1440, borderless.
- Runs of 60 s: buffer off, buffer on, buffer on plus a stream.
- Reported: game fps (median, p99 frame time), client process RAM and CPU, VRAM, encode queue drops, disk write rate.
- Gates: the numbers of section 8.

### 10.5 QA harness and one journey

| Check | Content |
|---|---|
| `replay.gate-cpu` | The replay lane, CPU. |
| `replay.gate-hw` | The replay lane, hardware. Windows PC. |
| `perf.replay` | The perf scenario. Windows PC. |
| Journey `game-clip` (mello-driver) | Local stack only, voice through the local SFU. An `e2e-game` build injects a fake game (`mello-core/src/e2e_game.rs`), and the replay capture uses the reference source in that build. Join voice with a second client, wait for `recording`, press F8, assert the card in the feed with a poster, assert the MP4 in local MinIO, open it on a second client and play it. |

No test, script or journey points at production.

---

## 11. Build order

| Step | Todo id | Depends on | Lands as |
|---|---|---|---|
| 1 | `capture-hub` | none | Own PR. Stream gate unchanged. |
| 2a | `loopback-check` | none | A measurement on the Windows PC, before the ring: process loopback on a running game (CS2 and one other) gives a signal for the whole session, and the level before or after the game's own volume. A report, no code. If it fails, the game audio source is decided again before 2b. |
| 2b | `replay-ring`, `replay-session`, `mp4-mux` | 1, 2a | libmello, with the ctest tests and the replay gate lane |
| 3 | `backend` | none | Can run beside step 2 |
| 4 | `core-lifecycle`, `upload` | 2b, 3 | mello-core |
| 5 | `client-key-settings`, `feed-card-playback` | 4 | Client. Renders approved by Bob before push. |
| 6 | `gates` | 2 to 5 | Perf scenario, harness checks, journey |
| 7 | `specs` | 6 | Spec updates. This plan is then deleted. |
| 8 | `macos` | 7 | Later phase |

One feature branch (`feat/game-clips`), with the hub refactor as its own PR before it.

---

## 12. Out of scope

- Stream clips (a viewer clips someone else's stream, CLIPS.md 3.2).
- Trim UI and share export (CLIPS.md 14).
- Game clip playback on iOS and in the web lounge. The MP4 (moov first, H.264, AAC) plays in `AVPlayer` and `<video>` as is. The cards there are follow-ups.
- HLS and a second rendition (section 6.2).
- **Auto highlights: the second iteration.** No clip starts without the key in this iteration (Bob, Q14). An auto highlight records and posts a moment that the user did not choose, so it needs a way to ask the user first: opt in or not, per user or per crew, and how the crew learns that auto highlights are on. That design comes first in the second iteration, before any marker source ships. Then:

  - `Command::CaptureGameClip` gets a trigger: `key`, or `marker` with a kind (`multi-kill`, `clutch`, `utility-kill`). The capture call stays the same (it takes a window).
  - `post_clip` accepts an optional `trigger` and `tag`. The card can show the tag.
  - The poster of a marker clip is the frame at the marker time, decoded from the ring (at most one GOP).
  - The ring goes to 45 segments, for a marker at round end that needs 35 s back.
  - The first marker source is the CS2 adapter (`mello-core/src/telemetry/cs2_gsi.rs`). It reads fields that the installed GSI cfg already requests:

  | Field | Marker use |
  |---|---|
  | `player.state.round_kills`, `round_killhs` | A delta between two payloads is a kill, a headshot |
  | `player.state.health` | A drop to 0 is a death |
  | The weapon in `player.weapons` with `state == "active"` | The weapon of a kill (grenade, knife, zeus) |
  | `round.bomb` | `planted`, `defused`, `exploded` |
  | `player.match_stats.mvps` | A delta at round end is the MVP |

  Rules that found the real moments in the recording: three or more kills in a round, two kills within 3 s, the MVP, a defuse while alive, a kill with a grenade, knife or zeus in hand. CS2 quirks for that work:

  - `map.round` goes up when a round ends, so the `over` phase already has the next number. A round starts when the phase leaves `over`.
  - The last round of a match has no `over` phase. The map goes to `gameover` with `round.phase = freezetime`. The score delta gives the winner.
  - When the player is dead, the `player` block follows the spectated teammate. Use it only when `player.steamid` equals `provider.steamid`.

---

## 13. Decisions (Bob, 2026-10-31)

| # | Question | Decision |
|---|---|---|
| Q1 | Is "Game clips" on by default? | On by default. |
| Q2 | Default clip quality? | 720p60 (about 20 MB for 30 s). |
| Q3 | Do game clips run on Windows 10, where the WGC border always shows? | Yes. |
| Q4 | May the replay buffer use the game capture hook? | Only when the hook is already the active capturer. The replay never injects it. |
| Q5 | Does the buffer run out of voice? | No. The crew voice is part of every game clip. |
| Q6 | Which crew gets the clip? | The active crew. A user is always in a crew. |
| Q7 | Can a game clip be the hero of the feed? | Yes, after a stream preview from the last 24 hours. |
| Q8 | Upload while the game runs? | Yes, in the background, at low resource use. |
| Q9 | Mix levels of game and voice? | Unity with a limiter. |
| Q10 | Retention of game clip media? | Kept forever. |
| Q11 | Does macOS get game clips? | Yes, after Windows. |
| Q12 | F7 with the replay on? | F7 stays voice only. |
| Q13 | Download a clip before playback? (2026-11-01) | No. Playback streams from R2 from the first bytes (section 6.2). |
| Q14 | Auto highlights in the first iteration? (2026-11-01) | No. Second iteration, after a design to ask users first (section 12). |

Still to measure on the Windows PC (step 2a): whether process loopback on a game process gives a signal at all (section 9.1), and whether it captures before or after the game's own volume (section 5.3).

---

## 14. Evidence from a CS2 recording (2026-10-30)

One evening of CS2 premier (two matches, 40 rounds) was recorded on the Windows PC with a Game State Integration log beside a screen recording. A rule detector ran over the log. What it changed in this plan:

| Finding | Effect on the plan |
|---|---|
| The player pressed no mark key in 40 rounds. The one moment named afterwards (a kill with an incendiary in the last round) got no press. Only a weapon rule found it. | F8 is the manual path of this iteration. Markers are the second iteration, after a consent design (section 12). The capture call takes a window, so markers need no new capture path. |
| Detector windows: a multi-kill 12 to 36 s, a clutch 28 s, a utility kill 12 s. A marker at round end (MVP, clutch) needs up to 35 s back. | The second iteration raises the ring to 45 segments (sections 5.2, 12). |
| An OBS process loopback capture of `mello.exe` gave digital silence for the whole session. | Step 2a measures process loopback on a game before the ring is built (sections 9.1, 11). |
| CS2 refuses the OBS game hook without the `-allow_third_party_software` launch option. WGC capture of the game worked with no launch option. | The replay never injects the hook (Q4). WGC window capture is the base path. |
| Markers arrive with a receive time on the steady clock that indexes the ring. | A marker maps to a ring position with no alignment step (second iteration). |
