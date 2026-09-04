---
name: Echo cancellation — Windows handoff
overview: "macOS work is done on branch echo-cancellation-improvements (harness, render-accumulator fix, delay-hint plumbing, CoreAudio latency). This file lists what must be done on a Windows machine: rebuild, WASAPI latency, sign validation, v2.x upgrade, and the manual echo matrix."
isProject: false
---

# Windows Handoff — Echo Cancellation Improvements

Branch: `echo-cancellation-improvements` (cut from `main` @ `22aa1e6`).
Status: macOS work done, **uncommitted**. Commit or stash before switching machines.

## What landed on macOS (do not redo)

1. **ERLE harness** — `libmello/tests/test_echo_canceller.cpp`, 4 new tests:
   - `BroadbandLoopbackCancelsEcho` (white-noise loopback, 1-frame delay,
     AGC2 off, threshold >10 dB; measured **24.33 dB** on v1.3 / macOS arm64).
   - `BlindRunStaysPassthrough` (no render feed, must stay within +/-3 dB).
   - `StreamDelayHintClamped` (setter clamp 0..500 ms).
   - `RenderAccumulatesSubFrameChunks` (48x100-sample feeds must yield
     exactly 10 APM frames; fails on the old drop-the-tail code).
2. **Render-tail fix** — `echo_canceller.cpp::process_render` now accumulates
   into `render_pending_` instead of dropping sub-480 tails. Capture path
   logs a warning if `count % 480 != 0` (never happens: pipeline frames 960).
3. **Delay-hint plumbing** — `EchoCanceller::set_stream_delay_ms` (clamped,
   forwarded to APM) + `AudioPipeline::refresh_stream_delay_hint()` called
   on init and on both device switches. Formula is a **sum**:
   `out_latency + in_latency + jitter_ms`. Sign is UNVALIDATED (see TODO 3).
4. **CoreAudio latency** — `capture_coreaudio` / `playback_coreaudio` report
   unit latency + safety offset + buffer frames, cached at init.
   Base interfaces (`audio_capture.hpp`, `audio_playback.hpp`) default to 0.
5. **Neural insertion point** — comment in `on_captured_audio` marks where
   the two-input suppressor goes and notes the gate must move to post-stage
   RMS at that time. No behavior change today.

## Windows TODOs (in order)

### 1. Rebuild and run the audio suite

```powershell
cmake -B libmello/build -S libmello -DMELLO_BUILD_TESTS=ON `
  -DCMAKE_TOOLCHAIN_FILE="$PWD/external/vcpkg/scripts/buildsystems/vcpkg.cmake" `
  -DVCPKG_TARGET_TRIPLET=x64-windows-static-md
cmake --build libmello/build --target mello_tests
$env:CI='true'; ctest --test-dir libmello/build --output-on-failure
```

- Expect all 13 `EchoCancellerTest.*` green.
- Record the `BroadbandLoopbackCancelsEcho` `[ERLE]` line on Windows hardware.
  Expect ~20-25 dB. If it is far lower (<15 dB), suspect the WASAPI path
  before blaming the engine.
- Known pre-existing failure (macOS, unrelated, audio untouched):
  `RtpVideoSenderFecTest.ParityFecRepairsOneLossPerGroupWithoutPli`.
  Confirm it fails on clean `main` too before investigating.

### 2. Implement WASAPI latency overrides

- Files: `libmello/src/audio/capture_wasapi.cpp`,
  `libmello/src/audio/playback_wasapi.cpp`.
- Override `input_latency_ms()` / `output_latency_ms()` using
  `IAudioClient::GetStreamLatency()` + `GetDevicePeriod()`.
- Convert 100-ns units to ms, add device period, clamp 0..500.
- Never fail init on query error — return 0 and log.
- Re-check the `stream delay hint:` INFO line on a real device switch.

### 3. Validate the delay-hint sign

- Current formula sums both legs. The plan doc says
  `output − input + jitter`, which looks wrong (both legs add delay).
- Validation: run the ERLE harness with the hint forced to 0 vs the
  computed value vs a deliberately wrong value (e.g. +100 ms).
- Correct sign = fastest convergence / highest ERLE. Fix the formula and
  the comment in `refresh_stream_delay_hint()` to match the measurement.

### 4. Mutation-check the harness (TESTING.md rule)

- Temporarily stub out `process_render` (early return).
- Confirm `BroadbandLoopbackCancelsEcho` goes RED and
  `BlindRunStaysPassthrough` stays green.
- Restore with `touch` (not `cp`/`mv`, per TESTING.md mtime note).

### 5. v2.x upgrade spike (blocked on TODOs 1-4)

- Replace `libmello/third_party/webrtc-audio-processing` with freedesktop
  v2.x. Regenerate the explicit file list in
  `libmello/cmake/webrtc-audio-processing/CMakeLists.txt`.
- Audit `apply_config()` in `echo_canceller.cpp`: `residual_echo_detector`
  may be gone/renamed in v2.x config.
- Watch the absl version bump in the vcpkg manifest.
- Gate: same harness ERLE >= 25 dB + `check-full.sh` green.
- Keep the int16 `ProcessStream` / `ProcessReverseStream` usage.

### 6. Windows-specific notes (not macOS work)

- VPIO backend (plan step B) is macOS-only. Nothing to do on Windows.
- Neural model bench (plan step C) needs min-spec Windows CPU numbers:
  <=10 ms inference per 20 ms frame, <=20 MB RAM, <=6 MB installer.
- Manual echo matrix must run on Windows hardware: laptop speakers
  (remote talk + clip playback), Bluetooth connect/disconnect mid-session,
  headset double-talk over loud clip, mouth-to-ear <50 ms.

## Files changed (macOS, uncommitted)

- `libmello/src/audio/echo_canceller.hpp` / `.cpp`
- `libmello/src/audio/audio_capture.hpp`, `audio_playback.hpp`
- `libmello/src/audio/audio_pipeline.hpp` / `.cpp`
- `libmello/src/audio/capture_coreaudio.hpp` / `.cpp`
- `libmello/src/audio/playback_coreaudio.hpp` / `.cpp`
- `libmello/tests/test_echo_canceller.cpp`

No spec changes yet (docs step lands after the engine upgrade).
No public C API changes. No new dependencies.
