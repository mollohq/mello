---
name: Game clips (F8)
overview: "F8 saves the last 30 seconds of the user's own game: video, game audio and the crew voice, in one MP4. A local replay buffer runs while the user is in voice and a game is detected, also when the user does not stream. On by default. Windows first (WGC window capture, NVENC/AMF), macOS after Windows."
todos:
  - id: capture-manager
    content: "libmello: one game capture, many consumers. A refactor commit with no behavior change, proven by the stream gate. d724c21a (from main), check-full green. Stream gate A/B against main 690c2857, CPU and hw lanes: no change (2026-11-02). Merged into feat/game-clips (cb277f7f): one branch and one PR for the whole feature (Bob, 2026-11-02)"
    status: completed
  - id: loopback-check
    content: "Windows PC: measure WASAPI process loopback on a game process (signal present, before or after the game volume) before the ring is built on it. CS2, Hearthstone and the tone test done. CS2 ran in exclusive Fullscreen in all CS2 runs (Bob, 2026-11-02). The QPC position against the arrival time (A2) moves to the real loopback row of the replay lane (section 10.3)"
    status: completed
  - id: f7-key-cause
    content: "Windows PC: find why the physical F7 key does not reach the rdev hook on Bob's PC while a synthetic F7 does (Logitech G HUB runs there). F8 uses the same path. Before client-key-settings"
    status: pending
  - id: replay-ring
    content: "libmello: one preallocated ring file of encoded video with a byte index per GOP, game audio PCM, the voice ring with the crew mix and the mic as two Opus streams, each with its own time anchors (Q24), and m3llo's session volume (Q20), pinning during a capture with a spare rule. Built 2026-11-02 (abaee4f6, 63e036ca)"
    status: completed
  - id: replay-session
    content: "libmello: mello_replay_* C API, an encoder intent (stream or recording) through EncoderFactory into NVENC and AMF with a test per encoder, replay ladder (the hook only when it is already the active capturer, WGC window, no monitor capture), the stream priority rules of section 3.6, one shared game audio loopback per game (F7). Built 2026-11-02: the encoder intent (b817ecbf), the session with its own CaptureManager on the game window and the C API (747435b7), the shared capture with a stream of the same game (section 3.5 item 5, as built: the replay moves onto the stream's manager and back; F1, F2, Q18, Q19). F3 (the replay sheds first: 30 fps, then a pause, 60 s back up) and F7 (one process loopback per game in the context, shared by the stream host and the replay) are built too. The stream path changed a little (a flag per frame, the follow at start and stop, the overload shedder, the shared game audio): the stream gate A/B, with audio.json, runs on it before the release (todo gates)"
    status: completed
  - id: mp4-mux
    content: "libmello: MP4 mux on Windows (IMFSinkWriter, H.264 passthrough, AAC, moov first), the poster JPEG with an IDR-decode fallback, mux time under 3 s for 30 s. Built 2026-11-02 (c45b73d0): a 30 s clip at 5 Mbps muxes in under 1 s"
    status: completed
  - id: core-lifecycle
    content: "mello-core: replay manager (setting, foreground game with a 10 s hold, exclusive fullscreen, disk, encoder), CaptureGameClip, post_clip with clip_type game"
    status: pending
  - id: upload
    content: "mello-core: the poster at once, the MP4 as S3 multipart in 5 MB parts from disk (no full-file read into RAM), a token-bucket pace (G5), the stream budget and pause from the rung controller (G2 to G4, needs the host's GoogCC estimate and send rate in the stream telemetry), a persisted queue with the upload id and done parts for the resume after a restart"
    status: pending
  - id: backend
    content: "Nakama: validate clip_type, game clip fields, a presigned poster PUT with a signed length and Cache-Control, multipart for the MP4 (CreateMultipartUpload, a presigned UploadPart per 5 MB part with a signed length, CompleteMultipartUpload), an abort after 24 h, clip_upload_complete checks the objects with HEAD, Go tests"
    status: pending
  - id: client-key-settings
    content: "Client: F8 listener and gate, Settings section, HUD toast, testkit tests"
    status: pending
  - id: feed-card
    content: "Client: game clip card with poster in the feed and in chat; the clipper plays the local file"
    status: pending
  - id: clip-streaming-playback
    content: "mello-core and libmello: ClipRangeReader, IMFByteStream over it, streaming AAC playback, the viewer pipeline with a clip source, the expanded view"
    status: pending
  - id: gates
    content: "Replay lane in tools/stream-gate (VMAF floor first, then the bitrate per quality; a hw-lane row with a real loopback tone, which also measures A2), perf harness scenario, QA harness checks, one mello-driver journey with a crew voice tone burst (Q24)"
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

> **Status:** One branch and one PR for the whole feature (Bob, 2026-11-02). Step 1 (`capture-manager`, commit `d724c21a`) is merged into `feat/game-clips`; the stream gate A/B shows no change (CPU and hw lanes, 2026-11-02). Step 2a is done. Step 2b is built (2026-11-02): the replay in libmello, its C API, the shared capture and game audio with a stream, and the stream priority rules. Nothing is pushed. Nothing in the client starts the replay yet (step 4 and 5). Bob answered the open questions on 2026-10-31 (section 13). The review of 2026-11-01 ([GAME-CLIPS-REVIEW.md](GAME-CLIPS-REVIEW.md)) is folded in: section 3.6 (with the upload rules G1 to G8), Q17 to Q25, and the todos.
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

**Result (2026-11-01, Windows PC):** all checks pass. ClipBuffer 8 of 8. A clip from the ring has the talker's voice, 30.0 s, and plays. The ring file is 768000 bytes in voice, and it is gone after a leave and after a forced end of the process. The fix stays on this branch (Q15).

Found on the way: the physical F7 key did not reach the key hook on Bob's PC, with or without Fn, while a synthetic F7 did and made a clip. The cause is not known yet. The voice clip upload held the command loop for 625 ms (`UploadClip`): the `upload` todo removes that path.

### 0.3 Step 2a: measure process loopback (a report, no code)

The CS2 recording got digital silence from a process loopback capture (section 14). Measure it before the ring is built on it:

1. Use the existing loopback code (`capture_wasapi_loopback.cpp`, `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`) through `tools/stream-host --capture-backend process` on the game window, or a small test that opens it on a pid and writes a WAV.
2. Games: CS2 and one other (a game with a launcher process, so the process tree matters). 10 minutes each, in a match, with m3llo in voice.
3. Record per game: is there a signal for the whole run (RMS per second, the share of silent seconds), does the level follow the in-game volume, the Windows volume mixer slider for the game, or neither.
4. Write the result into this plan, section 9.1. If the signal is not reliable, stop and ask Bob before step 2b. Endpoint loopback is not a fallback (section 9.1).

### 0.4 Step 1: write the capture manager design, then build it

Section 3.2 gives the goal, not the design. Read `capture_process.cpp`, `video_pipeline.cpp`, `capture_screencapturekit.mm` and the stream host start in `mello.cpp` first. Then add a section 3.5 to this plan that answers:

- Which thread hands a frame to each consumer, and what a slow consumer does to the others (it must not stall the capture or the other consumer).
- How a consumer is added and removed while frames are in flight (the stream starts and stops while the replay runs).
- How the per-capture events reach every consumer: a method change (a keyframe for each encoder), `state()` for the pause card, `target_exited`, the exclusive-fullscreen restart, a source size change.
- Who owns the `GraphicsDevice` and the manager, and what happens when the device is poisoned (12-STREAMING.md 3.1).
- Whether the macOS ScreenCaptureKit path moves into the manager now or in the macOS phase.
- What does not change: the stream C API (`mello_stream_*`) and the stream behavior.

Get Bob's OK on section 3.5, then build it as its own commit on the feature branch.

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

- `CaptureManager` (name in Q16, design in section 3.5) owns one `ProcessCapture` for one game pid, on the shared `GraphicsDevice`.
- Each consumer registers a frame callback. The manager hands each consumer the same BGRA texture and capture time. Each consumer has its own NV12 ring (3 slots), so the two encoders never alias.
- The stream host becomes a consumer. Its behavior does not change.
- Step 1 is the manager refactor alone, in its own commit. The stream gate (CPU and hw lanes) must show no change. Bob decided on 2026-11-02 that the whole feature is one branch and one PR, so the refactor is a separate commit, not a separate PR.

### 3.3 Replay capture ladder

The replay buffer records in the background for hours. It must not show the desktop, and it must not show a border around the game.

| Ladder step | Replay | Reason |
|---|---|---|
| Hook | Only when the hook is already the active capturer: a stream that uses the hook shares its capture through the manager. The replay never injects the hook itself. | Bob, Q4. |
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
| Cost tiers | Yes | The replay sheds first: 30 fps, then a pause, before the stream's own tiers move (section 3.6, F3). Without a stream: the same overload signal, and below tier 2 the replay drops to 30 fps. |
| Bitrate controller | Yes | No |
| Bitrate | Preset | Per clip quality, from the replay lane: set the VMAF floor first, then take the lowest bitrate that passes it on the reference clip (section 10.3). The 5 Mbps of sections 5.2 and 6.3 is an estimate until then. |
| B-frames | None (`frameIntervalP = 1`) | None. The segments (an IDR with SPS and PPS at each start) and the `IMFSinkWriter` passthrough (no decode timestamps) need it. The high quality tuning must not turn them on. AMF: `B_PIC_PATTERN = 0`. |
| SPS and PPS | `repeatSPSPPS = 1` | The same. AMF: header insertion at each IDR. |
| How it is set | Fixed in each encoder | A new intent field in `EncoderConfig` (stream or recording), through `EncoderFactory` into NVENC and AMF, with a unit test per encoder for the tuning, the VBV and the GOP. Today `EncoderConfig` has no such field, NVENC tries P4 ULL, P1 ULL and P1 LL (`encoder_nvenc.cpp`), and AMF sets the ultra-low-latency usage (`encoder_amf.cpp`). |

The 1 s GOP sets the start error: a clip starts at the last keyframe at or before T minus 30 s, so a clip is 30 to 31 s long.

### 3.5 The capture manager (step 1 design, for Bob's review)

**Today** (read on this branch):

- `VideoPipeline` owns the device (`init_device`), the capture source, the preprocessor, the encoder and the encode thread.
- The backend's capture thread calls `VideoPipeline::on_captured_frame`. That function decimates, consumes the swap event, draws the DXGI pointer, applies a geometry request, converts BGRA to NV12 on the GPU into the preprocessor's ring, and enqueues the job (2 deep, newest wins). The encode thread encodes and calls the packet callback.
- The BGRA texture is valid only during the callback. The convert is the last use of it.
- `ProcessCapture` lets only the active backend's frames through (`deliver_mutex_`, `active_token_`).
- The capture events are pulled by the pipeline: `consume_swap_event` (one flag, consumed once), `state`, `failed`, `target_exited`, `method_history`, `get_cursor`, `cursor_mode`.
- Every backend gets its target rate at `start` (WGC, DXGI and hook throttles with `frame_cadence_accepts`; the hook also writes `target_fps` into the shared block).
- A capture stop that times out leaks the whole pipeline (`abandoned`).

**Objects and owners:**

| Object | Owns | Held by |
|---|---|---|
| `CaptureManager` (`src/video/capture_manager.{hpp,cpp}`) | The `GraphicsDevice`, one `CaptureSource` (a ladder, a window, a monitor, the reference clip or SCK), the present-delay histogram, the consumer list | Each consumer, by `shared_ptr` |
| Consumer (the stream's `VideoPipeline` host side; the replay session in step 2b) | Its preprocessor and NV12 ring, its encoder, its encode thread, its own decimation | Its owner (`MelloStreamHost`, later the replay session) |
| Registry (step 2b) | A `weak_ptr` per capture key: process pid plus start time, or the reference clip | `mello::Context` |

In step 1 each stream host makes a private manager with one consumer. The registry and a second consumer come in step 2b. The step 1 PR then cannot share anything by mistake.

**1. The thread that hands a frame to each consumer**

- The backend's capture thread, as today. The manager calls the consumers in order: the stream first, then the replay.
- Each call gets `{texture, capture_us, ladder step, swap generation}`. Step 1 has no ladder step field: no step 1 consumer reads it, and it needs a `ProcessCapture` change. It comes with the ladder policy per consumer (item 5) in step 2b.
- The consumer callback may decimate, convert on the GPU into its own ring, and enqueue without a wait. It must not wait on the disk, the network or its encoder. `on_captured_frame` keeps this rule now.
- A slow encoder drops from its own queue (newest wins, counted in its own `encode_queue_drops`). The capture and the other consumer do not wait for it.
- A convert that hangs stops the capture thread for all consumers. This is not a new risk: the consumers share one device, and a hung device stops both (item 6).
- Not chosen: one BGRA copy and one delivery thread per consumer. That is one more full-size copy per frame per consumer, and the convert is already a short GPU submit.

**2. Add and remove while frames are in flight**

- Add: append under the manager's list mutex. The next frame reaches the new consumer. Its first encoded frame is an IDR, as at a stream start now.
- Deliver: under the mutex, copy the list of consumer pointers, and for each consumer that is not removed, increment its in-delivery counter. Release the mutex, call each one, and decrement its counter. The check and the increment are under one mutex, so a remove cannot pass between them.
- Remove: set the removed flag (no new calls), then wait until the counter is 0, at most 5 s (the `kEncodeJoinDeadline` rule). After that the manager never calls the consumer again and the owner can free it. When the wait times out, the capture thread is stuck inside that consumer: the owner leaks the consumer, as `abandoned` does now.
- A consumer must not remove itself from inside its callback (the rule of the voice packet sink).
- The last remove stops the capture source and takes the manager out of the registry.

**3. The capture events**

| Event | Today | With the manager |
|---|---|---|
| Method change (force a keyframe) | `consume_swap_event`, consumed by the pipeline | The manager consumes the source's flag once per frame, before the fan-out, and increments a swap generation. Each consumer forces its own keyframe when the generation of a frame is new to it. |
| `state` (pause card), `failed`, `method_history` | Pulled from the source | Pulled from the manager, which asks the source. All consumers see one value. |
| `target_exited` | Pulled at 1 Hz, ends the stream | The same for the stream. The replay manager stops the replay. |
| Exclusive-fullscreen restart | A ladder move inside `ProcessCapture` | No change for the stream: it is a method change (row 1). The replay alone has no step to move to, and it never injects the hook. It stays on WGC window: CS2 in exclusive Fullscreen was captured by WGC window (section 9.1, Q21). A game that WGC cannot see delivers no frames, and the replay is then not available for it. |
| Source size change | `ConvertResult::source_resized` per frame | No change: each consumer has its own preprocessor and counts its own changes. |
| Pointer | `get_cursor`, `cursor_mode` from the active backend | Forwarded by the manager. |
| Present-to-capture histogram | The pipeline owns it and gives it to the source | The manager owns it. The stream's telemetry reads it. |
| Source audio (SCK audio, reference timed audio) | One callback on the source | One callback per consumer. The manager fans out on the source's audio thread. |

**4. Frame rate**

- Step 1: one consumer, so the capture rate is the stream rate. No change.
- Step 2b: while a stream consumer is attached, the capture rate is the stream's rate, and the replay records at that rate (section 3.6, F1). With the replay alone, the capture rate is the replay's rate. When a stream joins or leaves, the manager gives the active backend the new rate through a new `CaptureSource::set_target_fps`: an atomic in the WGC, DXGI and hook throttles, and the hook's shared block. The replay never raises the rate while a stream runs, so the hook's copy cost inside the game stays the stream's.

**5. Ladder policy per consumer (step 2b, named here so that step 1 leaves room for it)**

- A consumer says which ladder steps it takes. The stream takes all of them. The replay takes `WgcWindow`, and `Hook` only while a stream consumer is attached (Q4).
- With a stream consumer, the manager runs the stream's ladder (`allow_hook` from the `start_stream` policy). With the replay alone, the ladder is `[WgcWindow]`.
- The replay drops a frame of a step that it does not take. That is a gap in its ring.
- A stream joins a replay on `WgcWindow`: the manager runs the stream's ladder exactly as at a stream start today, with `keep_old_until_first_frame` and all its timings. The replay follows the step that the stream's ladder chose, or pauses on a step that it does not take (section 3.6, F2, Q18).
- The stream leaves while the step is `Hook`, `WgcMonitor` or `Dxgi`: the manager goes back to `[WgcWindow]` with `keep_old_until_first_frame`. The replay does not keep the hook. The hook stops 5 s after the heartbeat stops (Q19).

**As built (2026-11-02).** A manager keeps one source, so the replay moves between managers in place of one manager that changes its ladder:

| Event | What happens |
|---|---|
| A stream of the replay's game starts | The stream starts exactly as without the replay. Then `follow_stream` stops the replay's own WGC capture and makes the replay a consumer of the stream's manager, with its preprocessor and encoder rebuilt on that manager's device, at min(replay rate, stream rate) (F1). |
| A frame of a monitor method (WGC monitor, DXGI) | `CapturedFrame::target_only` is false (`CaptureSource::frames_show_only_target`, for ProcessCapture the method that delivers now). The replay drops it and reports Waiting. |
| The stream stops | Before the stream removes its consumer, `leave_stream` puts the replay back on its own WGC window capture. The stream's capture, the hook too, stops with the stream (Q19). |
| The replay starts while the same game is streamed | The context keeps a weak reference to the running stream's capture and its game, so the new replay joins it at once. |
| A switch | The encode size stays the same (the preprocessor fits each source into it), so a clip has one picture size. Each switch starts a new segment with an IDR. The gap is about 100 ms (measured with the reference source). |
| A failure in a switch | Only the replay turns off (F5). |

**6. The device and a poisoned device**

- The manager makes the device (`create_d3d11_device`) and owns it. The capture, and every preprocessor and encoder of its consumers, use it (zero copy needs one device, 14-VIDEO-PIPELINE.md 2.2). A consumer holds the manager, so the device lives longer than its encoder.
- This is not one more device than today: the device moves from the host pipeline to the manager. The replay of the same game uses the same device.
- A poisoned device (`device_poisoned_`): no more ladder moves. `failed` reports it after the pass, the stream sends its `StreamError` as today, and the replay manager turns the replay off.
- A replay fault never touches shared state: an encoder, convert or ring error turns the replay off. The replay path never moves the ladder and never sets `device_poisoned_`. A device that the GPU removed (`DXGI_ERROR_DEVICE_REMOVED`) is GPU-wide and the one exception (section 3.6, F5).
- A manager whose stop times out leaks with its device. It leaves the registry first and is never used again. The next consumer makes a new manager on a new device. A leaked manager counts in `mello_stream_abandoned_pipelines`.

**7. macOS (Q17)**

- Recommended (changed after the review of 2026-11-01): macOS keeps the direct capture until the macOS work. `VideoPipeline` keeps an `#ifdef` at each capture call until then. The step 1 PR does not change the macOS host path, which the Windows PC cannot run.
- The reason: the macOS pipeline encodes inside the capture callback (VideoToolbox on the SCK queue). That breaks the callback rule of item 1, so a second consumer cannot run there anyway until the macOS work adds an encode thread.
- The other choice: the manager code is platform neutral, and step 1 puts `SCKCapture` behind it too, with one consumer. One capture path, but a change to the macOS host path that only the macOS CI lane and a stream on Bob's Mac can check.

**8. What does not change**

- `mello.h` and the `mello_stream_*` C API.
- The stream behavior: the ladder and its timings, keyframes, the pointer, the telemetry fields, the abandon counting.
- The viewer side, and `Context::video()` for the encoder and decoder lists.

**9. The step 1 commit**

| Item | Content |
|---|---|
| New | `capture_manager.{hpp,cpp}` |
| Changed | `video_pipeline.{hpp,cpp}`: `capture_` and `init_device` on the host side become the manager. `mello.cpp`: audio callbacks and `mello_stream_reference_info` through the manager. |
| Tests (`CaptureManager`, fake source, no GPU) | Two consumers get every frame in order. A removed consumer gets no call after `remove` returns, and the other consumer keeps its frames. `remove` waits for a call in flight and returns "abandoned" after 5 s for a stuck one. A swap reaches each consumer once. The last remove stops the source. Audio reaches each consumer. |
| Gate | Section 0.4: stream gate CPU and hw lanes, 3 runs each on `main` and on the branch, side by side in the PR. |

### 3.6 The stream has priority

Bob's rule (2026-11-01): the game clips feature must not affect a stream that the user runs. Each place where the replay meets a running stream has a rule. Source: section F of `GAME-CLIPS-REVIEW.md`.

| # | Where | Rule |
|---|---|---|
| F1 | The capture rate (3.5 item 4) | While a stream runs, the capture rate is the stream's rate. The replay records at that rate and never raises it. |
| F2 | A stream that starts while the replay runs (3.5 item 5, Q18) | The manager runs the stream's ladder exactly as today. The replay follows the step that the stream chose, or pauses on a step that it does not take. |
| F3 | Overload (3.4 cost tiers) | The replay sheds first. The first overload signal (the stream's encode queue drops, its encoder wait time up, the first tier step) drops the replay to 30 fps. The next one pauses the replay. The stream's own tiers move only after the replay is paused. The replay resumes after 60 s with no overload. The ring keeps its content over a pause, and a clip shows the last frame for the gap. |
| F4 | Upload (6.3) | Bob rejected "no upload while a stream runs": a clip that waits for the end of a 4 h stream is bad. The upload runs during the stream as a flow that the stream's own controller governs, and the stream never pays for it (rules G1 to G8 below). |
| F5 | A replay fault (3.5 item 6) | An encoder, convert or ring error turns the replay off and logs it. The replay path never moves the ladder and never sets `device_poisoned_`. Only a device removed by the GPU is shared, because it ends the stream anyway. |
| F6 | The poster readback (5.3) | The copy to the staging texture is on the capture thread: a submit, no wait. The map and the JPEG run on the mux worker thread. |
| F7 | Game audio (9.1) | One game audio capture per game, shared like the video: one WASAPI process loopback on the game pid, fanned out to the stream's audio pipeline and the replay ring. This lands in step 2b, and the stream gate's `audio.json` gates stay unchanged. |

**As built (2026-11-02).**

- F3: `ReplayLoad` (`replay_load.hpp`) has three levels: full rate, 30 fps, paused. The stream's overload check (`VideoPipeline::maybe_reduce_encoder_cost`, once per 60 frames) first calls the overload shedder that `mello_stream_start_host` sets. The replay that follows the stream gives up one level and the stream keeps its tier. Only when the replay is paused does the stream's tier move. Without a replay the stream's rule is unchanged. Alone, the replay checks its own encoder with the stream's rule: its cost tiers first, then the same levels. After 60 s with no overload, the replay goes one level back up. `MELLO_REPLAY_PAUSED` reports a pause.
- F7: `SharedGameAudio` (`shared_game_audio.cpp`) is one process loopback of a game's process tree, with a fan-out. The context keeps one per pid (`Context::game_audio`). The stream host's audio pipeline and the replay take their game audio from it. On a Windows without process loopback, the stream host keeps its own capture with the endpoint fallback, as before; the replay then has no game audio.
| F8 | The gates (10.3, 10.4) | The "stream and replay together" lane adds an F8 press during the run, a forced overload that drops the replay to 30 fps, and a pause and a resume of the replay. The stream gates must pass inside the spread of the `main` runs in every case. The perf case "buffer on plus a stream" reports the stream's encode queue drops and frame timing. |

A Settings control "Game clips while streaming" (on, off) gives a streamer who sees an effect a way out (Q23).

**The upload during a stream (F4, section G of the review).** The host runs a GoogCC bandwidth estimate on TWCC feedback (`GoogCcController`, `libmello/src/transport/twcc.hpp`). `QualityController` (`mello-core/src/stream/controller.rs`) moves a rung down after 2 s under the floor or 2 s of encoder overload, and up after 15 s (`UP_DWELL`).

| # | Rule |
|---|---|
| G1 | The poster uploads at once, in every state. It is under 1 MB. The card shows the poster and "uploading" within seconds, on every client in the crew. |
| G2 | The budget while a stream runs: headroom = the GoogCC estimate minus the stream's send rate. Upload rate = min(1 Mbps, 25 % of headroom). Zero while the stream is under its top rung. These are starting values; the gate (G7) confirms or moves them (Q25). |
| G3 | Pause and resume follow the rung controller. A rung drop, a loss or overload signal, or an estimate under the stream's rate pauses the upload at once. It resumes after 15 s at the top rung with headroom, the controller's own rung-up hold. The upload sheds before the stream moves. |
| G4 | No GoogCC estimate (the sender paces at its ceiling): a fixed 500 kbps, with the same pause on a rung drop. |
| G5 | Smooth, not bursty: a token bucket, writes of 16 KB, one connection, a socket send buffer of 64 KB. A burst into the home router's queue costs the stream RTP jitter and RTT, which TWCC reads as congestion. The burst is the risk, not the average rate. |
| G6 | S3 multipart upload in 5 MB parts, each a presigned `UploadPart` with a signed `Content-Length`. A pause costs at most the part in flight. A client restart lists the parts and goes on (section 6.3, 6.4). |
| G7 | Gate: the "stream and replay together" lane runs one upload at the budget through the shim. The stream gates, the TWCC RTT and the RTP jitter stay inside the spread of the `main` runs. A second case with no headroom (`rate_fraction` 1.0) proves that the upload pauses and the stream loses no rung to it. |
| G8 | In a game with no stream: 3 Mbps as in 6.3, smoothed as in G5. No game: no cap. |

A 20 MB clip takes about 3 min at 1 Mbps and about 5.5 min at 500 kbps during a stream. The card shows "uploading" until then. The upload needs the host's bandwidth estimate and send rate in the stream telemetry, if the stream manager does not read them already.

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
| WGC window delivers frames of the game | The replay ladder (section 3.3). The exclusive-fullscreen check (`SHQueryUserNotificationState` and the foreground pid) does not turn the replay off: CS2 in exclusive Fullscreen was captured by WGC window (section 9.1, Q21). A game that WGC cannot see delivers no first frame, and F8 then says "Clips are not available for this game. Set the game to borderless." |
| The buffer folder has at least 2 GB free | CLIPS.md 4.4 uses 100 MB for audio. Video needs more headroom. |

Voice is a condition (Q5). A game clip always has the crew voice in it.

### 4.2 Lifecycle

| Event | Action |
|---|---|
| A game starts and the rules hold, or the user joins voice while a game runs | Start the replay consumer and its ring files. |
| The user leaves voice | Stop. Delete the ring. |
| The foreground game changes, and the new game stays in the foreground for 10 s | Stop the old consumer, delete its ring, start one for the new game. One replay at a time. The 10 s hold keeps an alt-tab between two games from a restart each time. |
| The game is minimized | The capture holds (no frames). The ring keeps its content. A clip shows the last frame for the gap. |
| The game exits | Stop. Delete the ring. |
| The user turns the setting off | Stop. Delete the ring. |
| Disk free goes under 2 GB | Stop. Delete the ring. HUD toast once. |
| The client starts | Nothing to delete: the ring files are delete on close, so a crash leaves none (section 5.2). |

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
| Game audio | The arrival time at `GetBuffer` minus the duration of the packet, as the stream path does (`StreamAudioHostPipeline::on_pcm`). The resample ratio of 5.3 removes the drift. | The QPC position that `GetBuffer` can give is not read today (`capture_wasapi_loopback.cpp` passes `nullptr`), and the process loopback device is virtual. It is not used before a measurement against the arrival time (section 9.1). |
| Voice: the remote voice | The render time of the 10 ms mix plus the output latency | The time the user heard it. The same sum that `AudioPipeline` gives the echo canceller (03-LIBMELLO.md 4, stream-delay hints). |
| Voice: the local mic | Its capture time | Today the ring mixes the mic into the render frame when 10 ms of mic is ready (`AudioPipeline`, the clip buffer tap). The mic then sits at the render time, late by the capture-to-render queue depth, which varies. Decided (Q24): the ring keeps the mic apart from the crew mix, with its own time anchors. The mux and the F7 capture mix them. |

The time anchors of the voice ring are taken on the thread that hands the samples to the ring (the render thread for the crew mix, the capture thread for the mic) and travel with the samples through the 2 s RAM queue. An anchor that the writer thread takes is up to 2 s wrong.

### 5.2 Rings

| Ring | Where | Content | Size at the default quality |
|---|---|---|---|
| Video | Disk, in the buffer folder | Annex-B access units in one preallocated ring file. One segment is one GOP (1 s) and starts with an IDR plus SPS and PPS. An index per segment: byte offset, size, capture time of each access unit, keyframe flag. | 40 slots of 2 s of the peak rate: 62.5 MB at 5 Mbps (720p60). About 25 MB of it holds data on average (below). |
| Game audio | Disk, same folder | 48 kHz stereo int16 PCM in a ring file, with a time anchor every 10 ms chunk | 40 s, 7.7 MB |
| Voice | Disk, the existing `ClipBuffer` ring file | Two streams of 48 kHz mono Opus frames, 60 s each: the crew mix and the local mic (Q24). A time anchor per frame, and m3llo's session volume with the anchors (Q20). The mux decodes the window. | 1.6 MB (one stream of 0.8 MB exists on this branch) |

- One ring file, preallocated, with a byte offset per segment in the index. No file is created or deleted while the game runs. One file is simpler than a file per segment for the crash cleanup and for pinning. With VBR at 1.25x a segment's size varies, so each slot holds the largest GOP that the encoder can make: with a VBV of 1 s of the peak rate, a 1 s GOP carries at most 2 s of the peak rate. A GOP that is still larger ends its segment early, and the ring asks the encoder for an IDR (built 2026-11-02, `replay_video_ring.cpp`).
- The ring files are `replay_video_<pid>_<n>.bin` and `replay_audio_<pid>_<n>.bin` in the buffer folder, opened delete on close as the voice ring is. Windows deletes them when the ring stops and when the process dies, so there is no ring folder and no crash cleanup (section 4.2).
- 40 segments: the 30 s window, one GOP for the keyframe before it, and spare segments for pinning. The segment count is one constant. The second iteration raises it to 45 (a marker at round end needs 35 s back, section 14).
- A capture pins the segments it reads. The writer skips a pinned segment and takes the next free one.
- Spare rule: two captures inside 30 s pin up to 32 of the 40 segments, and the writer then has 8 s of free segments. A third capture while two muxes run waits for the first to unpin. The mux time is a gate: under 3 s for a 30 s clip on the Windows PC (section 10.2).
- The writer thread is not the encode thread. The encode thread hands each access unit to a bounded queue (2 MB). A full queue drops the new unit and every unit after it up to the next keyframe, counts them, and asks the encoder for an IDR: a P-frame without its reference cannot be decoded. Disk stalls never block the encoder.
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
- Mix: game and voice at unity gain, then a limiter (Q9). Process loopback captures after the game's own volume and after the game's slider in the Windows volume mixer (measured, section 9.1). The voice ring is before m3llo's own slider. Decided (Q20): the mux applies m3llo's session volume, recorded with the voice anchors, to the crew mix. Both sources are then after their sliders and before the endpoint volume.
- Poster: the first captured frame after the press (the texture is valid only in the callback, section 3.5), copied to a staging texture once, written as JPEG (WIC on Windows). It costs one copy, only at a press. The copy is a submit on the capture thread; the map and the JPEG run on the mux worker (section 3.6, F6). A minimized or alt-tabbed game delivers no frame: then the poster is the last IDR of the ring, decoded once. The second iteration needs that decode for marker posters, so it is built once.

### 5.4 The MP4 file

| Item | Windows | macOS |
|---|---|---|
| Muxer | `IMFSinkWriter`, H.264 input type equal to output type (passthrough), AAC encoder MFT | `AVAssetWriter`, video input with `outputSettings: nil` (passthrough) |
| moov first (web and phone playback start before the download ends) | `mp4_faststart` after `Finalize`: moov moves before mdat and the chunk offsets move with it. `MF_MPEG4SINK_MOOV_BEFORE_MDAT` wrote chunk offsets that point before the media data (measured 2026-11-02), so it is not used. | `shouldOptimizeForNetworkUse = true` |
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
- The byte stream reports `MFBYTESTREAM_IS_SEEKABLE` and carries `MF_BYTESTREAM_CONTENT_TYPE = video/mp4`, or the source resolver sniffs and fails. The source reader seeks on open. A read that waits for a range must not wait on the thread that fetches it.
- This player is its own todo (`clip-streaming-playback`), apart from the card (`feed-card`). The card can land and be approved while the player is built; until then the clipper plays the local file.
- Time to first frame is a gate (section 10.3): under 1 s from the click on a 20 Mbps link to the local MinIO with 50 ms added delay.

Playback opens in the expanded view of the feed area, as `ExpandedStreamCard` does. Escape closes it. Inline playback in the card is not in scope: the DComp visual is an overlay (12-STREAMING.md 14), so cards that scroll over it need the geometry sync for each card.

### 6.3 Upload

| Item | Rule |
|---|---|
| Size of a 30 s clip | Estimates until the replay lane sets the bitrates (section 3.4): 720p60 at 5 Mbps (default): about 20 MB. 1080p60 at 8 Mbps: about 31 MB. 720p30 at 3 Mbps: about 12 MB. Audio adds 0.6 MB. |
| Body | Stream the file from disk. Do not read the whole file into RAM. |
| Pace | The upload runs in the background while the game runs, at low resource use (Q8): a rate cap of 3 Mbps, one request at a time, a low-priority thread, smoothed by a token bucket (section 3.6, G5). With no game, no cap. While a stream runs, the stream's controller sets the budget and pauses the upload (G2 to G4). |
| Order | The poster first (small, at once in every state, G1), then the MP4. The card shows the poster as soon as the poster is up. |
| Parts | S3 multipart upload in 5 MB parts, each part a presigned `UploadPart` with a signed `Content-Length` (G6). A pause or a failure costs at most the part in flight. |
| Retry | A failed part retries with backoff. A persisted upload queue (in the client's data folder) holds each clip not yet up with its upload id and its done parts. After a client restart the client lists the parts and goes on. |
| Local file | Stays on disk as the clipper's copy (CLIPS.md 6.1). |

### 6.4 Backend and storage

| Change | Detail |
|---|---|
| `post_clip` | Accept `clip_type` in `voice`, `game`. Reject others. A game clip carries `width`, `height`, `has_voice`. The waveform stays optional. The second iteration adds a `trigger` and a `tag` (section 12). |
| `clip_upload_url` | For `game`: the poster `crews/{crew_id}/{clip_id}.jpg` as one presigned PUT, `image/jpeg`, a signed `Content-Length` of at most 1 MB and the `Cache-Control` of section 6.2. The MP4 `crews/{crew_id}/{clip_id}.mp4` as a multipart upload (section 3.6, G6): `CreateMultipartUpload` with `video/mp4` and the `Cache-Control`, then a presigned `UploadPart` per 5 MB part with a signed `Content-Length`, then `CompleteMultipartUpload`. A total cap of 64 MB, which follows the bitrates of section 3.4. Today `GeneratePresignedPUT(key, contentType, expiry)` (`s3.go`) signs no length and no `Cache-Control`, so this needs new helpers with Go tests (aws-sdk-go-v2 presigns parts). Check that R2 enforces a signed `Content-Length`: MinIO and R2 can differ. |
| Multipart cleanup | An upload that is not complete after 24 h is aborted (a backend job or a bucket lifecycle rule). |
| `clip_upload_complete` | Completes the multipart upload, then checks each object with a HEAD request (it exists, its size up to the cap, its type), then sets `media_url` and `poster_url` on `StoredClip`. A card never shows a poster with no video. |
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
| Game clips while streaming | On, off | On, if Q23 adds it (section 3.6) |

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
| Process RAM | Ring index, a 2 MB write queue, encoder bitstream buffers; during a capture also the sink writer of the mux | Two numbers, measured before a promise: the steady state, and the peak during a capture. The steady state at most 10 MB more than without the buffer. |
| Disk space | About 72 MB of files (video 62.5 MB sized for the peak, game audio 7.8 MB, voice 1.6 MB) | Fixed after start |
| Disk writes | About 0.8 MB/s, 2.9 GB per hour of play | Report. SSD wear is a known cost of every replay buffer. |
| Install size | No new binary. OS frameworks only. | No change |

The voice ring is on disk now, so the voice clip buffer frees 5.76 MB of RAM in voice. The replay adds at most 10 MB. Process RAM in voice must stay under 80 MB with the buffer on.

With a stream and the replay at the same time, the GPU does one capture, two converts and two encodes. NVIDIA GeForce drivers allow several encode sessions at once. The device has `SetMultithreadProtected(TRUE)` (`graphics_device.cpp`), so the capture thread and two encode threads on one device are safe but serialized. The perf gate measures this case too, with the convert time of each consumer and the stream's encode queue drops (section 3.6).

---

## 9. Platforms

### 9.1 Windows (first)

| Part | Rule |
|---|---|
| Capture | WGC window. The hook only when it is already the active capturer (Q4). No monitor or desktop capture. |
| Encoder | NVENC (tested in the lab). AMF compiled and untested (Bob's AMD check). QSV is off unless `MELLO_ENABLE_QSV=1` (12-STREAMING.md 3.4), so an Intel-only PC has no game clips until QSV is tested. |
| Game audio | WASAPI process loopback, include the game's process tree. Windows 10 2004 or later. Endpoint loopback is **not** a fallback for the replay: it records the crew voice and every other app twice. **Not yet proven for this use:** in the CS2 recording, an OBS application audio capture of `mello.exe` (the same mechanism) attached and gave digital silence for the whole session, while the endpoint mix had the voice. The cause is not known. Step 2 starts with a measurement on a game process (section 11). |
| Border | Windows 11: hidden with consent. Windows 10: always shown. Game clips run there with the border (Q3). |

**Step 2a measurement (2026-11-01, Windows PC).** Tool: `libmello/tests/loopback_probe.cpp` (`mello_loopback_probe`). It opens the stream host's `WasapiLoopbackCapture` with the scope "include the process tree" on a pid. It refuses endpoint loopback. Each second it logs the level, the share of digital-zero samples, the frames that arrived, and the game's session in the Windows volume mixer (slider, mute, peak meter). m3llo was in voice, with a second client that talked the whole time.

| Run | Result |
|---|---|
| Control: `mello.exe` in voice, 20 s | Signal: median −23 dBFS, 48000 frames a second, no digital-zero second. The silence of the OBS application capture in the CS2 recording (section 14) came from OBS, not from process loopback. |
| CS2, 10 min in a match (`cs2.exe`, run 2) | Signal for the whole run: 600 of 600 s with frames (45120 to 51360 a second), 0 digital-zero seconds, median −35 dBFS. The 21 s under −60 dBFS all fall in the in-game volume step, where the level is low (−60 to −66 dBFS, not zero). |
| CS2: the Windows volume mixer slider at 50 % for 55 s | No answer: −31 dBFS at 50 %, against −34 dBFS before and −30 dBFS after. The game's own level changes by more than the 6 dB step. The tone row below answers the question. |
| CS2: the game's master volume at 50 % | The level follows it: about −50 dBFS for the 75 s of the step, against −27 dBFS before. Process loopback captures **after** the game's own volume. |
| CS2 in the background (run 1) | CS2 plays nothing when it is not the active window, unless its setting "Play Audio When Game In Background" is on. The loopback is then digital zero, and the session meter shows the same. A clip of an alt-tabbed CS2 has no game audio, which is what the user heard. |
| Hearthstone, 10 min (`Hearthstone.exe`, started from Battle.net) | Signal for the whole run: 600 of 600 s with frames (45600 to 50880 a second), 0 digital-zero seconds, median −35.5 dBFS, 5 quiet seconds (not zero). The game has no child process. Its parent had exited and no Battle.net process ran, so there was no launcher tree to probe. Slider at 50 % for 82 s: −42.8 dBFS, against −36.9 dBFS before and −41.1 dBFS after, also no clear answer. |
| A constant 1 kHz tone (ffplay through its Chocolatey shim), the slider set from the probe | The level follows the slider exactly: −36.1 dBFS at 100 %, −42.1 dBFS at 50 %, −36.1 dBFS at 100 % again. Process loopback captures **after** the Windows volume mixer slider. The session peak meter stays at −33.1 dBFS: it is before the slider, so it cannot show the slider. The probe targeted the shim's pid, and the tone came from its child `ffplay.exe`: the "include the process tree" scope works. |
| CS2 in "Fullscreen" (exclusive) under WGC window capture | Bob, 2026-11-02: CS2 was in exclusive Fullscreen in the recording of 2026-10-30 and in the runs above. OBS recorded it with WGC capture in that mode (section 14). The ladder's check (`SHQueryUserNotificationState`) reported CS2 as exclusive on 2026-09-18 (`capture_wgc.cpp`), so that check alone does not prove that WGC sees nothing. The replay gates on WGC frames, not on the check (section 4.1). |
| The QPC position of the process loopback device against the arrival time | Moved to the real loopback row of the replay lane (section 10.3), which gates the game audio times at ±20 ms against the frame codes. Until then the game audio time is the arrival time minus the packet duration. |

Effect on the mix (section 5.3): the game audio is the level after the game's own volume and after the game's slider in the Windows volume mixer. The endpoint (master) volume is not measured. The voice ring taps m3llo's mix in the render path, before m3llo's own slider in the mixer. The mux applies that slider to the crew mix (Q20), so the clip balance is what the user heard.

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
| `ReplayAvClock` | Three sources on synthetic clocks, one 300 ppm fast: after the mux, a marker in each source lands within 1 ms of its video frame. This proves the mux, not the times that the sources give (section 10.3 has the real loopback row). |
| `ReplayMuxTime` | A 30 s mux of the default quality finishes in under 3 s on the Windows PC (section 5.2, spare rule). |
| `ReplayGaps` | A gap in video or audio keeps the samples after it at their own time. |
| `ClipByteStream` | The `IMFByteStream` over a range reader: a read of a missing range waits and then returns the bytes, a timeout returns an error, a seek reads from the new offset. |
| `CaptureManager` | Two consumers get every frame. Removing one does not stop the other. |
| Encoder intent | NVENC and AMF: the recording intent sets the tuning, the VBV and the GOP of section 3.4, and keeps B-frames off and SPS and PPS at each IDR. |

### 10.3 Stream gate: a replay lane

`tools/stream-gate` already has what the replay test needs: the reference clip with burned-in frame codes, the reference game audio with tone bursts tied to frames, the CPU lane (OpenH264 encode, `MELLO_SOFT_ENCODER=1`) and the hw lane. The replay lane needs no SFU and no shim: it is a file.

| Step | Detail |
|---|---|
| Source | `MELLO_CAPTURE_REFERENCE` through the capture manager to the replay consumer. Voice: marker tones through the test audio backend (`mello_voice_inject_capture` and the test output pull). |
| Capture | `mello_replay_capture` at fixed clip times: 35 s, 47 s (across the scene cut), and twice inside 30 s (pinning). |
| Read back | Demux and decode the MP4. Read the frame code of every frame. |
| Gates | First frame index is T0 within one GOP. No missing frame index. VMAF, PSNR and SSIM against the clip frames, with floors per lane in `benchmarks/baselines/replay/`. The VMAF floor is set first; the bitrate per clip quality is then the lowest that passes it (section 3.4). Game audio burst to video frame offset within +-20 ms for every marker. Voice marker to video offset within +-20 ms. Length 30 to 31 s. |
| What the voice markers prove | `mello_voice_inject_capture` is the mic path. The remote voice path (SFU, playout, mix) is not in this lane, so the voice gate proves the mic path only. The QA journey checks the remote path with a tone burst (section 10.5, Q24). |
| Real loopback source (hw lane, Windows PC) | The reference source gives the game audio with exact capture times, so it does not test the loopback times. One row plays a 1 kHz tone with on and off steps from a small WASAPI player in the gate tool (no ffmpeg or ffplay: GPL, and m3llo is Apache 2.0) through a real process loopback, against the frame codes: offset within +-20 ms. The CPU lane cannot run this row. |
| Lanes | `cpu` (any machine), `hw` (the machine's encoder: NVENC on Bob's PC, AMF on an AMD box). |
| Streamed playback | Serve the captured MP4 from local MinIO through a delay and rate shim (20 Mbps, 50 ms). Play it through `ClipRangeReader` and the viewer pipeline. Gates: first frame under 1 s, no stall at 20 Mbps, every frame code in order, a seek to 20 s shows the frame of 20 s within 1 s. |
| Stream and replay together | The `clean` profile of the stream gate with a replay consumer on the same manager. The run adds an F8 press, a forced overload that drops the replay to 30 fps, and a pause and a resume of the replay (section 3.6, F8). The stream gates must pass inside the spread of the `main` runs in every case. This lane also proves the capture manager refactor. |
| Upload during a stream (section 3.6, G7) | The same lane runs one clip upload at the budget through the shim, against local MinIO. The stream gates, the TWCC RTT and the RTP jitter stay inside the spread of the `main` runs. A second case with no headroom (`rate_fraction` 1.0) proves that the upload pauses and the stream loses no rung to it. |

The gate runs from the QA harness, not from `check.sh`.

### 10.4 Perf gate

A scenario in the perf harness (20-PERF-HARNESS.md), on the Windows PC:

- A GPU-bound game load (Unigine Heaven, as in 12-STREAMING.md 3.1), at 1920x1080 and 3440x1440, borderless.
- Runs of 60 s: buffer off, buffer on, buffer on plus a stream.
- Reported: game fps (median, p99 frame time), client process RAM (steady and during a capture) and CPU, VRAM, encode queue drops, disk write rate. With a stream: the stream's encode queue drops and frame timing, and the convert time of each consumer.
- Gates: the numbers of section 8.

### 10.5 QA harness and one journey

| Check | Content |
|---|---|
| `replay.gate-cpu` | The replay lane, CPU. |
| `replay.gate-hw` | The replay lane, hardware. Windows PC. |
| `perf.replay` | The perf scenario. Windows PC. |
| Journey `game-clip` (mello-driver) | Local stack only, voice through the local SFU. An `e2e-game` build injects a fake game (`mello-core/src/e2e_game.rs`), and the replay capture uses the reference source in that build. Join voice with a second client, wait for `recording`. The second client plays a tone burst into voice and logs the time it was played on the first client. Press F8, assert the card in the feed with a poster, assert the MP4 in local MinIO, and assert the burst in the clip within ±20 ms of the time the first client played it (Q24). Open it on a second client and play it. |

No test, script or journey points at production.

---

## 11. Build order

| Step | Todo id | Depends on | Lands as |
|---|---|---|---|
| 1 | `capture-manager` | none | Own commit (`d724c21a`), merged into the feature branch. Stream gate unchanged. Done. |
| 2a | `loopback-check` | none | A measurement on the Windows PC, before the ring: process loopback on a running game (CS2 and one other) gives a signal for the whole session, and the level before or after the game's own volume. A report, no code. If it fails, the game audio source is decided again before 2b. |
| 2b | `replay-ring`, `replay-session`, `mp4-mux` | 1, 2a | libmello, with the ctest tests and the replay gate lane. The voice times follow Q24, the voice level follows Q20 (sections 5.1, 5.3). |
| 3 | `backend` | none | Can run beside step 2 |
| 4 | `core-lifecycle`, `upload` | 2b, 3 | mello-core |
| 4a | `f7-key-cause` | none | A finding, then a fix if the cause is in m3llo. Before step 5. |
| 5 | `client-key-settings`, `feed-card` | 4, 4a | Client. Renders approved by Bob before push. |
| 5a | `clip-streaming-playback` | 2b, 5 | mello-core and libmello. The card plays the local file until it lands. |
| 6 | `gates` | 2 to 5 | Perf scenario, harness checks, journey |
| 7 | `specs` | 6 | Spec updates. This plan is then deleted. |
| 8 | `macos` | 7 | Later phase |

One feature branch and one PR (`feat/game-clips`). The manager refactor is its own commit in it (Bob, 2026-11-02).

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
| Q15 | Does the voice ring fix (`570e8ec`) get its own PR? (2026-11-01) | No. It stays on `feat/game-clips`. |
| Q16 | The name of the shared capture object? (2026-11-01) | `CaptureManager`, in `capture_manager.{hpp,cpp}`. |
| Q17 | Does macOS go behind the manager in step 1? | No (Bob, 2026-11-01). macOS keeps the direct capture and its `#ifdef` until the macOS work (section 3.5, item 7). |
| Q18 | A stream starts while the replay runs on WGC window, with the hook allowed: does the stream show WGC frames until the hook delivers? | No (Bob, 2026-11-01). The stream runs its ladder exactly as today, and the replay follows or pauses (section 3.6 F2). |
| Q19 | The stream ends while the hook captures: the replay goes back to WGC window, and the hook stops. Accept? | Yes (Bob, 2026-11-01). The hook permission comes with a stream start. This does not touch a running stream. |
| Q20 | The game's mixer slider is in the loopback, m3llo's own slider is not in the voice ring. Does the mux apply m3llo's slider to the voice, so the clip balance is what the user heard? | Yes (Bob, 2026-11-02). Record m3llo's session volume with the voice ring's time anchors and apply it in the mux. Both sources are then at the same point, after their sliders and before the endpoint volume. |
| Q21 | A game in exclusive fullscreen: the replay never injects the hook (Q4). What does the replay do? | Measured (Bob, 2026-11-02): CS2 in exclusive Fullscreen was captured by WGC (section 9.1). The replay stays on WGC window and gates on its frames, not on the exclusive-fullscreen check (section 4.1). A game that WGC cannot see gets no clips, and the toast says "Set the game to borderless". |
| Q22 | WGC asks Windows for borderless access at the first WGC start in the process (`ensure_borderless_access`, `capture_wgc.cpp`). Can a consent dialog appear when the replay starts? | No (Bob, 2026-11-02): m3llo is signed in production, and no borderless dialog appears. The replay asks at its first WGC start, as the stream does. |
| Q23 | A Settings control "Game clips while streaming" (on, off)? | Open. Proposed by the review: yes, default on, with the rules of section 3.6. A streamer who sees any effect has a way out. |
| Q24 | Voice times in the ring (section 5.1): the local mic sits in the mix at the render time, late by a varying queue depth, and the replay lane's voice markers prove only the mic path. | Decided (Bob, 2026-11-02). The mic: the ring keeps it apart from the crew mix, each with its own anchors. The remote path: a check in the QA journey, where the second client plays a tone burst at a known time (section 10.5). |
| Q25 | The starting values of the upload budget during a stream (section 3.6, G2): at most 1 Mbps, and 25 % of the headroom. | Open. The gate (G7) confirms or moves them. A 20 MB clip then takes about 3 min. |
| — | Upload during a stream (2026-11-01) | Decided: the upload runs during the stream, governed by the stream's controller (section 3.6, G1 to G8). "No upload while a stream runs" is rejected. |

Step 2a (section 9.1) is done: CS2 and Hearthstone are measured. Process loopback gives a signal for the whole run on both, after the game's own volume and after the game's mixer slider. CS2 ran in exclusive Fullscreen (Q21). Not measured: a game with a launcher tree that stays open (Battle.net had exited). The QPC position against the arrival time moves to the replay lane (section 10.3).

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
