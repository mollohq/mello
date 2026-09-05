---
name: Echo cancellation — Windows handoff
overview: "Actionable Windows checklist for the echo branch: rebuild and verify, WASAPI latency, delay-hint sign check, manual echo matrix. macOS work (harness, v2.1, VPIO duplex, crash fixes) is landed and committed; specs match the code."
isProject: false
---

# Windows Handoff — Echo Cancellation Improvements

Branch: `feat/echo-cancellation-improvements` (includes `origin/main` through the graphite-ui merge).
Specs `03-LIBMELLO.md` §4/§9 and `10-AUDIO_PIPELINE.md` §3/§4/§5.2/§6.2 describe the landed architecture.

## Landed on macOS (do not redo)

1. **ERLE harness** (`libmello/tests/test_echo_canceller.cpp`): broadband loopback (frame-aligned + misaligned-delay variants), blind-passthrough check, delay-clamp check, render-accumulator check. Baselines on v2.1/macOS arm64: aligned 23.74 dB, misaligned 22.72 dB.
2. **Engine v1.3 → v2.1 (M131)**: submodule pin `846fe90`, regenerated CMake wrapper, `absl::numeric`, Config deltas, ref-counted APM handle, inert transient toggle, nullability compat shim (`compat/absl/base/nullability.h`, MSVC-safe).
3. **Delay hints**: `set_stream_delay_ms` plumbing + `refresh_stream_delay_hint()` on init and every device/backend switch. Formula sums both legs; sign still unvalidated (task 3).
4. **VPIO duplex (macOS)**: one VoiceProcessingIO unit for capture+playback, toggle-selected with plain-HAL fallback, software APM skipped on the duplex path. Field-proven on clips; input-only VPIO provably never initializes.
5. **Crash fixes**: 8192-frame capture buffers + drop-and-log guard (VPIO delivers 960-frame slices against a reported 512 max — ASan-proven heap overflow), `AudioUnitUninitialize` before dispose, CoreAudio setup/teardown lock, absl link fix in `mello-sys/build.rs` (Unix `lib` prefix).

No public C API changes. No new dependencies.

## Windows tasks (in order)

### 1. Rebuild and run the audio suite

```powershell
cmake -B libmello/build -S libmello -DMELLO_BUILD_TESTS=ON `
  -DCMAKE_TOOLCHAIN_FILE="$PWD/external/vcpkg/scripts/buildsystems/vcpkg.cmake" `
  -DVCPKG_TARGET_TRIPLET=x64-windows-static-md
cmake --build libmello/build --target mello_tests
$env:CI='true'; ctest --test-dir libmello/build --output-on-failure
```

- Expect all `EchoCancellerTest.*` plus `VpioDuplex.*` (skips without hardware) green.
- Record both `[ERLE]` lines. Expect aligned ~23-24 dB, misaligned ~22-23 dB. Below ~15 dB aligned implicates the WASAPI path, not the engine.
- MSVC watch items: nullability shim (no `include_next`, C++17), regenerated wrapper lists (SIMD object libs carry `/arch:AVX2` etc. as before), `BEFORE PRIVATE` compat includes.
- Known pre-existing failure (also on macOS, untouched files): `RtpVideoSenderFecTest.ParityFecRepairsOneLossPerGroupWithoutPli`. Confirm on clean `main` before investigating.

### 2. Implement WASAPI latency overrides

- Files: `libmello/src/audio/capture_wasapi.cpp` (`input_latency_ms()`), `libmello/src/audio/playback_wasapi.cpp` (`output_latency_ms()`). Base defaults are 0, so this is purely additive.
- Source: `IAudioClient::GetStreamLatency()` plus `GetDevicePeriod()`. Convert 100-ns units to ms, add the device period, clamp 0..500. Mirror `query_input_latency_ms()` / `query_output_latency_ms()` in the CoreAudio backends for shape (cache at init, never fail init on query error — return 0 and log).
- Verify: the `stream delay hint:` INFO line on a real device switch should show nonzero out/in values.

### 3. Validate the delay-hint sign

- Current formula in `refresh_stream_delay_hint()` sums both legs. The original plan doc says `output − input + jitter`, which looks wrong (both legs add delay).
- Validation: force the hint to 0 vs the computed value vs a deliberately wrong value (e.g. +100 ms) and compare harness ERLE / convergence. Fix the formula and its comment to match the measurement.

### 4. Mutation-check the harness (TESTING.md rule)

- Temporarily stub out `process_render` (early return).
- Confirm `BroadbandLoopbackCancelsEcho` goes RED and `BlindRunStaysPassthrough` stays green.
- Restore with `touch` (not `cp`/`mv`, per TESTING.md mtime note).

### 5. Manual echo matrix (Windows hardware)

| Output device | Scenario | Pass criterion |
|---|---|---|
| Laptop speakers | Remote peer talks; you stay silent | Far side hears no self-echo |
| Laptop speakers | Clip plays in VC | Far side hears no clip |
| Bluetooth (AirPods-class) | Both scenarios | Same, incl. mid-session connect/disconnect |
| Headset | Double-talk over loud clip | Your speech transmits naturally (full duplex intact) |
| Any device | Voice latency | Median mouth-to-ear < 50 ms, no sustained-underrun warnings |

### 6. Merge gate

- `check-full.sh` green (adds libmello ctest + Nakama Go modules).
- iOS build still verifies on a Mac with the iOS toolchain (submodule move dropped the Apple packaging scripts; CMake never used them).

## Explicitly not Windows work

- VPIO backend is macOS-only.
- Neural residual suppression continues on branch `feat/neural-echo-suppression` (DTLN-AEC). Its Windows CPU bench (≤10 ms inference per 20 ms frame on min-spec CPU, ≤20 MB RAM, ≤6 MB installer) is tracked there.
