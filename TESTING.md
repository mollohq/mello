# Testing & Harnesses

One place that lists every test tool across the three repos (`mello`, `mello-sfu`,
`mello-site`), how to run it, and when to reach for it. Skim the quick reference,
then jump to the section you need.

> Repos: this file lives in `mello/`. The SFU (`mello-sfu`) and marketing/test
> site (`mello-site`) are sibling repos checked out next to `mello/`.

---

## The UI harness

`client/src/testkit.rs` runs the real UI headlessly — no window, no network, no
audio device. It drives the production `callbacks::wire_all` and
`handlers::handle_event`, not copies, so a change to either shows up here.

```rust
let mut h = Harness::new();

h.app().invoke_mic_toggle();                        // UI -> core
assert!(h.commands().iter().any(|c| matches!(c, Command::SetMute { muted: true })));

h.emit(Event::VoiceStateChanged { in_call: true, transport: VoiceMode::SFU }); // core -> UI
assert!(h.app().get_in_voice());
```

Journey tests live in `client/src/flow_tests.rs` and run under plain
`cargo test --workspace`. They are deliberately *not* in `client/tests/`: that
would need `--features testkit`, and CI would silently skip them.

Three things that are easy to get wrong:

- **Find controls by label, other elements by type.** Every control declares
  `accessible-role` and `accessible-label`, so `find_by_accessible_label` works.
  Two tests in `client/src/a11y_lint.rs` keep it so: a static check over every
  `.slint` file, and a runtime check that no control on the main screens renders
  an empty label. Non-controls declare no role, and `accessible_enabled()`
  returns `None` for them; query those by component type name.
- **Hidden elements are absent from queries.** There is no visibility predicate.
  "The screen is blank" is expressed as "no screen root matched", which is what
  `assert_not_blank()` does.
- **`MainWindow`'s default state renders nothing.** `onboarding-step 0` with
  `logged-in false` matches no branch in `main.slint`. Set a state before
  asserting, or the test passes while testing nothing.

## Does the suite actually catch bugs?

```bash
./scripts/mutation-check.sh
```

It breaks the code on purpose, one change at a time, and checks the suite goes
red for each. A green suite proves nothing by itself — it can be green because
the code works, or because the tests assert nothing, and the difference is
invisible until an outage. A sweep once found the whole suite passing with the
chat message body deleted.

When adding a regression test, verify it fails without its fix. Every starred
regression test in `flow_tests.rs` was checked that way.

## Flaky tests

A flaky test is a broken test. Fix the determinism or delete it — never add
retries, and never `ctest --repeat until-pass`. A suite you re-run until it
goes green tells you nothing, and the habit spreads.

Every flaky test found in this codebase so far had the same cause: **the test
asserted on a race it did not control.** Three examples, all in libmello RTP:

| Test | The race | The fix |
|---|---|---|
| `QueueOverflowEntersIdrGate` | Needed the send queue to overflow; paced 20 Mbps against 16 tiny frames, so the pacer usually drained them first | Starve the pacer (8 kbps) and send 48 frames — overflow becomes arithmetic |
| `BoundedBurst…QueueOverflow` | Same, but through a peer connection with no settable pacing | Overflow by volume instead: 24 × 512 KB against a 4 MB bound |
| `ParityFecRepairs…WithoutPli` | Needed the queue *not* to overflow; a flat 3 ms sleep assumed the pacer kept up | Keep the sleep (the receiver needs it) and also wait for the queue to drain |

The pattern to reach for: **make the condition true by construction**, not by
hoping the machine cooperates. Starve a producer, exceed a bound by volume, or
wait on the actual state instead of sleeping.

Beware two traps seen while fixing these:

- A sleep may be load-bearing for something other than what it looks like.
  Removing the 3 ms above fixed the sender race and broke the receiver.
- When mutating code to check a test catches a regression, restore with `touch`
  afterwards. `cp`/`mv` restores an older mtime, so cargo reuses the artifact
  built from the *mutated* source and every later result is wrong.

To check the suite still catches regressions at all:

```bash
./scripts/mutation-check.sh
```

---

## The two commands

```bash
./scripts/check.sh        # ~90s — run before every push (also the pre-push hook)
./scripts/check-full.sh   # ~10min+ — before a release, or when touching libmello/ or backend/
```

`check.sh` is the fast gate: fmt, clippy, the whole Rust workspace (including
the headless UI flow tests and screen-state invariants), and the cross-language
RPC contract check. No Docker, no backend, no devices.

`check-full.sh` adds the two suites needing a native toolchain — libmello's C++
gtest suite and the Nakama Go modules.

---

## Quick reference

| Tool / suite | Layer | Needs a backend? | Runs in PR CI? | Command (short) |
|---|---|---|---|---|
| `cargo test --workspace` | Rust (core, client, tools) | No | ✅ yes | `cargo test --workspace` |
| `cargo fmt` / `cargo clippy` | Rust | No | ✅ yes | `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings` |
| libmello `ctest` | C++ audio/video DSP | No | ✅ yes | `ctest --test-dir libmello/build` (configure with `-DMELLO_BUILD_TESTS=ON` first) |
| Nakama `go test` | Backend modules | No | ✅ yes | `cd backend/nakama/data/modules && go test ./...` |
| Nakama `gofmt` / `go vet` | Backend modules | No | ✅ yes | `cd backend/nakama/data/modules && gofmt -l . && go vet ./...` |
| `dev_fault` RPC | Backend fault injection | Yes (live Nakama) | No (manual/integration) | RPC call, gated by `MELLO_ENABLE_DEV_FAULT=1` |
| SFU `go test` | SFU server | No | ⚠️ not wired (run locally) | `cd mello-sfu && go test ./...` |
| `voice-soak` | SFU load/churn/resilience | Yes (live SFU) | No (manual/integration) | `go run ./tools/voice-soak ...` |
| `stream-soak` | SFU H.264 RTP stream relay | Yes (live SFU) | No (manual/integration) | `go run ./tools/stream-soak ...` |
| Stream certification | SFU soak + LAN client gates | Yes (local Nakama + SFU) | No (manual) | `scripts/run-stream-certification.ps1` |
| `stream-host` probe | Client host RTP path + telemetry | Yes (live Nakama + SFU) | No (manual) | `scripts/run-stream-host.ps1` |
| `sfu-stream-viewer-probe` | Client viewer RTP path + telemetry | Yes (live Nakama + SFU) | No (manual) | `scripts/run-stream-viewer.ps1` |
| `voice-test-client` (GUI) | Client DSP A/B | Yes (live backend) | No (manual) | `cargo run` in `tools/voice-test-client` |
| `voice-test-client` (headless) | Client reconnect/resync E2E | Yes (live backend) | 🔧 integration job | `cargo run -- --scenario scenarios/<f>.json` |
| `scripts/voice/voice-local-gate.sh` | Cross-repo local RED/GREEN gate + artifacts | Yes (local Nakama + SFU) | 🔧 integration job | `../scripts/voice/voice-local-gate.sh` |
| `perf-harness` | Client CPU/RSS regression (headless) | Partial (`idle` no; voice yes) | No (local/agent) | `./scripts/perf/run.sh` |
| Voice quality gate | libmello voice path: MOS, delay, concealment under impairments | No | Smoke test only (`cargo test`); full gate in `check-full.sh` | `./scripts/voice-gate.sh` |
| `sfu-test.html` | Browser voice/stream + robustness | Yes (live SFU) | No (manual) | open via `npm run dev` in `mello-site` |

Legend: ✅ runs today · ⚠️ runnable, not yet in the PR workflow · 🔧 needs an
integration job with a seeded backend.

---

## What PR CI runs today

`.github/workflows/pr-checks.yml` runs on self-hosted **Windows** and **macOS**
runners for every PR to `main`:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings   # warnings are errors
cargo test --workspace
```

Notes:

- **A PR that changes only `specs/` or `plans/` skips the build.** The `changes`
  job decides this. The three test jobs show as skipped, and a skipped required
  check passes. Do not use `paths-ignore` for this: the checks never start, and
  the PR cannot merge.
- **`CI=true` is set automatically by GitHub Actions.** That's what makes
  `cargo test --workspace` skip the device-dependent audio/video tests. Locally
  those tests block waiting on real capture/playback devices, so run local tests
  with `CI=true` too (PowerShell: `$env:CI='true'; cargo test --workspace`).
- `--all-targets` + `--workspace` now also lint/build `tools/voice-test-client`
  (it's a workspace member). Keep it clippy-clean or the PR lane breaks.
- The Go (backend + SFU) and C++ (libmello) suites are **not** in the PR
  workflow yet — run them locally before "done" (see CLAUDE.md), or add jobs.

---

## Rust — core, client, tools

From the `mello/` repo root:

Prefer `./scripts/check.sh`, which runs all of this with the right flags. The
individual commands, if you need one in isolation:

```bash
cargo fmt --all                 # format
cargo clippy --workspace --all-targets -- -D warnings
$env:CI='true'; cargo test --workspace      # PowerShell; bash: CI=true cargo test --workspace
```

`--workspace` on clippy is not optional. Without it, only `default-members`
(`client`, `hud`, `mello-core`, `mello-sys`) are linted and every crate under
`tools/` is skipped. A lint error hid there until `release.yml` — the only place
that used `--workspace` — failed and blocked a release.

Useful filters:

```bash
cargo test -p mello-core --lib reconnect      # reconnect supervisor unit tests
cargo test -p mello-core --lib command::tests
cargo test -p mello-core --lib events::tests
```

Highlights of what's covered:

- **`mello-core/src/client/reconnect.rs`** — `ReconnectSupervisor` is pure
  decision logic (backoff, wake-gap detection, lost/recovered edges) with
  deterministic unit tests using injected clocks. This is the regression net for
  sleep/wake + reconnect behaviour; no backend required.
- **`events.rs` / `command.rs`** — lock the FFI JSON shape (adjacently-tagged
  `{"type","data"}`) used across the core↔client boundary.

### Which build is this? `mello --build-info`

A harness that runs journeys must know which commit a binary came from.
`mello --build-info` prints one line of JSON and exits with code 0. It starts no UI, no logging, no updater and no
single-instance lock. The flag works anywhere in the arguments.

```bash
SLINT_EMIT_DEBUG_INFO=1 cargo build -p mello-client --no-default-features --features development,e2e
target/debug/mello --build-info
# {"commit":"<40 hex>","dirty":false,"built_at":"2026-10-02T09:12:00Z","version":"0.0.0-DEV","features":["development","e2e"]}
```

- `dirty` is `true` when the worktree had uncommitted changes at build time.
- The stamp is made by the small `build-stamp` crate. It has its own build script, so the Slint UI does not
  compile again after a Rust edit.
- Cargo runs the stamp again when HEAD, the index or a client or `mello-core` source file changes.
  An edit in another crate does not run it. Rebuild after a commit if you need an exact stamp.
- Without git (a source tarball), `commit` is `"unknown"` and the build still works.
- In GitHub Actions, the version edits in `Cargo.toml`, `Cargo.lock` and `Info.plist` do not set `dirty`.
- The client also logs the same line at startup (`Build: {...}`).
- A Windows release build uses the GUI subsystem. Read the line from a pipe or a file, not from an interactive console.

---

## libmello — C++ audio/video DSP

Unit tests live in `libmello/tests/` (echo canceller, VAD, Opus, the NetEQ
playout, the voice receive path, noise suppressor, video pipeline). Per CLAUDE.md:

`MELLO_BUILD_TESTS` defaults to **OFF** — `mello-sys/build.rs` configures libmello
on every `cargo build`, and we don't want the gtest suite compiled into every dev
build. You must opt in explicitly, or `ctest` will report *"No tests were found"*.

Run from the repo root. `CMAKE_TOOLCHAIN_FILE` **must be absolute** — CMake
resolves a relative value against the build directory, not your shell's cwd, so
a relative path fails with *"Could not find toolchain file"*.

```bash
cmake -B libmello/build -S libmello \
  -DMELLO_BUILD_TESTS=ON \
  -DCMAKE_TOOLCHAIN_FILE="$PWD/external/vcpkg/scripts/buildsystems/vcpkg.cmake" \
  -DVCPKG_TARGET_TRIPLET=arm64-osx     # x64-windows-static-md on Windows
cmake --build libmello/build
CI=true ctest --test-dir libmello/build --output-on-failure
```

Windows notes:

- The Visual Studio generator is multi-config. Pass `-C Release` to every
  `cmake --build` and `ctest` call, or ctest probes an unbuilt Debug tree.
- ONNX DLLs (`onnxruntime.dll`, `onnxruntime_providers_shared.dll`) copy next
  to `mello_tests` at build time via a POST_BUILD step. Without this step the
  suppressor and VAD tests fail with `err=126`.
- Run `check-full.sh` under Git Bash (`C:\Program Files\Git\bin\bash.exe`).
  `C:\Windows\system32\bash.exe` (WSL) chokes on the CRLF line endings that
  `core.autocrlf=true` checkouts produce.
- The vendored webrtc-audio-processing tree pins C++20 on its targets
  (designated initializers; MSVC rejects them under C++17) and defines
  `WIN32_LEAN_AND_MEAN` (winsock include order). Keep both on engine upgrades.

**`CI=true` is required here for the same reason as `cargo test --workspace`.**
`VideoPipelineTest` does real monitor capture + hardware encode; its `SetUp`
(`libmello/tests/test_video_pipeline.cpp`) skips when `CI` is set. Without it,
on a machine lacking screen-recording permission the capture path dies with
**SIGTRAP** rather than failing cleanly — three tests "fail" for reasons that
have nothing to do with your change.

Four binaries: `mello_tests`, `mello_rtp_tests`, `mello_procenum_tests` and
`mello_neteq_spike_tests` (the stage 3 NetEQ proofs). Under `CI=true` the
hardware cases skip (`VideoPipelineTest` and the device suites). If you see 0
tests, the `-DMELLO_BUILD_TESTS=ON` flag didn't take.

Voice capture and speech gate (test backend, Silero, the voice gate corpus):

| Suite | File | What it proves |
|---|---|---|
| `VoiceCaptureTest`, `PacketSinkTest`, `InputSensitivityTest` | `test_voice_capture.cpp` | RTP media time per frame and over a gate gap, the packet sink, the input sensitivity gate threshold. |
| `SpeechGateTest` | `test_speech_gate.cpp` | The pre-roll and the first live frame leave at the captured level (within 0.5 dB), not through the expander. The Silero decision latency from the speech onset to the first encoded frame, no pre-roll, over the corpus spurts: mean 34 ms or less, worst 50 ms or less (measured 30 and 40; 38.8 and 60 before the restart and peek). |

Voice receive and playout (no device, no sleep, deterministic):

| Suite | File | What it proves |
|---|---|---|
| `VoicePlayout` | `test_voice_playout.cpp` | One NetEQ per peer: clean playout, a new SSRC and a sender restart start a new stream, reorder is not a restart, late frames count a stall and not a talk pause, a long gate gap leaves no delay, the pre-roll burst is held and not learned as jitter, real jitter still raises the target, a target jump does not stretch speech, volume and pan per peer, two peers mix, removal keeps lifetime counts, junk is rejected. |
| `ParseRtp`, `StreamResampler` | `test_voice_playout.cpp` | The RTP parser (CSRC, extension, padding) and the sinc resampler of the WASAPI edges (rate, level, frequency, channel order, block sizes). |
| `VoiceReceiveTest` | `test_voice_receive.cpp` | Through a real `AudioPipeline` on the test backend: the loopback voice frame and SFU RTP packets decode through NetEQ, losses add no delay, single losses use in-band FEC (speech corpus), an outage recovers, late underruns, deafen, volume, leave and peer removal. |
| `NetEqSpike` | `test_neteq_spike.cpp` | The stage 3 spike: NetEQ and the mixer driven directly (clean, loss, burst tuning, drift, gap, limiter, cost). |

Voice send-leg adaptation (spec 10 sections 4.6 and 7.5):

| Suite | File | What it proves |
|---|---|---|
| `SendAdaptation` | `test_send_adaptation.cpp` (`mello_tests`) | The controller with a trace of reports, one per second: the rung sequence (down on loss with round-trip inflation or jitter growth, one rung up after 10 s without congestion), 13 % random loss with a flat round trip and flat jitter does not step down, an ineffective step goes back one rung and holds 30 s, report arrival jitter does not widen a window, one lossy second steps down once, too few packets give no decision, the loss perc mapping (floor 5, cap 30, 4 s), the ladder from the channel bitrate. |
| `SendAdaptationPipeline` | `test_send_adaptation.cpp` (`mello_tests`) | Through a real `AudioPipeline` on the test backend: a report reaches the encoder at the next frame and the frames shrink with the bitrate; a new session starts at the top rung. |
| `VoiceSendFeedback` | `test_voice_send_feedback.cpp` (`mello_rtp_tests`) | The RTCP parser with crafted receiver and sender reports (compound, signed 24-bit loss, malformed lengths), the round trip from LSR and DLSR, the per-report interval of the tracker. |
| `VoiceSendFeedbackWire` | `test_voice_send_feedback.cpp` (`mello_rtp_tests`) | A plain libdatachannel peer answers our sender reports with crafted receiver reports; they reach the voice peer's send track and its callback with the right loss, jitter and a real round trip. |

RTP transport (sender, receiver, peer roles) — release gate for stream PRs:

```powershell
cmake --build libmello/build --config Release --target mello_rtp_tests
libmello/build/tests/Release/mello_rtp_tests.exe
```

Run when you touch anything under `libmello/src/audio/` or `libmello/src/video/`
(read `specs/03-LIBMELLO.md` first — threading/COM/callback invariants).

Stream viewer (streaming-quality stage 3, spec 12 section 7):

| Suite | File | What it proves |
|---|---|---|
| `RtpH264ReceiverTest` and siblings | `test_rtp_h264_receiver.cpp` (`mello_rtp_tests`) | Assembly, out-of-order emit before an older incomplete access unit, frame ids and references, padding packets, NACK every max(RTT, 10 ms) until the repair deadline, a new RTT applies at once, a key frame still arriving is not lost, a loss is reported once after its deadline, a key frame ends older repairs, discontinuity, memory bounds. Simulated clock. |
| `VideoReceiveTimeline`, `ReceivePath` | `test_video_receive_timeline.cpp` (`mello_rtp_tests`) | The timeline alone (release on the RTP clock, key-frame waits and retries, loss, decode error, the 150 ms cap, the minimum delay after a reset) and behind the receiver on a simulated lossy link: 2 % single losses cost no key frame, an unrepairable burst costs one, the target delay returns within 5 s after a 3 s jitter spike. |
| `ViewerSizeChange` | `test_capture_reference.cpp` (`mello_tests`, skips under `CI`; needs the OpenH264 DLL and, for the hardware cases, NVDEC or AMF) | A stream that steps from 640x360 to 320x180 at an IDR (as after a resolution rung) shows whole pictures at each size with the software and the hardware decoder; the hardware decoder shows the picture of each access unit with that access unit, as OpenH264 does. |
| `ViewerPipelineTest`, `ViewerConverter` | `test_video_viewer.cpp` (`mello_tests`, needs a D3D11 adapter) | A fake decoder through the real ring, present thread and converter: frames shown at their render time (interval p95 under 20 ms), only the newest due frame shown, a size change gives a new shared texture, the shared frame opens on a second device and reads under keyed-mutex key 0, the R8 (NVDEC) layout converts. |

Stream game audio and A/V sync (streaming-quality stage 7, spec 12 sections
9.3 and 9.4):

| Suite | File | What it proves |
|---|---|---|
| `StreamPlayoutTest` | `test_stream_audio_pipeline.cpp` (`mello_tests`) | Through a real `AudioPipeline` on the test backend, pulled 10 ms at a time: the game audio plays in the voice mix (not a voice peer), mute is silent at the next 10 ms and the first packet after unmute plays, a device change keeps the game audio, the volume scales it, reordered packets are not concealed, a lost packet is repaired from in-band FEC, the sync sample maps the heard audio onto the video clock. |
| `StreamAudioSyncDrift` | `test_stream_audio_pipeline.cpp` | 10 simulated minutes with the host's audio clock 300 ppm fast: the audio's mean end-to-end delay (last minute against the first minute after settling) moves by under 10 ms (NetEQ accelerates; the mapping takes each packet's capture time). A guard holds each 1 s sample under 20 ms from the start (one time-stretch step plus the band); the test prints the accelerate chunk sizes. |
| `StreamAudioHostFec` | `test_stream_audio_pipeline.cpp` | Receiver reports of 15 % loss give the encoder an expected loss of 15 % and in-band FEC in its packets; each frame's capture time reaches the link. The game loss rule: no FEC on a clean link, any loss gives at least 10 % and LBRR in the packets. |
| `AvSyncExtension`, `AvSync`, `AvSyncInput` | `test_av_sync.cpp` (`mello_rtp_tests`) | The header extension (add to a packet with and without one, CSRCs, the two-byte form), the decision rule (dead band, steps of at most 10 ms, a late video lowers the video minimum before it delays the audio, a jump over 1 s is ignored), and the rule against the real receive timeline on a simulated clock (the minimum follows the audio both ways; no audio for 3 s resets it). |
| `AvSyncWire` | `test_av_sync.cpp` | A stream host peer and a viewer peer in process: the game-audio packet carries the video RTP timestamp of its capture time (the frame's timestamp plus 3600 ticks for 40 ms). |

### Streaming quality gate

`tools/stream-gate` plays a deterministic reference clip through the real
stream path and scores what the viewer presents against a frozen baseline
(plans/streaming-quality.md stage 1). Every change to the stream path
reports its delta per profile and per metric. Windows only: the reference
source is D3D11.

```bash
./scripts/stream-gate.sh                       # CPU lane, all 7 profiles, about 3 minutes
./scripts/stream-gate.sh --lane hw             # hardware lane (NVENC/AMF/QSV and their decoders)
./scripts/stream-gate.sh --only clean,mobile   # some profiles
./scripts/stream-gate.sh --no-vmaf             # PSNR and SSIM only
./scripts/stream-gate.sh --repeat 3            # every profile 3 times, prints the spread
./scripts/stream-gate.sh --report-only         # print every gate, enforce none
./scripts/stream-gate.sh --clip game.yuv --clip-size 1920x1080   # a real I420 clip: transport metrics only, no baseline
```

The exit code is 1 when VMAF drops by more than 1 point on any profile
with a VMAF floor (more where `gates.json` `vmaf_max_drop_profile` records a
wider measured spread), when freeze time rises by more than the profile's
spread (`freeze_ms_max_rise`), or when a gate of `gates.json` fails. It is 2
for a setup error and for a run on a busy machine.

| Part | Where | What it does |
|---|---|---|
| Path | `tools/stream-gate/src/session.rs` | Reference source → D3D11 video processor (NV12 to BGRA) → preprocessor → encoder → mello-core `StreamManager` and `SfuSink` → RTP sender → shim → mello-sfu in test mode → shim → RTP receiver session → receive timeline → the viewer's decode thread → its present thread at each frame's render time → CPU frame callback (the client's viewer since stage 3: `mello_stream_viewer_attach_peer`; the 16 ms loop only sets the receive target, pings and reads the SFU events). Host and viewers run in one process, each on its own `MelloContext`, in the client's order (native host start, then SFU connect and join). |
| Clip | `tools/stream-gate/src/clip.rs` | 20 s at 60 fps, 1920x1080 and a 1280x720 cut: a panning textured background, six bouncing textured sprites, two scrolling text lines, a scene cut at 10 s, a static section from 14 s to 17 s. Integer arithmetic and a frozen generator: rendered on first use into `benchmarks/clips/` (git-ignored, 1.7 and 3.7 GB) and checked against the FNV-1a hashes in `profiles.json`. |
| Burned-in code | `tools/stream-gate/src/code.rs` | Top-left 512x48 px: the frame index, the clip clock in ms and a CRC-16, as 16x16 blocks. The viewer reads it from every presented frame. |
| Reference source | `libmello/src/video/capture_reference.cpp` | `MELLO_CAPTURE_REFERENCE` with `mello_test_set_reference_clip`: plays the clip at a fixed rate, frame n due at start + n/fps, never skipped, looped (spec 14 §4.7). |
| Lanes | `profiles.json` `lanes` | `cpu`: 720p60 at 5000 kbps (preset medium), OpenH264 encode and decode in software (`MELLO_SOFT_ENCODER=1`, `MELLO_SOFT_DECODER=1`). `hw`: 1080p60 at 8000 kbps (preset ultra), the machine's encoder and decoder. |
| Shim | `tools/stream-gate/src/shim.rs`, `netmodel.rs` | A UDP proxy on `127.0.0.2:10000`, the SFU's one ICE-lite candidate (`SFU_PUBLIC_IP=127.0.0.2`); the SFU's mux listens on `0.0.0.0:10000`. One flow per client. Forward direction of a leg: every impairment; reverse direction: base delay and outage. Loss per packet, jitter and the Gilbert-Elliott state per 10 ms slot, seeded (shim version 1). It reads the clear SRTP headers to count video, FEC (PT 127), retransmissions (a repeated SSRC and sequence) and RTCP per direction. |
| SFU | `tools/stream-gate/src/sfu.rs` | `go build ./cmd/sfu` from `MELLO_SFU_DIR` (default `../mello-sfu`) with `GOTOOLCHAIN=go1.24.9` (the production Go), run with a random `SFU_ADMIN_PASSWORD`, `SFU_ICE_TCP_PORT=0`, loopback ports, no metering, R2 or TLS. Test-mode join (01-SFU.md §5.1). Log: `target/stream-gate/<lane>/sfu.log`. |
| Scores | `score.rs`, `scripts/stream-gate-score.py` | PSNR and SSIM on luma per new presented frame against the clip frame of the same index. VMAF: `vmaf` 3.0.0 CLI (Netflix release `vmaf.exe`, SHA-256 pinned in `stream-gate.sh`, fetched into `target/stream-gate/tools/`), model `vmaf_v0.6.1`, luma. |
| Profiles | `benchmarks/baselines/stream/profiles.json` | `clean`, `home-wifi` (viewer leg 2 % loss, 20 ms jitter), `mobile` (5 %, 40 ms, 250 ms spikes, reorder, 300 ppm drift), `bad` (10 % with bursts, 30 ms), `congested` (host uplink capped at 60 % of the preset with a 150 ms tail-drop queue, 5 s to 25 s), `outage` (host leg down 3 s from 8 s), `late-join` (a second viewer at 5 s, measured). 10 ms each way on every leg. |
| Baselines | `baseline-2.1-cpu.json`, `baseline-2.1-hw.json` | Taken once, never edited. Commit, machine, GPU, driver, encoder, decoder, clip hash, seeds, SFU commit and Go, scorer versions. Both were taken on commit `c8a4bc7`, before the rebase onto the P2P removal (`f562f90`): the rebased tree is that tree minus the P2P deletions, which change no media path. |
| Gates | `benchmarks/baselines/stream/gates.json` | Per lane and profile: VMAF floor, freeze-time ceiling, time-to-first-frame ceiling, glass-to-glass p95 ceiling, fps floor, send rate within the cap. Made by `tools/stream-gate/make_gates.py` from the baseline and repeat runs on the same commit: the baseline value minus or plus the largest of a fixed margin, a share of the value and 1.1 x the spread of the runs. A metric missing in any run (no frame presented) has no gate. |
| Game audio (stage 7) | `tools/stream-gate/src/audio.rs`, `benchmarks/baselines/stream/audio.json` | The reference clip gets a track (`mello_test_set_reference_audio`): the voice gate corpus speech low-passed at 7 kHz, a 20 ms tone burst every 250 ms of clip time (marker k at one of 8 frequencies, 10 to 15.25 kHz, captured with video frame k x 15) and a 17.25 kHz pilot tone. The host plays it on the clip's clock with exact capture times; each viewer's context output is pulled 10 ms at a time on a fixed schedule (`mello_voice_test_pull_output`). A/V offset: each burst onset heard minus the present time of its video frame. Gaps: the output silent (30 dB under its median) for 20 ms or more; dropouts (reported): the pilot 12 dB down for 20 ms or more, which a concealed lost packet also gives. MOS: PESQ-WB on the speech, 4 s segments aligned by the markers, with the voice gate's scorer venv. Not in `inputs_fingerprint` (results.json has `audio_fingerprint`): the stage 4 baselines have no audio metrics, so they print as values. `audio.json` holds the stage 7 gates (home-wifi: 0 gaps, 95 % of the markers within +-40 ms; mobile: MOS at least the voice gate's `mobile` minus 0.2). |

Metrics (definitions in `tools/stream-gate/src/metrics.rs`):

- **New frame**: a present whose burned-in frame differs from the one
  before. **Dropped**: clip frames between the first and the last presented
  frame that never reached the screen. **Delivered fps**: new frames per
  second.
- **Freeze**: a gap over 150 ms between new frames, after the first frame,
  including a gap that runs to the end. Freeze time is the whole gap.
- **Time to first frame**: first present minus the viewer's join start.
- **Glass-to-glass**: present time minus the capture time of the frame
  (the source's start time plus the burned-in clock), same clock
  (`mello_clock_now_us`).
- **Wire**: host uplink rate, FEC bytes and retransmitted bytes per video
  byte, SFU-to-viewer retransmissions; on `congested` the send rate in the
  cap, its overshoot, the queue drops and the time until the send rate is
  at or under the cap.
- **Game audio** (stage 7, `audio.rs`): `av_within_40ms_pct` (markers heard
  with an offset within +-40 ms, in % of the markers whose video frame was
  shown while the output was pulled, 0.5 s from either end; a marker not
  heard counts as outside), `av_offset_p50_ms` and `av_offset_abs_p95_ms`,
  `audio_gaps` and `audio_gap_ms` (silences), `audio_dropouts` and
  `audio_dropout_ms` (concealment, not gated), `audio_mos`, and the viewer's
  own numbers at the end (`audio_delay_ms`, `av_sync_offset_ms`,
  `av_video_min_delay_ms`, `audio_expand_ms`, `audio_fec_used`). Per profile
  `av.csv` has each marker's offset and `sync.csv` the viewer's sync state
  every 100 ms.

Rules:

- **Frozen inputs.** The clip version and hashes, the shim version, the
  lanes, the profiles and seeds are in `inputs_fingerprint`. The harness
  refuses to compare across a change. Re-take the baselines on the commit
  they came from to change one.
- **The clock is real.** The gate runs in real time, through real sockets
  and a real SFU, so a run varies. `--repeat N` measures the spread; the
  freeze rule allows the spread at the baseline, nothing more.
- **Keep the machine quiet.** A build, a game or a soak on the same machine
  moves every number (two `cargo clippy` runs of other worktrees took the
  CPU lane's clean profile from 48 to 13-26 fps). The load guard
  (`load.rs`) measures the CPU that other processes use during each run;
  over 10 % of the machine, the run is INVALID (exit code 2) and nothing is
  compared or written. The run also stops at setup when UDP port 10000 is
  taken.
- **Median of three on a failure.** A profile outside a gate runs twice
  more, and the gates read the median of its three runs (`--no-confirm`
  turns this off). A profile that passes only on a lucky run does not pass.
- **Setup on first run**: Go (fetches go1.24.9), Python 3 (standard library
  only), network for the OpenH264 2.6.0 DLL (Cisco, into
  `libmello/third_party/openh264/`) and `vmaf.exe` 3.0.0, both checked
  against a pinned SHA-256.
- `check-full.sh` runs a 60 s smoke of the CPU lane (`clean`, report only)
  only with `STREAM_GATE_SMOKE=1`. The full gate runs from the QA harness.

Output (`results.json`, and per profile `presents.csv`, `host.csv`,
`wire.csv`) lands in `target/stream-gate/<lane>/`.

`--write-fixture FILE` encodes the first 2 s of the 720p clip with the
OpenH264 encoder: the H.264 fixture of mello-sfu `tools/stream-soak`.

### Stream start, watch and reconnect (streaming-quality stage 6)

The gate measures the media path with the harness's own host and viewer.
The client's start, watch and reconnect logic has its own tests. They run in
`cargo test --workspace`:

| What | Where |
|---|---|
| Reconnecting at 700 ms of silence, resume on the same connection within 5 s, rejoin after 5 s or a drop on the voice schedule (counted from each attempt's start), give-up 30 s after the drop; the viewer's host-silence check | `mello-core/src/client/stream_link.rs` |
| A failed `start_stream`, capture or SFU step takes the session back and names the cause; `advertise: "on_live"`; live only when the capture and the track are up, then `stream_live` | `client/stream_reconnect_tests.rs`, `streaming.rs` (`stream_lifecycle_tests`), `stream_host_start.rs` |
| Against a fake SFU with live native peers that answers WebSocket pings only (as mello-sfu does for streams): a short silence resumes on the same connection; a dropped socket and a 5 s silence rejoin the same session as the host, and the sink moves to the new connection with an IDR | `client/stream_reconnect_tests.rs` |
| A 10 s backend stall: END STREAM, the SFU `leave` and the next command do not wait on `stop_stream` | `client/stream_reconnect_tests.rs` (`end_stream_and_the_loop_do_not_wait_on_a_stalled_backend`) |
| The viewer keeps its leg on `host_reconnecting` and on its own drop; only `session_ended` ends the watch | `client/stream_watch.rs`, `stream_reconnect_tests.rs`, `streaming.rs` (`host_reconnecting_keeps_the_viewer`) |
| The sink with a late connection: backpressure, the settings kept for the attach | `mello-core/src/stream/sink_sfu.rs` |
| `stream_live`, `refresh_stream_token`, the deferred advert | `backend/nakama/data/modules/streaming_test.go` |
| The SFU refuses a token for another region | mello-sfu `internal/auth/jwt_test.go` |

`stream_reconnect_live` (`mello-core/src/client/stream_reconnect_live_tests.rs`)
runs the stage 6 gate against a real mello-sfu: a host that streams monitor 0
(hardware encoder) and a viewer (hardware decoder) in one process, with real
command loops and a fake Nakama that hands out test-mode tokens. The host's
signaling socket goes through a TCP proxy and every UDP flow through a proxy
on the SFU's candidate `127.0.0.2:10000` (the gate's shim address, so no
stream gate may run at the same time). A "cable pull" freezes the host's TCP
and drops its UDP flows. It reports click to first RTP, Watch to first
frame, a 3 s and a 10 s pull ("reconnecting" on the viewer within 1 s, the
picture back within 3 s of the cable, same session) and a 35 s pull (both
sides end with a message). Ignored by default:

```bash
# mello-sfu (the deployed SFU), from Git Bash
GOTOOLCHAIN=go1.24.9 go build -o /tmp/sfu6/sfu.exe ./cmd/sfu
SFU_ADMIN_PASSWORD=devpass SFU_PUBLIC_IP=127.0.0.2 SFU_ICE_TCP_PORT=0 \
  SFU_ADDR=127.0.0.1:18443 SFU_ADMIN_ADDR=127.0.0.1:18080 SFU_REGION=test \
  /tmp/sfu6/sfu.exe
# mello
MELLO_TEST_SFU=ws://127.0.0.1:18443/ws MELLO_TEST_SFU_PASSWORD=devpass \
  cargo test -p mello-core --lib -- --ignored --nocapture stream_reconnect_live
```

Log lines to read in a client log: "Stream start timing:
click_to_first_rtp_ms=…" and "Stream watch timing: click_to_first_frame_ms=…".

---

## Voice quality gate

`tools/voice-gate` pushes a speech corpus through the real libmello voice
path and scores the result against a frozen baseline. It needs no audio
device and no backend. Every change to the audio path runs it
(plans/voice-quality.md stage 7).

```bash
./scripts/voice-gate.sh                    # all profiles, about 7.5 minutes
./scripts/voice-gate.sh --only clean,wrap  # some profiles (a name also selects its extra seeds)
./scripts/voice-gate.sh --no-score         # structural metrics only
./scripts/voice-gate.sh --send-tick        # old sender timing, for comparison
./scripts/voice-gate.sh --no-adapt         # no send-leg feedback (before stage 5)
./scripts/voice-gate.sh --report-only      # print every gate, enforce none
```

The table shows each metric with its delta against
`benchmarks/baselines/voice/baseline-2.9-slot.json`. The exit code is 1 when
MOS drops by more than 0.1 on any profile or an enforced gate fails. The
gate enforces on Windows x86-64 only, the platform of the baseline. On
another platform it reports only (seeds, baselines and platforms, below).

| Part | Where | What it does |
|---|---|---|
| Corpus | `libmello/tests/fixtures/voice/` | Four public-domain LibriVox clips, 48 kHz mono. Frozen. |
| Profiles and gates | `benchmarks/baselines/voice/profiles.json` | `clean`, `home-wifi`, `mobile`, `bad`, `burst`, `drift`, `outage`, `wrap`: loss, bursts, jitter, stalls, reorder, outage, clock drift, RTP wrap, seeds. |
| Added profiles | `benchmarks/baselines/voice/profiles-congestion.json` | Profiles added after the baseline, with their own seeds and fingerprint (`added_inputs_fingerprint`). `congestion`: a rate-limited bottleneck with a tail-drop queue; `"compare_no_adapt": true` also runs it as `congestion-no-adapt` without feedback. No baseline values. |
| Sender | `mello_voice_inject_capture`, `mello_voice_set_packet_sink` | Real capture DSP and Opus encode on a context with the test backend. Each packet leaves at its encode time, as on the SFU path. `--send-tick` replays the 20 ms voice tick of the command loop instead (the send timing before stage 2). |
| Shim | `tools/voice-gate/src/shim.rs` | Builds the RTP packet as the SFU forwards it: payload type 111, the 16-bit sequence from `rtp_seq_start`, the timestamp (a seeded start + media time), a seeded SSRC. Then applies the profile. Seeded, one set of draws per 20 ms send slot (shim version 2, below). |
| Feedback | `tools/voice-gate/src/rtcp.rs` | The SFU's receiver reports (a port of Pion's receiver stream, one report per second) from the delivered packets, and the sender reports that give the round trip. Each report reaches the sender's encoder adaptation (`mello_voice_report_send_feedback`) at its arrival time. A run replays the shared sender trace with a probe context until the controller changes a setting, then encodes live with the reports applied. The lossless twin replays the packets that the live sender sent. Jitter draws from the seed stream `rtcp`. |
| Receiver | `mello_voice_feed_rtp`, `mello_voice_test_pull_output` | Real NetEQ per peer (decode, FEC, concealment, time stretch), the mixer and the output ring, pulled 10 ms at a time like the device thread. |
| Scorer | `scripts/voice-gate-score.py` | PESQ wideband (ITU-T P.862.2), `pesq==0.0.4`, MOS-LQO per clip. Optional. |

Test hooks in `mello.h` (not for production use):

- `MELLO_AUDIO_BACKEND=test` before `mello_init()` selects a device-free
  backend. No device opens and no device thread starts.
- `mello_voice_test_pull_output()` pulls mixed output through the playback
  render path.
- `mello_test_set_clock_ms()` drives the receive clock. NetEQ times its
  playout by the 10 ms pulls. The harness steps the clock 1 ms at a time, so
  a run is deterministic and faster than real time.
- `MelloDebugStats` has the NetEQ statistics at its end (`rx_*`, spec 10
  section 5.4).
- Test backend only: `MELLO_NETEQ_DELAY_MANAGER_CONFIG` replaces the delay
  manager field trial value (`default` = upstream),
  `MELLO_VOICE_PRE_ROLL_FRAMES` changes the sender pre-roll, and
  `MELLO_NETEQ_SPURT_HOLD=0` turns the talk-spurt hold off (talk-spurt gate,
  below). `MELLO_GAME_NETEQ_DELAY_MANAGER_CONFIG` does the same as the
  first for the game audio of streams (stage 7; the stream gate runs on the
  test backend). For tuning runs and measurements, never in a committed
  result.

Rules:

- **Frozen inputs.** The corpus, the shim version (`"shim"`), the profile
  parameters, the seeds, the analysis settings and the scorer version are
  fixed. The harness refuses to
  compare when they differ from the baseline. To change one, re-take the
  baseline on the commit the old baseline came from.
- **Gates may change.** A gate with `"enforced": false` prints but does not
  fail. The stage that fixes a defect sets its gate to `true`.
- **Metrics of a removed playout print n/a.** The NetEQ playout has no fill
  PLC, no per-kind concealment counters and no overflow drops. Those cells
  print `n/a` against the baseline, with no delta. Concealment per lost frame
  is now voice expand (concealed minus silent concealed samples) plus frames
  repaired from in-band FEC, per lost frame.
- **Concealable losses only.** The denominator of `conceal_per_lost` is the
  network loss and the receiver drops, without lost Opus DTX frames and
  without losses at the head of a talk spurt before any packet of that spurt
  was delivered (NetEQ has played nothing of the spurt and starts it with
  the next packet). `lost_spurt_head_frames` counts the second kind.
  Since stage 3b the gate is informational (talk-spurt gate, below).
- **Seeds.** A profile with `extra_seeds` (in either file) also runs as
  `<name>@<seed>`, without gates; the gate reads `<metric>_median` over the
  seeds.
- **MOS from different scorers is never compared.** Without the scorer the
  gate prints structural metrics only and says so.
- `cargo test --workspace` runs a 14 s smoke test of the same path, so
  `check.sh` catches broken wiring.

Output (WAV of reference and output, delay curve CSV, receiver stats CSV,
`results.json`) lands in `target/voice-gate/`. With the feedback loop each
profile also gets `sender.csv` (the bitrate and loss perc every 100 ms) and
`reports.csv` (each receiver report and the setting after it), and
`results.json` carries both under `info.adaptation`.

### Seeds, baselines and platforms

**Shim version 2: draws per send slot.** Each random stream of the shim
(loss, burst state and drop, jitter, reorder) gives one set of draws per
20 ms send slot. The slot of a packet is its RTP timestamp offset (48 kHz
media time from the first captured sample) divided by 960. To reach the
slot of a packet, each stream advances through every slot up to that slot.
A slot without a packet (DTX, a closed gate) consumes its draws, and the
Gilbert-Elliott state takes one step per slot. A sender that sends one
packet more or less therefore meets the same network in every later slot.
Version 1 (baseline 2.9) drew once per packet: `ed4e817` sends one packet
less on `mobile`, and every later draw moved. A sender without gaps (the
push-to-talk `drift` profile) gets the same draws under both versions.
`profiles.json` and `profiles-congestion.json` carry `"shim": 2`; the
harness refuses a file for another version.

**In-clip growth over three seeds.** `latency_clip_growth_ms` is one seed's
median over the clips. Where a stall lands against a clip boundary decides
it, so it spreads over seeds by more than its 20 ms limit. `home-wifi`,
`mobile` and `bad` run with `extra_seeds` (seed + 1, seed + 2). Their
enforced gate is `latency_clip_growth_ms_median`, at most 20 ms; the
single-seed value stays as an informational gate. Each extra seed is one
more 93 s profile (about 27 s of run time with the live sender, and 8 more
clips to score): on the Windows machine the full gate went from 279 s
to 454 s.

**Baselines.**

| File | Audio state | Shim | Seeds | Platform | Use |
|---|---|---|---|---|---|
| `baseline-2.9.json` | af8c7f0 | 1, per packet | one per profile | Windows x86-64 | Record of 2.9. Not read. Never edited. |
| `baseline-2.9-slot.json` | af8c7f0 | 2, per slot | with the extra seeds | Windows x86-64 | The gate compares with it. |

The harness on `main` needs libmello hooks that af8c7f0 does not have
(packet sink, `mello_voice_feed_rtp`, send feedback). So the af8c7f0
harness takes the baseline, as it took baseline 2.9, with
`benchmarks/baselines/voice/baseline-2.9-slot-harness.patch`: the slot
shim, `extra_seeds`, the seed medians and the 2.9-slot output. That
libmello has no packet timestamp. The patch gives each packet the media
time of its frame: frame c / 2 - 1 after capture chunk c, and the pre-roll
frames of a gate opening just before it. On the current tree this rule
gives the RTP timestamp of all 3987 packets. Without the patch the af8c7f0
tree reproduces baseline 2.9 exactly.

Baseline 2.9 against 2.9-slot (same audio, other draws):

| Profile | MOS 2.9 | MOS 2.9-slot | In-clip growth 2.9 | 2.9-slot | Note |
|---|---:|---:|---:|---:|---|
| clean | 3.28 | 3.27 | -5.0 | -5.0 | |
| home-wifi | 2.58 | 2.47 | 60.1 | 69.9 (median 90.1) | Seeds 203, 204: 2.55, 2.55. |
| mobile | 1.84 | 1.92 | 184.9 | 170.1 (median 170.1) | Seeds 304, 305: 1.93, 1.99. |
| bad | 1.43 | 1.42 | 402.6 | 358.1 (median 358.1) | Seeds 405, 406: 1.44, 1.38. |
| burst | 3.19 | 3.05 | 0 | 0 | Worst clip 2.26 to 1.58: the burst's losses fall in other speech. |
| drift | 4.26 | 4.26 | 0.67 | 0.67 | Identical: no gaps in the stream. |
| outage | 2.96 | 2.96 | 0 | 0 | |
| wrap | 3.55 | 3.78 | -16.9 | 0 | The old buffer's wrap defect (2 late drops, 80 ms dropout) does not fire with the new jitter draws. |

The 2.9 receiver concealed every loss twice and let the delay grow, so
where a loss falls moves its MOS: over three seeds the same profile spreads
by 0.06 to 0.08. The shifts are of that size, except `burst` and `wrap`,
whose one event lands differently.

**Does `ed4e817` move delay growth?** `ed4e817` (the Silero decision at the
end of each frame at an onset) sends one packet less on `mobile`. With shim
version 1 that moved every later draw, and seed 303 went from 3.25 to
29.6 ms. The same question with the network held fixed (shim version 2):
`mobile`, 16 seeds, `--only mobile --no-score`, on `8a29f53` (the commit
before) and on `ed4e817`, each with this harness. Windows x86-64,
2026-10-13. In-clip growth in ms; packets: the encoded trace, then the
packets fed to the receiver.

| Seed | 8a29f53 | Packets | ed4e817 | Packets | Change |
|---:|---:|---:|---:|---:|---:|
| 303 | 28.81 | 3988 / 3772 | 8.38 | 3987 / 3770 | -20.44 |
| 304 | -17.12 | 3988 / 3768 | -6.00 | 3987 / 3766 | +11.12 |
| 305 | -11.50 | 3988 / 3755 | -22.38 | 3987 / 3755 | -10.88 |
| 306 | 13.25 | 3988 / 3786 | 9.38 | 3987 / 3786 | -3.88 |
| 307 | -2.00 | 3988 / 3801 | -4.75 | 3987 / 3799 | -2.75 |
| 308 | -5.75 | 3988 / 3806 | 5.38 | 3987 / 3805 | +11.12 |
| 309 | 89.00 | 3988 / 3785 | 92.50 | 3987 / 3782 | +3.50 |
| 310 | -5.00 | 3988 / 3793 | -5.56 | 3987 / 3794 | -0.56 |
| 311 | 7.00 | 3988 / 3820 | 10.25 | 3987 / 3823 | +3.25 |
| 312 | -1.38 | 3988 / 3791 | -2.38 | 3987 / 3789 | -1.00 |
| 313 | -9.25 | 3988 / 3786 | -4.56 | 3987 / 3784 | +4.69 |
| 314 | 23.62 | 3988 / 3810 | 16.38 | 3987 / 3810 | -7.25 |
| 315 | 8.19 | 3988 / 3777 | 20.00 | 3987 / 3776 | +11.81 |
| 316 | 0.12 | 3988 / 3790 | -0.38 | 3987 / 3787 | -0.50 |
| 317 | -5.94 | 3988 / 3796 | -6.94 | 3987 / 3793 | -1.00 |
| 318 | -7.25 | 3988 / 3794 | -6.50 | 3987 / 3793 | +0.75 |
| Median | -1.69 | | -1.38 | | -0.53 |
| Mean | 6.55 | | 6.43 | | -0.13 |
| Over 20 ms | 3 of 16 | | 1 of 16 | | 7 up, 9 down |

The median moves 0.3 ms and the mean 0.1 ms, against the 5 ms that would
call for a look at the commit. The per-seed change has a spread of 8 ms
both ways. `ed4e817` does not move delay growth; the 26 ms on seed 303 with
shim version 1 was the reshuffled network. One seed (309) reads 89 to
93 ms on both commits: one seed alone is not fit to gate, which is why the
gate reads the median over three. The three gate seeds (303 to 305) give a
median of -11.50 ms before and -6.00 ms after.

**Platforms.** The enforced numbers and the baselines are Windows x86-64
numbers. On macOS arm64 the receiver's float path (Opus, NetEQ, ONNX)
gives other values for the same packets (mobile in-clip growth 29.6 ms
against 16.4 ms with shim version 1). `scripts/voice-gate.sh` therefore:

1. takes `baseline-2.9-slot-<os>-<arch>.json` when it exists for the host,
   else `baseline-2.9-slot.json` (`<os>`, `<arch>`: Rust's names, for
   example `macos`, `aarch64`);
2. reads `"platform"` from that file;
3. passes `--report-only` when the host is another platform.

`--report-only` prints every metric and every gate with its result and a
banner: "report only: the enforced numbers and the baseline are Windows
x86-64; this platform is macOS arm64". It exits 1 only for decode errors or
a scorer that ran and gave no MOS for a clip; a run or setup error exits 2.
`VOICE_GATE_PLATFORM=darwin/arm64` replaces the host platform, to test the
rule on Windows.

To enforce on another platform, take a baseline there. The procedure is
the one of `baseline-2.9-slot.json`:

```bash
git worktree add --detach ../mello-bl af8c7f0
cd ../mello-bl
git submodule update --init
git apply ../mello/benchmarks/baselines/voice/baseline-2.9-slot-harness.patch
cp ../mello/benchmarks/baselines/voice/profiles.json benchmarks/baselines/voice/
CI=true ./scripts/voice-gate.sh --write-baseline
```

On a host other than Windows x86-64 the patched harness writes
`baseline-2.9-slot-<os>-<arch>.json` and `.md`. Copy both next to
`baseline-2.9-slot.json` and commit them; from then on the script enforces
on that platform with its own numbers. Use the same Silero model file as
the other platform (`libmello/models/silero_vad.onnx`).

### Send-leg adaptation (stage 5)

Gates:

| Profile | Gate | Enforced | Rule |
|---|---|---|---|
| every baseline profile | `mos` | yes | At least the stage 4 MOS minus 0.1. |
| `congestion` | `bitrate_reaction_s` | yes | At most 2 s from the end of the first 2 s in which the network lost more than 10 % of the sent packets, to the first step down. |
| `congestion` | `bottleneck_delay_mean_ms` | yes | At least 100 ms below `congestion-no-adapt` (`max_ref`). |
| `congestion` | `mos` | yes | At least the MOS of `congestion-no-adapt` minus 0.2 (`min_ref`): the bounded quality cost of the delay gain. |

Results (`--no-adapt` gives the stage 4 numbers on the baseline profiles):

| Profile | MOS stage 4 | MOS stage 5 | Bitrate min / mean kbps | Loss perc max | Ladder steps |
|---|---:|---:|---:|---:|---:|
| clean | 3.27 | 3.27 | 64 / 64 | 5 | 0 |
| home-wifi | 3.25 | 3.25 | 64 / 64 | 5 | 0 |
| mobile | 2.80 | 2.80 | 64 / 64 | 15 | 0 |
| bad | 2.22 | 2.20 | 32 / 57.4 | 27 | 3 (one wrong step at 61 s, back at 67 s) |
| burst | 3.29 | 3.28 | 48 / 61.2 | 12 | 2 (the jitter burst) |
| drift | 4.25 | 4.25 | 64 / 64 | 5 | 0 |
| outage | 2.98 | 2.98 | 64 / 64 | 30 | 0 |
| wrap | 3.74 | 3.74 | 64 / 64 | 5 | 0 |

| `congestion` | With the ladder | `congestion-no-adapt` |
|---|---:|---:|
| MOS | 2.91 | 3.07 |
| Delay mean in the bottleneck (ms) | 387 | 521 |
| Delay p95 (ms) | 508 | 558 |
| Queue drops | 42 | 180 |
| Reaction (s) | 1.72 | - |

Why the MOS gate allows a cost of 0.2. The tail-drop queue of a single flow drops
isolated packets, and in-band FEC repairs them: 175 of the 180 drops without
the ladder. So without adaptation the bottleneck costs delay, which PESQ
does not score. With the ladder, NetEQ accelerates to give back the delay
when the queue drains (308 ms of accelerate against 130 ms), and PESQ
penalizes the time stretching. A sender fixed at 32 kbps scores 3.27 on the
same profile: the low rate is not the cost; the drain and the probe step up
are. Other bottlenecks (48 kbps or a 100 to 150 ms queue, 20 s) give the
ladder +0.02 to +0.06 MOS, within the per-clip noise of PESQ.

Follow-ups:

- TWCC on the voice track: one receiver report per second sees a queue only after it overflows; transport-wide feedback would see it fill.
- Cross traffic in the bottleneck model: other flows make the queue drop bursts that FEC cannot repair, which a single-flow model does not show.

### Talk-spurt gate (stage 3b)

plans/voice-quality.md stage 3b, option A: a short pre-roll, the Silero
decision on its chunk, the pre-roll exempt from the expander.

Onset metric (`tools/voice-gate/src/analysis.rs`, `onsets`):

- A talk spurt starts at the first 10 ms reference frame at the active level
  (-45 dBFS) after at least 500 ms below it. The pause is longer than the
  sender gate needs to close (about 420 ms), so each spurt is a gate open.
- Onset loss: the output's level over the spurt's first 200 ms against the
  reference's, at the spurt's measured delay, less the same over the rest of
  the spurt's first second. That takes out the AGC, which gives each talker
  its own gain. Positive is quieter. A spurt with no matching output (an
  outage) is not measured.
- Metrics: `onset_loss_db_mean`, `onset_loss_db_p95`, `onset_spurts`; each
  spurt in `results.json` under `info.onsets`. The clean profile has 20
  spurts. With push-to-talk (`drift`) the metric reads 0 +- 0.4 dB, except
  the first spurt of the run (AGC2 starts at 0 dB): about 2 dB.
- The baseline finding of 25 to 33 dB low onsets (plans/voice-quality.md
  section 8a) is not seen with this metric on the stage 6 code: mean 0.61
  dB, p95 1.70 dB. The pre-roll frames came out up to 20 dB low, but the
  first 200 ms of a spurt hold most of their energy in the live frames.

Test backend switches (never in a committed result):

| Variable | Effect |
|---|---|
| `MELLO_VOICE_PRE_ROLL_FRAMES=N` | Sender pre-roll, 0 to 50 frames |
| `MELLO_NETEQ_SPURT_HOLD=0` | Talk-spurt controller without the pre-roll handling (spec 10 section 5.2); the jump rule stays |
| `MELLO_NETEQ_SPURT_HOLD_FLOOR_MS=N` | Spurt lateness below N ms gets no hold (default 40) |
| `MELLO_NETEQ_JUMP_NO_EXPAND_MS=N` | No preemptive expand for N ms after a target jump (default 5000) |

Sweep, 2026-10-08. "B+C" is this stage: the pre-roll exempt from the
expander, the expander at unity when the gate opens, the Silero restart and
peek. MOS marked (!) is more than 0.1 below stage 6. Stage 6 MOS: clean
3.27, home-wifi 3.25, mobile 2.80, bad 2.20, burst 3.28, drift 4.25, outage
2.98, wrap 3.74, congestion 2.91.

| Row | Clean p50 | Clean p95 | Clean MOS | Onset mean dB | Onset p95 dB | home-wifi | mobile | bad | burst | drift | outage | wrap | congestion | Enforced gates that fail |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| stage 6, 5 (shipped before) | 183.0 | 187.0 | 3.27 | 0.61 | 1.70 | 3.25 | 2.80 | 2.20 | 3.28 | 4.25 | 2.98 | 3.74 | 2.91 | none |
| stage 6, 2 | 124.2 | 131.5 | 3.23 | 0.52 | 1.57 | 3.18 | 2.70 | 2.06 (!) | 3.13 (!) | 4.25 | 3.05 | 3.74 | 3.02 | mobile conceal 1.17, mobile MOS 2.7, bad MOS 2.06, burst recovery 5.75, burst MOS 3.13, congestion bottleneck delay 462 |
| stage 6, 1 | 101.5 | 111.5 | 3.22 | 0.49 | 1.25 | 3.07 (!) | 2.71 | 2.17 | 3.11 (!) | 4.25 | 2.88 | 3.53 (!) | 2.86 | home-wifi MOS 3.07, burst recovery 6.75, burst MOS 3.11, wrap MOS 3.53 |
| stage 6, 0 | 103.8 | 110.2 | 3.07 | 0.51 | 1.34 | 2.98 (!) | 2.61 (!) | 2.06 (!) | 2.92 (!) | 4.25 | 2.88 (!) | 3.58 (!) | 2.79 (!) | clean MOS 3.07, home-wifi MOS 2.98, mobile MOS 2.61, bad MOS 2.06, burst recovery 10.8, burst MOS 2.92, outage MOS 2.88, wrap MOS 3.58, congestion bottleneck delay 457, burst MOS vs 2.9 0.264, clean MOS vs 2.9 0.217 |
| B+C, 5 (ships) | 183.5 | 192.8 | 3.38 | 0.05 | 0.39 | 3.28 | 2.87 | 2.29 | 3.38 | 4.25 | 3.02 | 3.86 | 3.10 | bad conceal 0.864, burst recovery 5.75, congestion bottleneck delay 467 |
| B+C, 4 | 161.8 | 171.5 | 3.36 | 0.04 | 0.43 | 3.29 | 2.84 | 2.25 | 3.30 | 4.25 | 3.05 | 3.88 | 3.00 | mobile conceal 0.947, burst recovery 5.5 |
| B+C, 3 | 144.0 | 148.0 | 3.35 | 0.06 | 0.46 | 3.30 | 2.84 | 2.24 | 3.30 | 4.25 | 3.11 | 3.88 | 2.99 | home-wifi conceal 0.946, bad conceal 1.05, burst recovery 5.75 |
| B+C, 2 | 123.5 | 131.5 | 3.32 | 0.08 | 0.98 | 3.26 | 2.84 | 2.17 | 3.24 | 4.25 | 3.10 | 3.86 | 2.98 | home-wifi conceal 0.947, burst recovery 13.5 |
| B+C, 1 | 103.8 | 111.5 | 3.31 | -0.05 | 0.30 | 3.19 | 2.78 | 2.24 | 3.16 (!) | 4.25 | 2.94 | 3.68 | 2.97 | burst recovery 14.5, burst MOS 3.16, congestion bottleneck delay 471 |
| B+C, 0 | 101.5 | 112.8 | 3.17 | 0.08 | 0.52 | 3.12 (!) | 2.64 (!) | 2.16 | 3.10 (!) | 4.25 | 2.95 | 3.68 | 2.90 | home-wifi MOS 3.12, mobile MOS 2.64, burst recovery 14.5, burst MOS 3.1, congestion bottleneck delay 445, clean MOS vs 2.9 0.111 |
| B+C hold off, 5 | 172.0 | 183.8 | 3.08 | 0.06 | 0.50 | 3.13 (!) | 2.88 | 2.31 | 3.22 | 4.25 | 2.83 (!) | 3.72 | 2.98 | clean clip growth -34.8, clean MOS 3.08, home-wifi conceal 0.939, home-wifi MOS 3.13, bad conceal 0.876, burst recovery 5.5, outage recovery 20, outage MOS 2.83, congestion bottleneck delay 482, clean MOS vs 2.9 0.199, outage MOS vs 2.9 0.125 |
| B+C hold off, 2 | 112.0 | 125.8 | 3.32 | 0.02 | 0.46 | 3.25 | 2.79 | 2.18 | 3.23 | 4.25 | 3.06 | 3.86 | 2.94 | home-wifi conceal 0.937, bad conceal 0.949, burst recovery 14.5 |
| B+C hold off, 1 | 101.5 | 111.5 | 3.29 | -0.05 | 0.30 | 3.23 | 2.77 | 2.21 | 3.19 | 4.25 | 3.02 | 3.74 | 3.02 | mobile clip growth 22.5, burst recovery 5.75, congestion bottleneck delay 506 |
| B+C hold off, 0 | 87.8 | 108.2 | 3.26 | 0.09 | 0.45 | 3.07 (!) | 2.69 (!) | 2.14 | 3.10 (!) | 4.25 | 2.92 | 3.70 | 2.88 | home-wifi MOS 3.07, mobile MOS 2.69, burst recovery 6.75, burst MOS 3.1 |

Attribution and one receive-side probe (not shipped):

| Row | Clean p50 | Clean p95 | Clean MOS | Onset mean dB | Onset p95 dB | home-wifi | mobile | bad | burst | drift | outage | wrap | congestion | Enforced gates that fail |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| B only, 5 | 187.5 | 191.5 | 3.34 | 0.07 | 0.47 | 3.32 | 2.85 | 2.25 | 3.38 | 4.25 | 3.03 | 3.81 | 3.05 | bad conceal 0.947, congestion bottleneck delay 473 |
| B only, 1 | 104.0 | 111.5 | 3.25 | 0.01 | 0.30 | 3.12 (!) | 2.76 | 2.23 | 3.14 (!) | 4.25 | 2.90 | 3.64 (!) | 2.84 | home-wifi MOS 3.12, mobile conceal 0.95, burst recovery 6.75, burst MOS 3.14, wrap MOS 3.64 |
| B+C forget_factor 0.5, 1 | 103.8 | 111.5 | 3.30 | -0.05 | 0.31 | 3.18 | 2.65 (!) | 2.18 | 3.16 (!) | 4.25 | 2.94 | 3.68 | 3.01 | mobile MOS 2.65, burst recovery 5.75, burst MOS 3.16, congestion bottleneck delay 461 |
| B+C forget_factor 0.5 hold off, 1 | 102.2 | 111.5 | 3.29 | -0.05 | 0.30 | 3.13 (!) | 2.70 | 2.27 | 3.26 | 4.25 | 3.02 | 3.74 | 3.00 | home-wifi MOS 3.13, mobile MOS 2.7, burst recovery 5.75, congestion bottleneck delay 507 |

What the sweep shows:

- B and C raise MOS on every gated profile at 5 frames (clean 3.27 to
  3.38, bad 2.20 to 2.29, burst 3.28 to 3.38, wrap 3.74 to 3.86) and cut the
  clean onset loss p95 from 1.70 to 0.39 dB.
- The selection rule of the brief (lowest delay; onset p95 no worse than
  the 5-frame row; MOS within 0.1 of stage 6 on every profile) picks 5
  frames with the talk-spurt hold on. 1 frame passes it only with the hold
  off (clean p50 101.5 ms, MOS within 0.1 everywhere). 0 frames loses
  0.13 to 0.16 MOS on home-wifi and mobile. 2 to 4 frames have a worse
  onset p95 (0.43 to 0.98 dB).
- No row of this stage passes every enforced gate. The second pass
  (below) corrected two metrics and tuned the receive path within the
  existing knobs.

Metric corrections (second pass):

- `conceal_per_lost` counts concealable losses only. A lost packet at the
  head of a talk spurt, before any packet of that spurt was delivered, leaves
  the denominator, as a lost Opus DTX frame does (`shim::SpurtLosses`, test
  `a_lost_spurt_head_is_not_concealable`). Stage 6 under the corrected rule:
  home-wifi 0.984, mobile 0.982, bad 0.956 (0.952 before; 2 head losses).
  It does not move bad at 5 frames with B and C (0.864; no head loss). The
  cause there is the lossless twin: its concealment for jitter and stalls is
  283.7 frames against 246.4 on stage 6 (`info.lossless_twin_neteq`), while
  the run's total is the same (728.6 against 731.0). Losses that fall in a
  stall share one expand with it in the run, and not in the twin.
- `congestion` and `congestion-no-adapt` run on seeds 909, 910 and 911; the
  delay gate reads the median (`bottleneck_delay_mean_ms_median`, the
  threshold is unchanged). Bottleneck delay mean, ms, ladder / no-adapt:
  stage 6 387, 387, 398 / 521, 519, 516; B and C at 5 frames 467, 403, 465 /
  541, 525, 510. The seeds do not cause the spread. The ladder's second step
  (48 to 32 kbps) comes at 19.1 s on stage 6 and at 22.1 s with B and C on
  2 of 3 seeds: at 19 s the window loss is 11.2 % (over 10 %), but the full
  tail-drop queue dropped the sender reports, so no fresh round trip
  confirms the congestion, and the jitter test does not (6.4 against
  5.1 ms). The step at 19 s gives 403 ms, at 22 s 465 ms.

Receive-path tuning (second pass). Knobs: the delay manager field trial
(ff = `forget_factor`, rff = `reorder_forget_factor`, `quantile`), the
spurt-hold floor (`MELLO_NETEQ_SPURT_HOLD_FLOOR_MS`, inert at 1 frame: the
lateness is 20 ms), the preemptive-expand window after a target jump
(`MELLO_NETEQ_JUMP_NO_EXPAND_MS`, PE window). Rows without a congestion
value ran without it. MOS gates: at most 0.1 under stage 4.

| Pre-roll | Hold | Delay manager | Controller | burst recovery s | drift growth ms | burst MOS | home-wifi MOS | mobile MOS | bad MOS | outage MOS | mobile clip growth ms | bad conceal | congestion median ms (no-adapt) | Enforced gates that fail |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|
| 1 | off | ff 0.6, rff 0.98 (shipped tuning) | - | 5.75 | 3.7 | 3.19 | 3.23 | 2.77 | 2.21 | 3.02 | 22.5 | 0.968 | - | mobile clip growth 22.5, burst recovery 5.75 |
| 1 | on | ff 0.6, rff 0.98 | - | 14.50 | 2.4 | 3.16 | 3.19 | 2.78 | 2.24 | 2.94 | -5.0 | 0.960 | - | burst recovery 14.5, burst MOS 3.16 |
| 1 | off | ff 0.5 | - | 5.75 | 3.9 | 3.26 | 3.13 | 2.70 | 2.27 | 3.02 | 16.8 | 0.963 | - | home-wifi MOS 3.13, mobile MOS 2.7, burst recovery 5.75 |
| 1 | off | ff 0.4 | - | 5.50 | 2.7 | 3.22 | 3.14 | 2.73 | 2.24 | 3.02 | -4.5 | 0.955 | - | home-wifi MOS 3.14, burst recovery 5.5 |
| 1 | off | quantile 0.90 | - | 5.75 | 3.9 | 3.26 | 3.18 | 2.70 | 2.21 | 3.02 | -7.1 | 0.955 | - | mobile MOS 2.7, burst recovery 5.75 |
| 1 | off | quantile 0.85 | - | 5.50 | -3.3 | 3.21 | 3.17 | 2.70 | 2.25 | 3.03 | -5.5 | 0.955 | - | mobile MOS 2.7, burst recovery 5.5 |
| 1 | off | ff 0.5, quantile 0.90 | - | 5.50 | -3.3 | 3.22 | 3.14 | 2.71 | 2.24 | 3.02 | -4.8 | 0.955 | - | home-wifi MOS 3.14, burst recovery 5.5 |
| 1 | off | rff 0.95 | - | 5.75 | 3.7 | 3.19 | 3.22 | 2.78 | 2.21 | 3.02 | 19.1 | 0.971 | - | burst recovery 5.75 |
| 1 | off | ff 0.4, rff 0.95 | - | 5.50 | 2.7 | 3.22 | 3.17 | 2.71 | 2.23 | 3.02 | -8.5 | 0.955 | - | burst recovery 5.5 |
| 1 | off | ff 0.3, rff 0.95 | - | 5.50 | -3.3 | 3.19 | 3.17 | 2.66 | 2.23 | 3.02 | -4.6 | 0.953 | - | mobile MOS 2.66, burst recovery 5.5 |
| 1 | off | quantile 0.85, rff 0.95 | - | 5.50 | -3.3 | 3.21 | 3.15 | 2.70 | 2.26 | 3.03 | -5.5 | 0.956 | - | mobile MOS 2.7, burst recovery 5.5 |
| 1 | off | quantile 0.80, rff 0.95 | - | 5.50 | -3.3 | 3.21 | 3.17 | 2.69 | 2.25 | 3.03 | -8.0 | 0.956 | - | mobile MOS 2.69, burst recovery 5.5 |
| 1 | off | ff 0.5, quantile 0.85, rff 0.95 | - | 5.50 | -3.3 | 3.20 | 3.17 | 2.69 | 2.26 | 3.03 | -8.0 | 0.958 | - | mobile MOS 2.69, burst recovery 5.5 |
| 1 | off | ff 0.4, quantile 0.90, rff 0.95 | - | 5.50 | -3.3 | 3.20 | 3.18 | 2.69 | 2.26 | 3.03 | -8.0 | 0.957 | - | mobile MOS 2.69, burst recovery 5.5 |
| 1 | off | rff 0.95 | PE window 0 s | 5.75 | 2.9 | 3.19 | 3.18 | 2.75 | 2.18 | 3.02 | 19.2 | 0.957 | - | burst recovery 5.75 |
| 1 | off | rff 0.95 | PE window 2 s | 5.75 | 0.7 | 3.19 | 3.19 | 2.73 | 2.21 | 3.02 | 1.9 | 0.959 | - | burst recovery 5.75 |
| 5 | on | ff 0.6, rff 0.98 (shipped tuning) | - | 5.75 | 2.4 | 3.38 | 3.28 | 2.87 | 2.29 | 3.02 | 16.4 | 0.864 | 465 (525) | bad conceal 0.864, burst recovery 5.75, congestion congestion delay 465 |
| 5 | on | rff 0.95 | - | 5.75 | 2.4 | 3.38 | 3.28 | 2.87 | 2.29 | 3.02 | 3.8 | 0.864 | 465 (525) | bad conceal 0.864, burst recovery 5.75, congestion congestion delay 465 |
| 5 | on | ff 0.5, rff 0.95 | - | 5.75 | -5.1 | 3.37 | 3.30 | 2.81 | 2.25 | 3.05 | 15.1 | 0.848 | 456 (514) | bad conceal 0.848, burst recovery 5.75, congestion congestion delay 456 |
| 5 | on | ff 0.4, rff 0.95 | - | 2.75 | -3.3 | 3.37 | 3.30 | 2.77 | 2.19 | 3.05 | 5.4 | 0.855 | 423 (492) | bad conceal 0.855, congestion congestion delay 423, congestion MOS 2.96 |
| 5 | on | quantile 0.90 | - | 5.75 | -5.1 | 3.37 | 3.30 | 2.84 | 2.20 | 3.05 | 8.8 | 0.854 | 458 (514) | bad conceal 0.853, burst recovery 5.75, congestion congestion delay 458 |
| 5 | on | rff 0.95 | PE window 2 s | 5.75 | 5.9 | 3.38 | 3.28 | 2.83 | 2.29 | 3.02 | 12.1 | 0.861 | 465 (532) | bad conceal 0.861, burst recovery 5.75, congestion congestion delay 465 |

What the tuning shows:

- Burst recovery at 1 frame stops at 5.5 s for every setting. The talk
  spurt that spans the burst end runs to 29.7 s. NetEQ's target is down to
  20 ms 3 to 3.5 s after the burst (ff 0.3 to 0.6), but accelerate drains
  the buffer only about 60 ms per second of speech: at 29.5 s it still holds
  67 ms (ff 0.3), and the delay is 129 ms against a limit of 124 ms. The
  next spurt (30.9 s) plays at 106 to 124 ms. The first point under the
  limit is at 30.5 s: 5.5 s. With the hold on, the post-burst spurts play
  20 ms later (NetEQ target 40 ms from 31.8 s to 39.3 s), 0.1 to 1 ms over
  the limit: 14.5 s.
- Mobile clip growth with the hold off: a stall at the start of a spurt
  enters NetEQ's arrival statistics (with the hold on, the first 200 ms of
  a spurt stay out) and the spurt plays at the raised target (32.2 s: 376 ms
  against 244 ms). The metric is the median over 8 clips of values from -78
  to +194 ms, so one clip moves it. ff 0.4 or rff 0.95 bring it under 20 ms.
- At 5 frames ff 0.4 with rff 0.95 brings burst recovery to 2.75 s, but
  congestion MOS falls to 2.96 (limit 2.97) and the congestion and bad
  concealment gates still fail.

Third pass (2026-10-08): three approved changes.

| Change | Result in the gate |
|---|---|
| Send side: congestion also on loss over 10 % with no fresh round trip for 2 reports in a row (spec 10 section 4.6) | No change in any profile. Our sender reports and the SFU's receiver reports both run on 1 s, so every other receiver report carries no fresh round trip also on a clean link; at the late step (19 s, `reports.csv` column `rtt_fresh`) only one report in a row is stale. The fresh sample before it (339 ms) is inflated, but the 3 s window holds one sample and needs two. |
| Receive side: NetEQ fast accelerate on | No change in any profile, also not in MOS: it acts only at a playout delay of 4 x (target + max arrival delay + 20 ms). After the burst the buffer is at most about 1.5 x that. |
| Metric: concealment inside each lost packet's playout window, no lossless twin | Not adopted: on stage 6 it reads home-wifi 0.96 to 0.99, mobile 0.88 to 0.92, bad 0.88 to 0.91 (window variants), against 0.98, 0.98 and 0.96 with the twin. That moves the natural level by more than 0.05. NetEQ expand for one 20 ms loss is often less than 960 samples (merge), and the media time to output position offset (about 30 to 40 ms: codec and DSP delay, output ring) must be estimated per run. The twin method stays. |

Rows with both changes (the metric of the second pass; congestion limit in
brackets):

| Row | Enforced gates that fail |
|---|---|
| 1 frame, hold off, ff 0.6 rff 0.98 | mobile clip growth 22.5 ms (20); burst recovery 5.75 s (5); congestion median 506 ms (436) |
| 1 frame, hold off, ff 0.4 rff 0.95 | burst recovery 5.5 s (5); congestion median 472 ms (398) |
| 1 frame, hold on, ff 0.6 rff 0.98 | burst recovery 14.5 s (5); burst MOS 3.16 (3.19); congestion median 467 ms (432) |
| 1 frame, hold on, ff 0.4 rff 0.95 | mobile MOS 2.68 (2.70); burst recovery 5.75 s (5); burst MOS 3.18 (3.19); congestion median 406 ms (391) |
| 5 frames, hold on, ff 0.6 rff 0.98 (branch default) | bad conceal 0.864 (0.95); burst recovery 5.75 s (5); congestion median 465 ms (425) |
| 5 frames, hold on, ff 0.4 rff 0.95 | bad conceal 0.855 (0.95); congestion median 423 ms (392); congestion MOS 2.96 (2.97) |

Fourth pass (2026-10-09): two measurement changes and a stage 5 fix.

- `conceal_per_lost` is informational. On stage 6 the total concealment on
  `bad` is the same (728.6 against 731.0 frames); NetEQ's expand for one
  lost frame is often shorter than a frame (merge), and a loss in a stall
  shares one expand with it. Delay growth, its harm, stays enforced
  (`latency_clip_growth_ms`, drift).
- `recovery_s` reads the NetEQ buffer (packet buffer, sync buffer, output
  ring), the audio that accelerate removes. The run samples it at every
  10 ms pull. A pull counts when NetEQ concealed less than the whole pull.
  The curve is the mean per 250 ms over the counted pulls (`buffer.csv`); a
  bin of mostly concealment (a pause, an outage) has no point, so an empty
  buffer is never a recovery. Same tolerance (20 ms), hold (2 s) and limit
  (5 s). The target delay is not used: it is the level accelerate aims for,
  and it comes back seconds before the buffer does. Informational:
  `recovery_e2e_s` (the earlier definition) and `recovery_target_s`.
- The send ladder: when the 3 s round-trip window holds exactly one fresh
  sample, the sample counts as inflated at 3 x max(30 s median, 50 ms). The
  two-sample rule (2 x) is unchanged. The 48 to 32 kbps step on
  `congestion` is at 19.1 s again on all three seeds; reaction 1.72 s.

| Row | burst recovery s (buffer / e2e / target) | burst buffer before, peak ms | congestion median ms (limit) | Enforced gates that fail |
|---|---|---|---:|---|
| 5 frames, hold on (branch default) | 4.50 / 5.75 / 3.50 | 108, 265 | 397 (425) | none |
| 1 frame, hold off | 14.25 / 5.75 / 3.75 | 32, 289 | 402 (436) | mobile clip growth 22.5 ms (20); burst recovery 14.25 s (5) |
| 1 frame, hold on | 14.25 / 14.5 / 3.75 | - | 384 (432) | burst recovery 14.25 s (5); burst MOS 3.16 (3.19) |

Outage recovery is 0 s on the buffer in every row. `bad` conceal per lost:
0.864 at 5 frames, 0.968 at 1 frame hold off, 0.960 hold on.

Why 1 frame still fails burst recovery: NetEQ's target is back to 20 ms
3.75 s after the burst. The buffer drains in the spurt that spans the
burst end (289 ms at 25.5 s, 83 ms at 30 s). In the next spurts (31 s to
39 s) the delay manager holds a target of 40 ms, so the buffer plays at
about 52 ms against 32 ms before the burst: 20 ms over, on the tolerance
line. At 5 frames the pre-roll hold sets the target (100 ms before, 110 ms
after the burst), and the buffer stays 14 ms over its level before the
burst. 1 frame is not shipped; the default stays 5 frames.

Fifth pass (2026-10-09): the delay manager at 1 frame, hold off. Sweep of
`forget_factor` (ff) x `reorder_forget_factor` (rff) on burst, mobile, bad
and home-wifi. "Target 20 ms" is when NetEQ's target is back to 20 ms after
the burst (it ends at 25 s). In every row the target then holds 40 ms from
31.6 s to 38 to 39 s, and again from 48.3 s.

| ff | rff | burst recovery s (buffer / target / e2e) | burst MOS | mobile clip growth ms | mobile MOS | bad MOS | home-wifi MOS | Target 20 ms at s | Enforced gates that fail (4 profiles) |
|---:|---:|---|---:|---:|---:|---:|---:|---:|---|
| 0.6 | 0.98 | 14.25 / 3.75 / 5.75 | 3.19 | 22.5 | 2.77 | 2.21 | 3.23 | 29.3 | burst recovery; mobile clip growth |
| 0.6 | 0.95 | 14.25 / 3.75 / 5.75 | 3.19 | 19.1 | 2.78 | 2.21 | 3.22 | 29.3 | burst recovery |
| 0.6 | 0.9 | 14.25 / 3.75 / 5.75 | 3.19 | 25.0 | 2.78 | 2.21 | 3.22 | 29.3 | burst recovery; mobile clip growth |
| 0.5 | 0.98 | 5.25 / 3.50 / 5.75 | 3.26 | 16.8 | 2.70 | 2.27 | 3.13 | 28.8 | burst recovery; mobile MOS; home-wifi MOS |
| 0.5 | 0.95 | 5.25 / 3.50 / 5.75 | 3.26 | 17.0 | 2.72 | 2.26 | 3.13 | 28.8 | burst recovery; home-wifi MOS |
| 0.5 | 0.9 | 5.25 / 3.50 / 5.75 | 3.26 | 19.9 | 2.71 | 2.23 | 3.13 | 28.8 | burst recovery; home-wifi MOS |
| 0.4 | 0.98 | 4.75 / 2.75 / 5.50 | 3.22 | -4.5 | 2.73 | 2.24 | 3.14 | 28.3 | home-wifi MOS |
| 0.4 | 0.95 | 4.75 / 2.75 / 5.50 | 3.22 | -8.5 | 2.71 | 2.23 | 3.17 | 28.3 | none |
| 0.4 | 0.9 | 4.75 / 2.75 / 5.50 | 3.22 | -4.5 | 2.71 | 2.25 | 3.17 | 28.3 | none |
| 0.3 | 0.98 | 4.50 / 2.25 / 5.50 | 3.19 | -4.2 | 2.67 | 2.25 | 3.14 | 27.8 | mobile MOS; home-wifi MOS |
| 0.3 | 0.95 | 4.50 / 2.25 / 5.50 | 3.19 | -4.6 | 2.66 | 2.23 | 3.17 | 27.8 | mobile MOS |
| 0.3 | 0.9 | 4.50 / 2.25 / 5.50 | 3.19 | -0.9 | 2.66 | 2.28 | 3.17 | 27.8 | mobile MOS |
| 0.4, hold on | 0.95 | 5.25 / 3.00 / 5.75 | 3.18 | 12.5 | 2.68 | 2.22 | 3.18 | 28.0 | burst recovery; burst MOS; mobile MOS |

The two rows that pass on the four profiles fail the full gate on
`congestion` MOS: 2.86 against no-adapt 3.10 - 0.2 = 2.90, with ff 0.4 and
rff 0.95 or 0.9. The faster forget drains the queue delay with more
accelerate (406 ms against 139 ms without the ladder), which PESQ scores.
Congestion MOS, seed 909 (910, 911): ff 0.6 rff 0.95 2.92 (2.88, 2.90);
ff 0.5 rff 0.95 2.92 (2.88, 2.85); ff 0.4 rff 0.95 2.86 (2.90, 2.89); ff 0.3
rff 0.95 2.78 (2.83, 2.88). No row passes every enforced gate. The
defaults stay: 5 frames, hold on, ff 0.6 rff 0.98.

The other fields of the delay manager field trial are `quantile` (0.95),
`start_forget_weight` (2), `resample_interval_ms` (500),
`use_reorder_optimizer` (true) and `ms_per_loss_percent` (20). None was
changed. `MELLO_NETEQ_SPURT_HOLD_FLOOR_MS=20` with the hold on gives the same
result as the hold on: at 1 frame the cost comes from the 200 ms of each
spurt that stay out of the arrival statistics, not from the held delay.

Mixed versions. A receiver with the hold off and ff 0.4 rff 0.95 that plays
a 5-frame sender: clean MOS 3.07 (3.38 with the hold on), onset loss p95
2.42 dB, clean in-clip growth -61.9 ms, burst recovery 13.5 s.

To rerun one row:

```bash
MELLO_VOICE_PRE_ROLL_FRAMES=1 MELLO_NETEQ_SPURT_HOLD=0 \
  ./scripts/voice-gate.sh --out target/voice-gate-pr1
```

### NetEQ tuning (stage 3)

The delay manager tuning and the talk-spurt controller (spec 10 section 5.2)
were chosen with the gate. Sweep, all rows with the pre-roll hold of the
talk-spurt controller unless noted. The binding gates are the burst recovery
(5 s), the drift growth (±20 ms) and the MOS of `burst`, `mobile` and `bad`
against stage 2 (3.39, 2.80, 2.26; at most 0.1 lower).

| Delay manager | Preemptive expand | mobile | bad | burst | burst recovery s | drift growth ms |
|---|---|---:|---:|---:|---:|---:|
| `forget_factor:0.7` | upstream | 2.66 | 2.12 | 3.29 | 5.75 | 2.3 |
| `forget_factor:0.8` | upstream | 2.76 | 2.14 | 3.27 | 10.8 | - |
| `forget_factor:0.6` | upstream | 2.69 | 2.13 | 3.29 | 4.75 | 22.4 (before a controller fix) |
| `forget_factor:0.5` | upstream | 2.69 | 2.09 | 3.27 | 3.25 | - |
| `forget_factor:0.6` | none | 2.80 | 2.24 | 3.29 | 5.75 | - |
| `forget_factor:0.6` | none for 20 ms+ deficits | 2.73 | 2.23 | 3.29 | 4.75 | 26.5 (low start-up level) |
| `forget_factor:0.6` | none for 5 s after a target jump (ships) | 2.80 | 2.22 | 3.29 | 4.75 | 2.4 |
| upstream NetEQ, no controller, `forget_factor:0.7` | upstream | 2.62 | 2.20 | 2.98 | 5.50 (outage: 21 s) | -0.5 |
| time stretching off (test only) | off | 2.92 | 2.27 | 3.36 | 5.50 | - |

`reorder_forget_factor:0.98` in every row. To rerun one row:

```bash
MELLO_NETEQ_DELAY_MANAGER_CONFIG=forget_factor:0.5,reorder_forget_factor:0.98 \
  ./scripts/voice-gate.sh --only mobile,bad,burst,drift --out target/voice-gate-sweep
```

### Voice send path off the command loop

`mello-core/src/client/voice_send_path_tests.rs` runs the real command loop
against a fake SFU. The fake SFU answers the offer with a native libmello peer
and records each voice RTP packet with its arrival time.
`Command::TestHoldLoop` (test builds only) blocks the loop thread with a sleep.

- `sfu_voice_frames_reach_the_peer_while_the_loop_is_held`: the regression
  test. It runs in `cargo test --workspace`.
- `send_path_latency_with_the_loop_idle_and_held`: the stage 2 gate
  measurement, 1000 frames idle and 1000 frames held. Ignored by default:

```bash
cargo test -p mello-core --lib send_path_latency -- --ignored --nocapture
```

### Voice receive slots and in-band speaking (stage 4)

Unit tests run in `cargo test --workspace`: the slot message rebinds a mid and
drops the old member's NetEQ, and a `speaking` message raises `VoiceActivity`
(`transport/sfu_connection.rs`), the rate limits (`voice/speaking.rs`), and
the UI keeps the SFU speaking state over a stale `voice_update`
(`client/src/flow_tests.rs`).

`slots_live_join_and_speaking_latency` (`mello-core/src/voice/slots_live_tests.rs`)
measures the stage 4 gate against a real SFU: 7 members talk, an 8th joins.
It reports SFU `joined` to the first packet heard in each direction, and VAD
open to the remote `VoiceActivity`. Ignored by default:

```bash
# mello-sfu
SFU_PUBLIC_IP=127.0.0.1 SFU_ADMIN_PASSWORD=devpass go run ./cmd/sfu
# mello
MELLO_TEST_SFU=ws://127.0.0.1:8443/ws MELLO_TEST_SFU_PASSWORD=devpass \
  cargo test -p mello-core --lib -- --ignored --nocapture slots_live
```

The SFU side (offers per join, the 50-member path past the slots) is
`voice-soak --max-offers-within-slots 0`, below.

### SFU region, reconnect and slot hold (stage 6)

Unit tests run in `cargo test --workspace` and in libmello `ctest`:

| What | Where |
|---|---|
| Region ranking: the median of three probes, 2xx only, 2 s timeout, fastest first | `mello-core/src/transport/sfu_region.rs` |
| `get_sfu_regions` lists both regions with their health URLs | `backend/nakama/data/modules/sfu_routing_test.go` |
| First reconnect attempt after 500 ms, then 1 s, then every 2 s; no gap over 2 s, and the give-up 30 s after the drop; a valid voice token is reused, an expired one is not | `mello-core/src/client/voice.rs` |
| A server close of the signaling socket is a disconnect; the close after `left` is not; a connect without an answer fails at its 5 s limit | `mello-core/src/client/sfu_voice_join.rs` |
| No local ICE candidate before the offer; a candidate found after the answer still reaches the SFU (a fake SFU and a fake STUN server that answers late) | `mello-core/src/client/sfu_voice_join.rs`, `transport/sfu_connection.rs` |
| The ICE servers reach libdatachannel: a TURN server on a local UDP port gets the Allocate request | `transport/sfu_connection.rs` |
| A link with no pong for 700 ms is stalled | `mello-core/src/voice/mod.rs` |
| The "reconnecting" state on the channel header and the local member's row; the region in the debug panel | `client/src/flow_tests.rs` |
| RTP before its `slot` message plays once the slot binds; RTP held 300 ms drops and counts in `rx_unmapped_dropped`; a late packet of the released owner never reaches the next owner | `libmello/tests/test_voice_slot_hold.cpp` |

`sfu_reconnect_live_timings` (`mello-core/src/client/sfu_reconnect_live_tests.rs`)
measures the reconnect against a real SFU in Docker. It kills or pauses the
container three times and reports the time to the "reconnecting" state and
from the link coming back to audio in both directions, with the event
timeline. Ignored by default. Build the SFU with Go 1.24 in the builder
image (native Go 1.26.1 on Windows has golang/go#77975) and run it with the
LAN address:

```bash
# mello-sfu, from Git Bash
MSYS_NO_PATHCONV=1 docker run --rm -v "$(cygpath -w "$PWD"):/src:ro" -v "C:\\tmp\\sfu:/out" \
  mello-sfu-builder:latest sh -c 'cp -r /src /tmp/b && cd /tmp/b && go build -tags openh264 -o /out/sfu ./cmd/sfu'
MSYS_NO_PATHCONV=1 docker run -d --name mello-sfu-stage6 -p 8443:8443 -p 8080:8080 -p 10000:10000/udp \
  -e SFU_PUBLIC_IP=<lan-ip> -e SFU_ADMIN_PASSWORD=devpass -v "C:\\tmp\\sfu:/opt/sfu" \
  mello-sfu-builder:latest /opt/sfu/sfu
# mello
MELLO_TEST_SFU=ws://127.0.0.1:8443/ws MELLO_TEST_SFU_PASSWORD=devpass \
MELLO_TEST_SFU_HEALTH=http://127.0.0.1:8080/health MELLO_TEST_SFU_CONTAINER=mello-sfu-stage6 \
MELLO_TEST_FAULT=kill MELLO_TEST_OUTAGE_MS=3000 \
  cargo test -p mello-core --lib -- --ignored --nocapture sfu_reconnect_live
```

`MELLO_TEST_FAULT=pause` freezes the container instead: the link goes silent,
as with a pulled cable. `MELLO_TEST_ICE` (comma-separated URLs) gives the
client's voice peer ICE servers; the log line `ICE selected pair` shows the
pair that carries the call. The measured table is in spec 10 section 7.6.

### Member volume

Unit tests run in `cargo test --workspace`:

| What | Where |
|---|---|
| Percent to gain, the local mute (gain 0.0, the volume stays), the 0..200 % clamp, no entry at the default, the value sent again when a member appears again, the table survives a leave | `client/src/member_audio.rs` |
| The popover: 70 %, mute, unmute back at 70 %, reset to 100 %, Escape; your own row has no control; the popover closes when its member leaves; the value goes to libmello again when the member comes back and after a rejoin | `client/src/flow_tests.rs` |
| The e2e state port reports `volume` and `local_muted` for each voice member | `client/src/e2e_state.rs` (`e2e` feature) |
| libmello stores and clamps the gain | `mello-core/src/client/voice.rs` (`peer_volume_command_reaches_libmello`) |

### Stream membership and ICE-TCP (stage 6b)

Unit tests run in `cargo test --workspace` and in libmello `ctest`:

| What | Where |
|---|---|
| The viewer's member list: the join snapshot, one entry per user, one host | `mello-core/src/stream/members.rs` |
| A viewer that joined before the host learns of the host and of a later viewer from a fake SFU; an SFU that sends viewers no events leaves the snapshot | `mello-core/src/client/streaming.rs` (`stream_members_tests`) |
| The e2e state port reports the list and clears it when watching stops | `client/src/e2e_state.rs` (`e2e` feature) |
| Every peer role gathers a `tcptype active` candidate | `libmello/tests/test_peer_rtp_roles.cpp` |
| Given a passive TCP candidate, the client connects to it and sends a STUN Binding request in RFC 4571 framing | `mello-core/src/client/sfu_voice_join.rs` |

`stream_members_live` runs the membership case against a real SFU: a viewer
joins, then the host, then a second viewer that leaves. It needs the
mello-sfu contract where viewers get `member_joined` and `member_left`; an
older SFU fails it with an empty list for the first viewer. Ignored by
default:

```bash
MELLO_TEST_SFU=ws://127.0.0.1:8443/ws MELLO_TEST_SFU_PASSWORD=devpass \
  cargo test -p mello-core --lib -- --ignored --nocapture stream_members_live
```

To force the TCP path against an SFU with passive ICE-TCP, publish only the
TCP port of the media mux: `-p 10000:10000/tcp`, no `/udp`. Then run
`slots_live` as above. The limits of the TCP path are in spec 10 section 7.6.

`region_ranking_live` (`transport/sfu_region.rs`) measures real regions:

```bash
MELLO_TEST_REGIONS=eu-west=https://sfu-eu.m3llo.app:8080/health,us-east=https://sfu-us.m3llo.app:8080/health \
  cargo test -p mello-core --lib -- --ignored --nocapture region_ranking_live
```

---

## Backend — Nakama Go modules

Unit tests (no server needed) live next to the code in
`backend/nakama/data/modules/`:

```bash
cd backend/nakama/data/modules
go vet ./...
go test ./...
```

> The modules are a Nakama **plugin**, not a standalone binary — `go build` of
> `main` will fail; use `go vet` / `go test` for static + unit checks.

Covered: idempotent voice-join, `cleanupVoiceOnCrewExit` guards, voice-update
push-drop scheduling, reconcile-oracle helpers.

### Local full stack

For anything touching live RPCs/hooks, bring up the Docker stack:

```bash
cd backend
docker-compose up
```

### `dev_fault` RPC (fault injection)

`DevFaultRPC` deterministically injects voice-state drift so the reconcile
oracle / GC / client-resync paths can be exercised against a live server. It is
**gated** — it mutates real voice state and can evict real users:

```bash
# Enable on the Nakama instance:
MELLO_ENABLE_DEV_FAULT=1
```

Actions (payload `{"action": ...}`):

- `ghost_member` — inject a member Nakama thinks is present but the SFU isn't
  (backdated so the reconcile oracle prunes it next tick). Requires
  `channel_id`, `crew_id`, `user_id`.
- `force_leave` — simulate a missed leave for `user_id`.
- `clear_channel` — evict every member of `channel_id`.
- `drop_next_push` — drop the next `count` `voice_update` pushes for `crew_id`
  (simulates lost notifications → client must self-heal).

Never enable in production.

---

## SFU — `mello-sfu`

### Unit tests

```bash
cd mello-sfu
gofmt -l .          # formatting (should print nothing)
go test ./...
```

### Run the SFU locally

```bash
cd mello-sfu
SFU_ADMIN_PASSWORD=devpass go run ./cmd/sfu
# health:  http://localhost:8080/health
# admin:   http://localhost:8080/admin
```

### `voice-soak` — voice load / churn / resilience

Drives synthetic clients through connect→join→(WebRTC)→hold→leave against a live
SFU in test mode (`SFU_ADMIN_PASSWORD` must be set). Exit code is non-zero when
gates fail, so it's CI/script friendly.

Smoke / churn:

```bash
SFU_ADMIN_PASSWORD=devpass \
go run ./tools/voice-soak \
  --endpoint ws://127.0.0.1:8443/ws --admin-password devpass \
  --clients 30 --rounds 40 --hold-ms 400
```

Full WebRTC lifecycle + RTP load: add `--negotiate-webrtc --rtp-audio`. The RTP
is real Opus speech with real timestamps; `--vad-gaps` adds talk spurts and the
in-band speaking messages. Each client offers 16 receive slots
(`--recv-slots`); `--max-offers-within-slots 0` fails the run when an offer
reaches a client that the slots still cover (stage 4 gate). On Windows, start
the SFU with `SFU_PUBLIC_IP` set to the LAN address: Pion clients do not use
loopback candidates.
Capacity sweep / auto-search: `--sweep-clients 25,50,75,100` or
`--auto-capacity-search`. (Full flag list in `mello-sfu/README.md`.)

Resilience flags added for the robustness work:

- `--reuse-user-id` — stable SFU identity per client across rounds, exercising
  the **same-user reconnect / eviction** path.
- `--half-open` (+ `--half-open-hold-ms`) — after join, freeze the client (stop
  reading WS, keep the PeerConnection alive) to simulate a **sleep/wake zombie**.
- `--idle-resume-ms` — idle (no RTP) for a while after connect, then resume
  sending RTP (requires `--negotiate-webrtc`).

### `stream-soak` — SFU RTP stream relay / impairment matrix

Synthetic **H.264 RTP** host + N viewers against a live SFU (`transport=rtp` in output). Negotiates WebRTC (offer/answer + ICE + control DC + RTP video track). Profiles: `goodNetwork`, `typicalHome`, `roughHome`.

```bash
SFU_ADMIN_PASSWORD=devpass \
go run ./tools/stream-soak \
  --endpoint ws://127.0.0.1:8443/ws \
  --admin-password devpass \
  --viewers 12 \
  --hold-ms 15000 \
  --packets-per-frame 3 \
  --profile-matrix goodNetwork,typicalHome
```

Single custom scenario (no matrix):

```bash
SFU_ADMIN_PASSWORD=devpass \
go run ./tools/stream-soak \
  --endpoint ws://127.0.0.1:8443/ws \
  --admin-password devpass \
  --viewers 12 \
  --profile-matrix "" \
  --sim-drop-pct 0.02 \
  --sim-jitter-ms 12 \
  --min-rx-ratio 0.90 \
  --max-p95-ms 220
```

1080-style gate campaign: `./tools/stream-soak/run-1080-gate.sh` (see `mello-sfu/README.md`). Output includes `admin_rtp_*` counters for fan-out verification.

### Stream probes (`stream-host` + `sfu-stream-viewer-probe`)

End-to-end client RTP path through Nakama + SFU. Requires env: `MELLO_NAKAMA_HTTP_BASE`, `MELLO_NAKAMA_AUTH_TOKEN`, `MELLO_CREW_ID`.

Host (SFU RTP via Nakama `start_stream` — default path):

```powershell
./scripts/run-stream-host.ps1 -CrewId "<crew-id>"
# logs: host_probe_tick ... tx_aus_sent tx_rtp_packets tx_remb
```

Viewer (auto-detects session from host log):

```powershell
./scripts/run-stream-viewer.ps1
# visual: minifb window (default)
./scripts/run-stream-viewer.ps1 -NativeMetrics $true
# headless: viewer_probe_tick + viewer_probe_native_rtp lines only
```

Merge timelines:

```bash
python scripts/coalesce_stream_timeline.py \
  --session auto \
  --host-log C:\temp\host.log \
  --viewer-log C:\temp\viewer.log
```

Run automated Slice 9 gates with `.\scripts\run-stream-certification.ps1` (requires local Nakama + SFU, Heaven benchmark visible for LAN visual). Set `SFU_ADMIN_PASSWORD` when the SFU admin API is protected. Loss/WAN and product-client gates remain manual.

---

## Client — `voice-test-client`

Lives at `mello/tools/voice-test-client/`. Two modes share one `mello-core`
path.

### GUI mode (DSP A/B)

```bash
cd tools/voice-test-client
cargo run                       # dev backend
VOICE_TEST_PRODUCTION=1 cargo run   # prod backend (also set NAKAMA_SERVER_KEY / NAKAMA_HTTP_KEY)
```

Login → select crew/channel → join voice → inject WAV → switch NS mode → read
live telemetry. WAV files must be **mono / 48 kHz / 16-bit PCM**
(`bash scripts/fetch_dataset.sh` bootstraps `test-data/`). See the tool's
`README.md` for the A/B protocol and MOS template.

### Headless scenario mode (reconnect/resync E2E)

Drives a real client (no UI) through a JSON scenario and asserts events — this is
the end-to-end net for the sleep/wake + reconnect + resync work.

```bash
cd tools/voice-test-client
cargo run -- --scenario scenarios/smoke.json
# or: VOICE_TEST_SCENARIO=scenarios/reconnect.json cargo run
```

Scenario steps: `device_auth`, `login`, `select_crew`, `join_voice`,
`leave_voice`, `inject_wav`, `stop_inject`, `set_mute`, `sleep`, `fault`,
`expect_event`, `assert_no_event`. `${ENV}` tokens in the JSON are expanded from
the environment, so one file parameterises per run/CI.

Fault kinds (compiled in via the `test-faults` feature, which this tool enables —
they are **never** in production/FFI builds):

- `nakama_disconnect` — force the realtime WS down → reconnect path.
- `sfu_disconnect` — force the voice session disconnected → voice reconnect.
- `simulate_suspend` — backdate the liveness clock → wake-gap detection.

Bundled scenarios in `scenarios/`:

- **`smoke.json`** — `device_auth` → expect `DeviceAuthed`. Turnkey: proves the
  binary builds (with `test-faults`), reaches the backend, and authenticates. No
  crew/channel/WAV fixtures needed. Good first gate for an integration job.
- **`reconnect.json`** — login → join voice → inject audio → `nakama_disconnect`
  → expect `ConnectionStateChanged` (loss detected) → expect a second
  `VoiceJoined` (resync re-joined voice = **recovery proven**) plus stability
  windows (`assert_no_event VoiceSfuDisconnected`) before/after fault injection
  → leave.
  Parameterised via env:

  | Var | Meaning |
  |---|---|
  | `VOICE_TEST_EMAIL` / `VOICE_TEST_PASSWORD` | seeded test account |
  | `VOICE_TEST_CREW_ID` | crew the account belongs to |
  | `VOICE_TEST_CHANNEL_ID` | a voice channel in that crew |
  | `VOICE_TEST_WAV` | path to a mono/48k/16-bit WAV |

  Backend host via `NAKAMA_HOST` / `NAKAMA_PORT` / `NAKAMA_SSL`
  (`VOICE_TEST_PRODUCTION=1` for the prod profile).

The process exits `0` on `PASS`, non-zero on `FAIL` (first unmet `expect_event`),
so scenarios gate a CI job directly.

### External local gate (`../scripts/voice`)

For end-to-end local stack validation across `mello` + `mello-sfu`, use the
workspace-level harness:

```bash
../scripts/voice/voice-local-up.sh
unset CARGO_TARGET_DIR
../scripts/voice/voice-local-gate.sh --skip-up --skip-fixtures
```

Key guards baked into the gate:

- Fails on SFU liveness regression markers (`liveness_timeout`, repeated unhealthy checks).
- Collects per-step artifacts (`command.log`, Nakama/SFU logs, health/overview API snapshots).

Desktop + mobile LAN testing:

- `voice-local-up.sh` now auto-sets `SFU_ENDPOINT_EU=ws://<lan-ip>:8443/ws`.
- It also sets `SFU_PUBLIC_IP=<lan-ip>` for ICE host candidates.
- Verify startup output is non-loopback before joining from mobile.

---

## Browser — `sfu-test.html`

A manual voice/stream console in `mello-site/`. Serve the static site and open
the page:

```bash
cd mello-site
npm run dev        # wrangler pages dev on :8788
# open http://localhost:8788/sfu-test.html
```

Connect to an SFU endpoint with a token, then drive voice/stream by hand and
watch live WebRTC stats. The **Robustness Tools** card adds:

- a reconnect loop (repeated disconnect→reconnect with latency measurement),
- a one-shot half-open trigger (drop the WS, keep the PeerConnection),
- threshold alerts (max RTT / jitter / loss) logged when stats exceed limits,
- log export.

Use it for quick interactive repros from a real browser when a soak run flags
something or you want eyes-on stats.

---

## Client — `perf-harness`

Headless CPU/RSS regression harness (spec: `specs/20-PERF-HARNESS.md`).

```bash
# idle scenario only (no backend):
cd mello && CI=true cargo run --release -p perf-harness -- run \
  --scenario-dir tools/perf-harness/scenarios \
  --output /tmp/perf-report.json

# full gate + artifacts (voice scenarios need Nakama + fixtures):
./scripts/perf/run.sh

# capture baseline after intentional perf work:
./scripts/perf/run.sh --write-baseline
```

Voice scenarios use the same env vars as `voice-test-client` headless
(`PERF_TEST_EMAIL`, `PERF_TEST_PASSWORD`, `PERF_TEST_CREW_ID`,
`PERF_TEST_CHANNEL_ID`, `PERF_TEST_WAV`). Use **`client-dev-release.sh`**
(not `client-dev.sh`) when measuring the full Slint client manually.

For AI-driven UI exploration in dev, `client-dev.sh` enables Slint 1.17 MCP
(`http://localhost:8765/mcp`) — separate from regression baselines.

---

## When to run what

- **Every change / before pushing:** `./scripts/check.sh`. If you touched
  `libmello/` or `backend/`: `./scripts/check-full.sh`.
- **After adding tests:** `./scripts/mutation-check.sh`, to confirm the suite
  still catches deliberate breakage.
- **UI behaviour changes:** add a journey test in `client/src/flow_tests.rs`
  using the harness, and verify it fails without your fix.
- **Voice state / reconnect / presence logic:** add/extend a `reconnect.rs` unit
  test, then run `scenarios/reconnect.json` against the local stack with a
  `dev_fault` (`drop_next_push` / `ghost_member`) to prove self-healing.
- **SFU capacity / churn / sleep-wake zombies:** `voice-soak` (+ `--half-open`,
  `--reuse-user-id`, `--idle-resume-ms`).
- **Stream relay quality:** `stream-soak` RTP profiles / 1080 gate; client probes for visual + `host_probe_tick` / `viewer_probe_native_rtp` logs.
- **Audio DSP quality (A/B, NS modes, MOS):** `voice-test-client` GUI.
- **Any change to the voice audio path (libmello audio, jitter, codec, mix):**
  `./scripts/voice-gate.sh`, and report its delta against baseline 2.9-slot.
- **Client CPU/RSS regression (after perf-sensitive changes):** `perf-harness` /
  `./scripts/perf/run.sh`.
- **Interactive browser repro / live stats:** `sfu-test.html`.

## CI lanes

1. **PR (fast, no backend) — exists:** Rust fmt/clippy/test plus libmello
   `ctest` (self-hosted Windows + macOS, after the Rust build so vcpkg is
   already bootstrapped) and Nakama `gofmt`/`go vet`/`go test` on a
   GitHub-hosted `ubuntu-latest` runner. SFU `go test` lives in the `mello-sfu`
   repo and is still unwired here.
2. **Integration (on demand / nightly):** `docker-compose up` the backend +
   `go run ./cmd/sfu`, seed a test account/crew/channel, then run
   `scenarios/smoke.json` (must pass) and `scenarios/reconnect.json`, plus a
   short `voice-soak` churn gate. These need live services and are too heavy/
   flaky for the per-PR lane.
