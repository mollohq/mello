# Review of GAME-CLIPS.md (2026-11-01)

Reviewer: the Fable session, at Bob's request. Scope: the plan on `feat/game-clips`
(`plans/GAME-CLIPS.md`, 715 lines, commit `b5c7ec0d`), checked against the code
on that branch. The plan is good. The design of sections 3 to 6 is sound and the
build order is right. The items below are what the code contradicts, what is
missing, and what should be decided before step 2b. They are ranked.

## A. Fix before step 2b

### A1. Exclusive fullscreen games get no replay

`capture_process.cpp:245`: WGC and DXGI cannot see an exclusive fullscreen game.
Only the hook can. The replay ladder is `[WgcWindow]` and the replay never
injects the hook (Q4). So a game in exclusive fullscreen has no replay buffer,
unless a stream with the hook runs. Section 3.5 item 3 says "exclusive-fullscreen
restart: no change". That is true for the stream. For the replay alone there is
no step to move to.

Many FPS players run exclusive fullscreen. CS2 offers "Fullscreen" and
"Fullscreen Windowed". The 2026-10-30 evidence ("WGC capture of the game worked")
does not say which mode CS2 was in.

Do:
- Add Q21: accept no clips in exclusive fullscreen (the toast and the Settings
  label say so, with the fix "set the game to borderless"), or allow the hook for
  the replay after a consent step, or something else. Bob decides.
- Add to the step 2a report: CS2 in "Fullscreen" (exclusive) under WGC window
  capture. Frames or not. Ten minutes is not needed. One match start is enough.
- Section 4.1: add the condition "the game is not in exclusive fullscreen"
  (`query_exclusive_fullscreen_output` exists) so the replay does not start a
  capture that fails and then shows "not available for this game" with no reason.

### A2. Game audio time: the plan names a value the code does not read

Section 5.1: "The loopback device position of the buffer (`GetBuffer` QPC
position)". `capture_wasapi_loopback.cpp:368` passes `nullptr` for both
positions. The stream path stamps loopback audio with its arrival time
(`stream_audio_pipeline.cpp:193`). The QPC position of the process loopback
virtual device is not a value to build on without a measurement: it is a
virtual endpoint, and the Windows sample for process loopback does not use it.

Do:
- Define the game audio time as the arrival time at `GetBuffer` minus the
  packet duration, and keep the resample ratio of 5.3 to remove drift. Or
  measure the QPC position first and write the result into 9.1.
- `ReplayAvClock` (10.2) runs on synthetic clocks. It proves the mux, not the
  source. Add one hw lane row with a real loopback source: the probe's 1 kHz
  tone (`ffplay`) with on and off steps, against frame codes, offset within
  +-20 ms. The CPU lane cannot do this. Say so in 10.3.

### A3. Voice time: which path the gate measures, and where the anchor is taken

- The voice ring mixes the local mic into the render frame when 10 ms of mic
  is available (`audio_pipeline.cpp:1068-1077`). The mic lag is the capture-to-
  render queue depth, variable, up to the 1 s ring. The remote voice gets
  "render time plus output latency". These are two anchors, and the plan gives
  one.
- The replay lane injects markers with `mello_voice_inject_capture`. That is
  the mic path. The remote path (SFU, playout, mix) is not in the lane ("needs
  no SFU"). The +-20 ms voice gate then proves the mic path only. Say so, and
  decide whether the remote path gets a check in the QA harness journey (a
  second client plays a tone burst at a known time).
- "Add a time anchor per frame" to the voice ring: the ring is written by a
  writer thread from a 2 s RAM queue (`clip_buffer.hpp:19-27`). The anchor must
  be taken in `write()` on the render thread and travel with the samples
  through the queue. An anchor taken by the writer is up to 2 s wrong.

### A4. The recording encoder profile has no plumbing

`EncoderConfig` (`encoder.hpp:14-21`) has width, height, fps, bitrate, keyframe
interval and codec. NVENC tries P4 ULL, P1 ULL, P1 LL (`encoder_nvenc.cpp:127-
135`). AMF sets `USAGE_ULTRA_LOW_LATENCY` and low-latency mode
(`encoder_amf.cpp:101-104`). Section 3.4 needs a new field (an intent: stream or
recording) through `EncoderFactory` into both encoders, and the `replay-session`
todo does not list it. Add it, with a unit test per encoder that the recording
intent sets the tuning, the VBV and the GOP of 3.4.

Keep as is, and write it into 3.4 as a requirement: `frameIntervalP = 1` (no
B-frames) and `repeatSPSPPS = 1` on NVENC, `B_PIC_PATTERN = 0` and header
insertion per IDR on AMF. The segment design (an IDR plus SPS and PPS at each
segment start) and the `IMFSinkWriter` passthrough (no decode timestamps) need
both. The high quality tuning must not switch B-frames on.

### A5. The bitrate is a guess until the gate exists

5 Mbps for 720p60 with a 1 s GOP is low for fast footage, and the 1 s GOP costs
more bits than the stream's 4 s. Do not fix the bitrate in the plan. Set the
VMAF floor of the replay lane first (`benchmarks/baselines/replay/`), then pick
the lowest bitrate per quality that passes it on the reference clip, then update
the size table of 6.3 and the 64 MB cap of 6.4. Q2's "about 20 MB" is then a
result, not an input.

## B. Decide before the build

### B1. The WGC borderless consent dialog

`ensure_borderless_access` (`capture_wgc.cpp:81-110`) runs once per process at
the first WGC start and shows the Windows consent dialog. Today that is a
stream start, an act of the user. With the replay on by default, the dialog
appears the first time a user joins voice with a game running, behind a
fullscreen game, with no act of the user. Add Q22: ask at the first stream
start or in Settings only, and run the replay with the border until granted, or
accept the dialog at the replay start.

### B2. Q17: keep SCK out of step 1

Recommend the other choice of 3.5 item 7. The Windows PC cannot run the macOS
path, the macOS pipeline encodes in the capture callback and breaks item 1
anyway, and a second consumer is refused there. The `#ifdef` at each capture
call stays until the macOS phase. Less risk in the refactor PR.

### B3. Q20: yes, and it is consistent

The loopback is after the game's mixer slider and before the endpoint master.
The voice ring is before m3llo's slider and before the master. Applying m3llo's
session volume to the voice in the mux puts both sources at the same point.
Record the slider with the voice anchors, as proposed.

## C. Design gaps in sections 5 and 6

### C1. Ring file layout

"Segment files are preallocated and reused in turn" with VBR 1.25x: a segment's
size varies and a preallocated file must hold the peak (an IDR plus 1 s at the
peak rate). Either state that size, or use one preallocated ring file with a
byte offset per segment in the index. One file is simpler for the crash cleanup
and for pinning. The "about 25 MB" of 5.2 is the average, not the allocation.

Pinning: two captures inside 30 s pin up to 32 segments of 40. The writer then
has 8 s of free segments. State the spare count as a rule, and make the mux
time a gate (under 3 s for a 30 s clip on the Windows PC).

### C2. Poster

The texture is valid only in the callback (3.5). So the poster is the next
frame after the press, not "the newest captured frame". An alt-tabbed or
minimized game delivers no frame: no poster. Fallback: decode the last IDR
from the ring. The second iteration needs that decode for marker posters, so
build it once.

### C3. Playback (6.2) is a feature of its own

A custom `IMFByteStream`, a streaming AAC path, a range reader with a disk
cache, the viewer pipeline with a clip source, the expanded view, and the macOS
resource loader, in one todo with the card. Split `feed-card-playback` into
`feed-card` (the card, and the clipper plays the local file) and
`clip-streaming-playback`. Then the card can land and be approved while the
player is built.

Gotchas for the Windows player: the byte stream must report
`MFBYTESTREAM_IS_SEEKABLE` and carry `MF_BYTESTREAM_CONTENT_TYPE = video/mp4`,
or the source resolver sniffs and fails. The source reader seeks on open; a read
that waits for a range must not wait on the thread that fetches it.

### C4. Backend presign

`GeneratePresignedPUT(key, contentType, expiry)` (`clips.go:438`) signs no
length and no `Cache-Control`. A new helper is needed, and the Go test must
cover it. Check that R2 enforces a signed `Content-Length`: MinIO and R2 differ.
`clip_upload_complete` should HEAD the object (exists, size, type) before it
sets `media_url` and `poster_url`, so a card never shows a poster with no video.

### C5. Upload resume

"Resumes after a client restart from the local file" needs a persisted upload
queue. No todo owns it. Add it to `upload` or drop the claim.

## D. Smaller items

- The physical F7 key does not reach the key path on Bob's PC (0.2). F8 uses
  the same path. Make the cause a step 0 item before `client-key-settings`, and
  press the physical key once in the manual check of the journey.
- 3.5 item 2: the in-delivery counter must be incremented under the same mutex
  as the removed-flag check, or a remove can pass between the two.
- `SetMultithreadProtected(TRUE)` is on (`graphics_device.cpp:126`). Two encode
  threads and the capture thread on one device are safe but serialized. The
  perf gate should report the convert time with two consumers.
- 4.2 "The foreground game changes": a user who alt-tabs between two games
  deletes and restarts the ring each time. Add a hold time (10 s) before a
  switch.
- 4.2 "The client starts: delete leftover ring folders": two client instances
  run on this PC (`MELLO_SESSION_KEY`). Name the folder by pid, as the voice
  ring does, and delete only folders of dead pids.
- Section 8 RAM gate "at most 10 MB more": the encoder session, the sink writer
  during a mux and the write queue. Make it two numbers: steady state and
  during a capture. Measure before you promise.
- 3.5 item 4: with the hook, `target_fps` in the shared block goes to the game
  process. Raising it from 30 to 60 for the replay changes what the hook sends
  while a stream runs. The stream decimates, so no change for viewers, but the
  hook's copy cost doubles. Note it in the perf gate case "stream and replay".

## E. What is right and should not change

- One capture, two consumers, with the stream as a consumer and the refactor as
  its own PR with the gate on both lanes.
- The replay never injects the hook, no monitor or desktop capture.
- A separate recording encoder, not the stream's output.
- H.264 passthrough into the MP4, PCM game audio in the ring, Opus voice from
  the existing ring.
- moov first, progressive MP4 over range requests, no HLS.
- The key is the only trigger in this iteration.

## F. The stream has priority (Bob, 2026-11-01)

Bob's rule: the game clips feature must not affect a stream the user runs.
Section 3.2 says "the stream host becomes a consumer, its behavior does not
change". The plan then changes the stream in four places. Each gets a rule.

| # | Where the plan affects the stream | Rule |
|---|---|---|
| F1 | 3.5 item 4: the capture rate becomes the highest consumer rate. A 60 fps replay beside a 30 fps stream doubles the WGC delivery and the hook's copy cost in the game process. | While a stream runs, the capture rate is the stream's rate. The replay records at that rate. The replay never calls `set_target_fps` while a stream consumer is attached. |
| F2 | 3.5 item 5 and Q18: a stream that joins a replay starts on WGC frames until the hook delivers. Today the stream waits for the hook. | Q18 = no. When a stream starts, the manager runs the stream's ladder exactly as today, `keep_old_until_first_frame` and all timings. The replay follows the step the stream chose, or pauses on a step it does not take. The stream gate proves it (section 10.3, "stream and replay together"). |
| F3 | 3.4 cost tiers: "the same overload signal, below tier 2 the replay drops to 30 fps". The stream's tiers move at the same time as the replay. | The replay sheds first. On the first overload signal (stream encode queue drops, encoder wait time up, the first tier step) the replay drops to 30 fps. On the next, the replay pauses. The stream's own tiers move only after the replay is paused. The replay resumes after a cooldown (60 s) with no overload. The ring keeps its content over a pause; a clip shows the last frame for the gap. |
| F4 | 6.3: the upload runs in the background at 3 Mbps while the game runs. A stream shares that uplink. 3 Mbps of PUT traffic is what the stream's bitrate controller sees as congestion, and it drops the stream to a lower rung. | No upload while a stream runs. The clip waits in the upload queue. The card shows "uploads after the stream" on the clipper's own client. The upload starts when the stream ends, with the pacing of 6.3 while the game runs. |
| F5 | 3.5 item 6: one device for both consumers. A fault in the replay's encoder path can poison the device, and the poisoned device ends the stream. | A replay fault never touches shared state. An encoder error, a convert error or a ring error turns the replay off and logs it. The replay path never moves the ladder and never sets `device_poisoned_`. The one exception is a device removed by the GPU (DXGI_ERROR_DEVICE_REMOVED), which is GPU-wide and would end the stream anyway. |
| F6 | 5.3 poster: "copied to a staging texture once" on a press. A copy plus a map is a GPU to CPU readback, and a map waits for the GPU. On the capture thread that wait stalls the stream's frame. | The copy to the staging texture is on the capture thread (a submit, no wait). The map and the JPEG are on the mux worker thread. The mux never runs on the capture or the encode thread (5.3 says so, keep it). |
| F7 | 9.1 game audio: the stream host opens its own process loopback on the game pid. The plan gives the replay its own loopback too (3.5 item 3 fans out SCK and reference audio only). Two loopback clients and two capture threads on one game. | One game audio capture per game, shared like the video: the manager (or a sibling `GameAudioCapture` in the registry) owns the WASAPI process loopback and fans the PCM out to the stream's audio pipeline and the replay ring. Step 1 is video only; this lands in step 2b with the stream gate on the `audio.json` gates unchanged. |
| F8 | 10.3, 10.4: the gates cover "stream and replay together" once. | Add to the stream gate lane "stream and replay": an F8 press during the run, the replay dropped to 30 fps by a forced overload, and the replay paused and resumed. The stream gates must pass inside the spread of the `main` runs in every case, as for the step 1 refactor. The perf gate case "buffer on plus a stream" reports the stream's encode queue drops and frame timing, not only the game's fps. |

Settings: a control "Game clips while streaming" (on, off) is cheap and gives a
streamer who sees any effect a way out. Default on, with the rules above. Bob
decides (Q23).

Q19 stays as proposed: the stream leaves, the replay goes back to WGC window.
That does not touch a running stream.
