---
name: Echo cancellation — Windows handoff
overview: "macOS work is done on branch echo-cancellation-improvements (harness, render-accumulator fix, delay-hint plumbing, CoreAudio latency). This file lists what must be done on a Windows machine: rebuild, WASAPI latency, sign validation, v2.x upgrade, and the manual echo matrix."
isProject: false
---

# Windows Handoff — Echo Cancellation Improvements

Branch: `feat/echo-cancellation-improvements`.
Status (2026-09-04, macOS arm64): harness + delay hints committed as
`9900279`, engine upgrade v2.1 committed as `da0c388`. VPIO backend is
done locally, uncommitted — operator field matrix (clip case first)
is the gate before merge.

## What landed on macOS (do not redo)

1. **ERLE harness** — `libmello/tests/test_echo_canceller.cpp`, 5 new tests:
   - `BroadbandLoopbackCancelsEcho` (white-noise loopback, 1-frame delay,
     AGC2 off, threshold >10 dB).
   - `MisalignedDelayLoopbackCancelsEcho` (24.4 ms non-integer delay,
     longer warmup, threshold >6 dB; guards the delay-estimator path).
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
6. **Engine upgrade v1.3 -> v2.1 (M131)** — submodule now pinned at v2.1
   (`846fe90`); CMake wrapper file lists regenerated from v2.1 meson
   sources (251 files, all verified present); `absl::numeric` added;
   `apply_config` drops the removed `voice_detection` /
   `residual_echo_detector` knobs; `apm_` handle is now
   `rtc::scoped_refptr` (v2.x `Create()` breaking change; no more manual
   delete); transient-suppression toggle stays API-compatible but inert
   (backend removed upstream); `compat/absl/base/nullability.h` shim
   covers the `Nullable`/`Nonnull` wrappers absl dropped (identity
   aliases, MSVC-safe, PRIVATE to WAP + mello + mello_tests targets).
   Measured ERLE on v2.1: aligned 23.74 dB, misaligned 22.72 dB
   (v1.3 aligned was 24.33 dB — the ideal harness does not discriminate
   generations; the win is under impairments, still to be field-proven).
7. **VPIO duplex backend (macOS) — IMPLEMENTED, hardware-verified
   init.** A hardware probe proved input-only VPIO never initializes
   (-10875); the duplex unit (one `VoiceProcessingIO` AudioUnit, input +
   output enabled, our mix rendered through its output bus as Apple's
   AEC reference) initializes cleanly on MacBook mic+speakers with
   explicit device selection. `VpioUnit` (refcounted start/stop,
   latency queries, contract validation) + capture/playback adapters
   are live; the toggle selects duplex vs plain HAL pair with fallback;
   software APM capture is skipped on the duplex path. `VpioDuplex`
   tests pass on hardware (init, frame flow both directions, invalid-id
   failure). REMAINING: operator field test of actual echo
   cancellation (remote voices, clips, AirPods) — init working does
   not yet prove cancellation working.
8. **VPIO teardown abort — FIXED.** Field crash on backend toggle with
   live DSP load: malloc `free_list_checksum_botch` inside Apple's
   DSPGraph teardown. ASan proved the cause: VPIO delivers 960-frame
   input slices against a reported `MaximumFramesPerSlice` of 512, so
   Apple's render wrote 1920 audio bytes past our 1024-byte capture
   buffer on every slice. Fix: 8192-frame capture buffers plus a
   drop-and-log guard on both VPIO and HAL capture paths, and
   `AudioUnitUninitialize` before dispose in VPIO teardown. Verified
   with a loud-clip toggle stress under ASan (aborts before, clean
   after). If a slice ever exceeds 8192 the log will say so.

## Windows TODOs (in order)

### 1. Rebuild and run the audio suite

```powershell
cmake -B libmello/build -S libmello -DMELLO_BUILD_TESTS=ON `
  -DCMAKE_TOOLCHAIN_FILE="$PWD/external/vcpkg/scripts/buildsystems/vcpkg.cmake" `
  -DVCPKG_TARGET_TRIPLET=x64-windows-static-md
cmake --build libmello/build --target mello_tests
$env:CI='true'; ctest --test-dir libmello/build --output-on-failure
```

- Expect all 14 `EchoCancellerTest.*` green.
- Record both `[ERLE]` lines on Windows hardware.
  Expect aligned ~23-24 dB, misaligned ~22-23 dB. If either is far lower
  (<15 dB aligned), suspect the WASAPI path before blaming the engine.
- MSVC attention: the nullability shim uses no `include_next` and is
  C++17-clean, but confirm the WAP target builds warning-clean under the
  vendored-code warning suppressions already in the wrapper.
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

### 5. Confirm the v2.1 upgrade on Windows (spike landed on macOS)

- The tree swap + wrapper regen + Config audit are done (see item 6
  above). On Windows: rebuild, run the harness, compare ERLE numbers
  with the macOS baselines (aligned 23.74, misaligned 22.72).
- Then run `check-full.sh` green before merging the branch.
- iOS note: the submodule move drops the Apple-framework packaging
  scripts (`create-lipo.sh`, cross inis) from the tree. The CMake build
  never used them, but the iOS build must still be verified on a Mac
  with the iOS toolchain before merge.

### 6. Windows-specific notes (not macOS work)

- VPIO backend (plan step B) is macOS-only. Nothing to do on Windows.
- Neural model bench (plan step C) needs min-spec Windows CPU numbers:
  <=10 ms inference per 20 ms frame, <=20 MB RAM, <=6 MB installer.
- Manual echo matrix must run on Windows hardware: laptop speakers
  (remote talk + clip playback), Bluetooth connect/disconnect mid-session,
  headset double-talk over loud clip, mouth-to-ear <50 ms.

## Files changed (macOS, `da0c388` committed; VPIO below uncommitted)

- `libmello/third_party/webrtc-audio-processing` (submodule pin f8efa84 -> v2.1 `846fe90`)
- `libmello/cmake/webrtc-audio-processing/CMakeLists.txt` (regenerated lists, `absl::numeric`, compat includes)
- `libmello/cmake/webrtc-audio-processing/compat/absl/base/nullability.h` (new)
- `libmello/CMakeLists.txt` (compat include for mello target)
- `libmello/tests/CMakeLists.txt` (compat include for mello_tests target)
- `libmello/src/audio/echo_canceller.hpp` / `.cpp` (scoped_refptr handle, Config deltas)
- `libmello/tests/test_echo_canceller.cpp` (misaligned variant, v2.1 baselines)
- `libmello/src/audio/audio_capture.hpp` (voice-processing + backend-report virtuals)
- `libmello/src/audio/capture_coreaudio.hpp` / `.cpp` (VPIO subtype, backend log)
- `libmello/src/audio/audio_pipeline.hpp` / `.cpp` (backend switch, APM skip, device-id store)

No spec changes yet (docs step lands after the engine upgrade).
No public C API changes. No new dependencies.
