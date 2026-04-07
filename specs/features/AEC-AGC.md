# WebRTC AEC3 + AGC2 Integration Spec

> **Component:** libmello (Audio Pipeline)  
> **Status:** Planned  
> **Parent:** [03-LIBMELLO.md](../03-LIBMELLO.md)

---

## 1. Dependency: Vendored WebRTC Audio Processing

Use [helloooideeeeea/webrtc-audio-processing](https://github.com/helloooideeeeea/webrtc-audio-processing) -- a fork of freedesktop's self-contained WebRTC APM extraction with added mobile cross-compile support (iOS/Android). BSD-3 license. No Google depot_tools/fetch/gn required.

**Source structure:**
- `webrtc/modules/audio_processing/` -- APM core including **aec3/** and **agc2/**
- `webrtc/common_audio/` -- shared audio utilities (resampler, FFT, etc.)
- `webrtc/rtc_base/` -- base utilities (logging, checks, platform abstractions)
- `webrtc/system_wrappers/` -- CPU features, denormal disabling
- `webrtc/third_party/` -- pffft, rnnoise (unused by us)
- `subprojects/abseil-cpp` -- bundled abseil dependency

**Build approach:** The upstream uses Meson. We **translate the `meson.build` source file lists into a `CMakeLists.txt`** that lives at `libmello/third_party/webrtc-audio-processing/CMakeLists.txt`. This is the same pattern used for rnnoise in this codebase (vendor source, custom static lib build). One-time mechanical translation; source files are pegged to a WebRTC snapshot and don't change.

- Add as a git submodule at `libmello/third_party/webrtc-audio-processing/`
- Pin to a specific commit for reproducible builds
- Transitive dependency: `abseil-cpp` (installed via vcpkg, already used by the project)
- Binary size impact: ~1.2 MB (well within the <5 MB libmello budget)
- Platform defines: `-DWEBRTC_WIN -DNOMINMAX -D_USE_MATH_DEFINES` (Windows), `-DWEBRTC_MAC` (macOS), `-DWEBRTC_POSIX` (POSIX), `-DWEBRTC_ENABLE_AVX2` (x86), `-DWEBRTC_HAS_NEON` (ARM64)
- Future: the repo already has cross-compile `.ini` files for iOS (device + simulator) and Android (aarch64/armv7a/x86_64) which aligns with the post-beta mobile roadmap

---

## 2. New C++ Class: `EchoCanceller`

New files: `libmello/src/audio/echo_canceller.hpp` and `echo_canceller.cpp`.

Wraps `webrtc::AudioProcessing` with this configuration:
- **AEC3**: enabled (default ON, user-toggleable)
- **AGC2**: adaptive digital mode, enabled (default ON, user-toggleable)
- **NS**: disabled (we use RNNoise)
- **Pre-amplifier / HPF**: disabled

Key API:

```cpp
namespace mello::audio {
class EchoCanceller {
public:
    bool initialize(int sample_rate, int channels);
    void shutdown();

    // Called from capture thread -- processes near-end (mic) signal in-place
    void process_capture(int16_t* samples, int count);

    // Called from playback thread -- feeds far-end (speaker) reference
    void process_render(const int16_t* samples, int count);

    void set_aec_enabled(bool enabled);
    void set_agc_enabled(bool enabled);
    bool aec_enabled() const;
    bool agc_enabled() const;
};
}
```

Internally, `process_capture` calls `apm->ProcessStream()` and `process_render` calls `apm->ProcessReverseStream()`. WebRTC APM is designed for exactly this cross-thread usage pattern and handles delay estimation internally.

Both methods must handle frame-size adaptation: APM processes 10ms frames (480 samples at 48kHz), while `AudioPipeline` works in 20ms frames (960 samples). The wrapper splits each 960-sample call into two 480-sample APM calls.

---

## 3. Pipeline Integration

### Processing order

Current: `WASAPI Capture -> VAD -> RNNoise -> Opus`

New: `WASAPI Capture -> AEC3+AGC2 -> RNNoise -> Silero VAD -> Opus`

AEC runs first on the raw mic signal (required for proper echo correlation), AGC normalizes levels, then RNNoise denoises. VAD runs last on the cleanest signal.

### Far-end reference tap

In `AudioPipeline::mix_output()` (called from the playback thread), after mixing all peer buffers, pass the mixed output to `echo_canceller_.process_render()`. This gives the AEC the speaker signal it needs.

```
mix_output():
    sum peer buffers -> mixed signal
    echo_canceller_.process_render(mixed, count)   // <-- new
    return mixed to WASAPI playback
```

### Capture path

In `AudioPipeline::on_captured_audio()`, insert `echo_canceller_.process_capture()` before VAD and RNNoise:

```
on_captured_audio():
    accumulate to FRAME_SIZE
    compute input_level
    if not muted:
        echo_canceller_.process_capture(frame, FRAME_SIZE)  // <-- new
        vad_.feed(frame, FRAME_SIZE)
        noise_suppressor_.process(frame, FRAME_SIZE)
        opus encode
```

### AudioPipeline changes

In `audio_pipeline.hpp`:
- Add `#include "echo_canceller.hpp"` and member `EchoCanceller echo_canceller_`
- Add public methods: `set_echo_cancellation(bool)`, `set_agc(bool)`

In `audio_pipeline.cpp`:
- Initialize `echo_canceller_` in `initialize()` (after playback init, before setting render source)
- Shutdown in `shutdown()`
- Call `process_render` in `mix_output`
- Call `process_capture` in `on_captured_audio`

### Playback device switch handling

In `set_playback_device()`, the playback is stopped and recreated. The render source callback is re-set, which re-wires `mix_output` -- no special AEC handling needed since `process_render` is called inside `mix_output`.

---

## 4. C API Surface

Add to `mello.h`:

```c
MELLO_API void mello_voice_set_echo_cancellation(MelloContext* ctx, bool enabled);
MELLO_API void mello_voice_set_agc(MelloContext* ctx, bool enabled);
```

Implement in `mello.cpp`, delegating to `ctx->audio().set_echo_cancellation()` / `set_agc()`.

Extend `MelloDebugStats` with two new fields for diagnostics:

```c
bool  echo_cancellation_enabled;
bool  agc_enabled;
```

---

## 5. Rust FFI Wiring

### mello-sys

`mello-sys/build.rs` uses bindgen on `mello.h`, so the new functions will be auto-generated.

### mello-core

Add `Command` variants in `command.rs`:
- `SetEchoCancellation(bool)`
- `SetAgc(bool)` (if separate AGC toggle is desired in the UI)

In the voice module, when handling these commands, call the new FFI functions.

### Client settings callback

In `callbacks/settings.rs`, the `on_setting_changed_echo_cancellation` callback currently only saves the setting. It must also send the `SetEchoCancellation` command to mello-core.

The `noise_suppression` setting has the same gap today (saves but doesn't send to libmello). This should be fixed at the same time for consistency.

---

## 6. CMake Integration

### New file: `libmello/third_party/webrtc-audio-processing/CMakeLists.txt`

A custom CMakeLists.txt that:
- Collects source files from `webrtc/modules/audio_processing/`, `webrtc/common_audio/`, `webrtc/rtc_base/`, `webrtc/system_wrappers/` (translated from the repo's `meson.build` files)
- Sets platform-specific defines (`WEBRTC_WIN`, `WEBRTC_POSIX`, `WEBRTC_MAC`, arch flags)
- Uses `abseil-cpp` from vcpkg via `find_package(absl CONFIG REQUIRED)`
- Produces a static library target: `webrtc_audio_processing`

### Changes to `libmello/CMakeLists.txt`:

```cmake
# WebRTC audio processing (AEC3 + AGC2) -- vendored source, custom CMake build
add_subdirectory(third_party/webrtc-audio-processing)

# Add echo_canceller.cpp to MELLO_SOURCES
list(APPEND MELLO_SOURCES src/audio/echo_canceller.cpp)

# Link
target_link_libraries(mello PUBLIC webrtc_audio_processing)
```

---

## 7. Spec Updates

Update these specs to reflect the change from Speex to WebRTC:
- `00-ARCHITECTURE.md`: Tech stack table row, voice flow diagram
- `03-LIBMELLO.md`: Audio pipeline diagram, project structure (add echo_canceller files), third_party list
- `README.md`: Voice pipeline description

---

## 8. Testing

- Unit test: `libmello/tests/test_echo_canceller.cpp` -- init/shutdown, process empty frames, enable/disable toggle
- Integration: verify AEC doesn't regress audio quality when no echo is present (passthrough behavior)
- Manual: two-person voice call with one user on speakers -- echo should be suppressed

---

## 9. Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| Upstream repo is a solo maintainer fork (9 stars) | It's a thin layer over freedesktop's extraction; the actual WebRTC source is Google's. Pin to a commit. If maintainer disappears, we can vendor from freedesktop directly. |
| CMake translation from Meson is manual work | One-time effort; source files are static (pegged to a WebRTC snapshot). The Meson files clearly list every source. |
| `abseil-cpp` version conflict with other deps | Installed via vcpkg alongside other deps; single version across the project. |
| APM adds latency | AEC3 adds ~10ms algorithmic delay; within the <50ms voice latency budget. |
| Binary size increase | ~1.2 MB; total libmello stays well under 5 MB. |
| MSVC build not officially tested by upstream | The Meson build already has `-DWEBRTC_WIN` support and the freedesktop source compiles on Windows. Our CMake translation gives us full control. |
