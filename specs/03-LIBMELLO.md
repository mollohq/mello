# LIBMELLO Specification

> **Component:** libmello (Low-Level C++ Library)  
> **Language:** C++17  
> **Status:** Beta Scope  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

libmello is the C++ library that handles all low-level audio/video capture, encoding, decoding, and the peer transport to the SFU. It exposes a pure C API for FFI compatibility with Rust and future mobile platforms.

**Key Responsibilities:**
- Audio capture (WASAPI), processing (WebRTC AEC3 + AGC2, RNNoise, Silero VAD), and encoding (Opus)
- Video capture (the capture ladder: game capture hook, WGC, DXGI; ScreenCaptureKit on macOS), encoding (NVENC/AMF/QSV), and decoding
- Peer transport to the SFU (libdatachannel)
- ICE/STUN/TURN connectivity

---

## 2. Project Structure

```
libmello/
├── CMakeLists.txt
├── include/
│   └── mello.h                     # Public C API (single header)
│
├── src/
│   ├── mello.cpp                   # API implementation
│   ├── context.hpp                 # Internal context
│   │
│   ├── audio/
│   │   ├── audio_pipeline.hpp/cpp
│   │   ├── audio_session_win.hpp/cpp # Windows ducking prevention
│   │   ├── capture_wasapi.hpp/cpp  # Windows audio capture
│   │   ├── echo_canceller.hpp/cpp  # WebRTC AEC3 + AGC2 (APM)
│   │   ├── playback_wasapi.hpp/cpp # Windows audio playback
│   │   ├── processing.hpp/cpp      # RNNoise + Silero VAD wrapper
│   │   ├── opus_codec.hpp/cpp     # Opus encoder (voice), decoder (stream audio)
│   │   ├── voice_playout.hpp/cpp   # NetEQ per peer + AudioMixerImpl (C++20 island)
│   │   └── stream_resampler.hpp/cpp # PushSincResampler for the WASAPI edges
│   │
│   ├── video/
│   │   ├── video_pipeline.hpp/cpp
│   │   ├── capture_source.hpp      # CaptureSource interface, CaptureState
│   │   ├── capture_process.hpp/cpp # ProcessCapture: the capture ladder
│   │   ├── capture_hook.hpp/cpp    # Game capture hook backend (client side)
│   │   ├── hook_launcher.hpp/cpp   # Offsets helper and injection helper
│   │   ├── hook_policy.hpp/cpp     # Run-time checks before any injection
│   │   ├── capture_wgc.hpp/cpp     # Windows.Graphics.Capture
│   │   ├── capture_dxgi.hpp/cpp    # Desktop Duplication API
│   │   ├── process_enum.hpp/cpp    # Processes, windows, monitors
│   │   ├── encoder.hpp             # Abstract encoder interface
│   │   ├── encoder_nvenc.hpp/cpp   # NVIDIA NVENC
│   │   ├── encoder_amf.hpp/cpp     # AMD AMF
│   │   ├── encoder_qsv.hpp/cpp     # Intel Quick Sync
│   │   ├── decoder.hpp/cpp         # Hardware decoder
│   │   └── color_convert.hpp       # GPU color conversion (BGRA↔NV12)
│   │
│   ├── transport/
│   │   ├── peer_connection.hpp/cpp # libdatachannel wrapper
│   │   ├── signaling.hpp           # Signal message types
│   │   └── ice_config.hpp          # STUN/TURN configuration
│   │
│   └── util/
│       ├── logger.hpp
│       ├── thread_pool.hpp
│       └── ring_buffer.hpp
│
├── third_party/                    # Git submodules & vendored libs
│   ├── libdatachannel/
│   ├── opus/
│   ├── rnnoise/
│   ├── webrtc-audio-processing/    # Submodule: github.com/helloooideeeeea/webrtc-audio-processing (WebRTC APM)
│   └── silero-vad/                 # ONNX model + runtime
│
├── models/                         # ML model files (Silero VAD .onnx)
│
└── tests/
    ├── test_audio_pipeline.cpp
    ├── test_video_pipeline.cpp
    └── test_peer_connection.cpp
```

---

## 3. Public C API

The entire library is exposed through a single C header (`mello.h`). This is the FFI boundary used by the Rust `mello-sys` crate (via bindgen). **Changes to this header break the Rust FFI layer.**

### Opaque types

| Type | Purpose |
|------|---------|
| `MelloContext` | Library context — created once at startup |
| `MelloVoiceSession` | Active voice session |
| `MelloStreamHost` | Active stream hosting session |
| `MelloStreamView` | Active stream viewing session |
| `MelloPeerConnection` | One peer connection: the client side of an SFU session |

### API surface groups

| Group | Key functions | Notes |
|-------|---------------|-------|
| **Context** | `mello_init`, `mello_destroy`, `mello_get_error` | One context per app |
| **Voice** | `mello_voice_start_capture`, `stop_capture`, `set_mute`, `set_deafen`, `is_speaking`, `set_vad_callback`, `get_packet`, `set_packet_sink`, `feed_packet`, `feed_rtp`, `remove_peer`, `set_peer_volume`, `set_peer_pan`, `report_send_feedback`, `set_max_bitrate` | Mute stops sending but capture continues for VAD. With a packet sink set, frames go to the sink, not to `get_packet`. Received audio plays through one NetEQ per peer (spec 10 section 5) |
| **Stream Host** | `mello_stream_start_host`, `stop_host`, `get_video_packet`, `request_keyframe`, `get_stats` | Config struct controls resolution/bitrate/encoder |
| **Stream View** | `mello_stream_start_view`, `stop_view`, `feed_video_packet`, `get_frame`, `free_frame` | Caller must free frames after use |
| **Peer Transport** | `mello_peer_create`, `destroy`, `set_ice_servers`, `create_offer`, `create_answer`, `set_remote_description`, `handle_remote_offer`, `add_ice_candidate`, `send_reliable`, `send_ping`, `rtt_ms`, `pong_age_ms` | The client side of an SFU session (§6), plus control-plane liveness/RTT probes. `create_answer` stays for the in-process tests, where a libmello peer stands in for the SFU |
| **Devices** | `mello_get_audio_inputs`, `get_audio_outputs`, `set_audio_input`, `set_audio_output`, `get_encoders` | Query/switch audio devices and video encoders at runtime |

### Callbacks

- `MelloVoiceActivityCallback` — fires on speaking state change (used for UI VAD indicators)
- `MelloPacketSinkCallback` — one encoded voice frame, on the capture thread (§7, spec 10 §4.5)
- `MelloSendFeedbackCallback` — one receiver report about the voice send track, on the libdatachannel thread (§7, spec 10 §7.5)
- `MelloAudioFrameCallback` / `MelloVideoFrameCallback` — raw frame delivery
- `MelloIceCandidateCallback` — ICE trickle candidate generated
- `MelloPeerStateCallback` — peer connection state change
- `MelloPeerTransportCallback` — the peer's transport changed: a state change, or a DataChannel or the video track opened or closed (`mello_peer_set_transport_callback`). A waiter reads the `mello_peer_is_*` functions instead of polling them

### Transport liveness helpers

- `mello_peer_send_ping` sends a timestamped keepalive on the reliable/control DataChannel.
- `mello_peer_rtt_ms` exposes smoothed control-path RTT derived from pong `ts`.
- `mello_peer_pong_age_ms` exposes time since the last observed pong (`-1` if none yet).
- Pong parsing accepts both text and binary control payloads so mixed client/server stacks do not lose liveness updates silently.

### Host stream stats (`MelloStreamStats`)

`mello_stream_get_stats` reports encoder rate/bitrate/keyframes/bytes plus the
host diagnostics needed to debug a remote user's stream: `frames_captured`,
`capture_idle_ms`, `encode_queue_depth`, `encode_queue_drops`, `convert_ms`,
`encode_ms`, `encode_ms_mean`, `encode_submit_ms`, `encode_wait_ms`,
`encode_lock_ms`, `encoder_cost_tier`, `capture_backend`, `encoder_name`,
`gpu_name`.

- **Capture arrivals are counted before any encode work** (`VideoPipeline::on_captured_frame`,
  both the D3D11 and the CVPixelBuffer overload). Comparing `frames_captured`
  against `fps_actual` is the only way to tell a stalled capture from a stalled
  encoder — encode-side counters alone cannot.
- **`encode_queue_drops` is silent frame loss.** The encode queue is
  `ENCODE_QUEUE_CAP = 2` with newest-wins eviction, so an encoder that cannot
  keep up discards frames and reports no error anywhere else.
- **Encode time is split by phase.** `encode_ms` wraps submit + async completion
  wait + bitstream lock. Fused, a busy encoder block and a stalled VRAM readback
  are indistinguishable; split, they name their own cause. `encode_ms_mean` is
  an exponential mean (~1 s at 60 fps) because the per-frame figure is far too
  noisy to act on — the overload policy consumes the mean, not the last sample.
- Rates are deliberately not computed here. The struct exposes monotonic counters
  and the caller derives windows, keeping a single clock policy in one place.
- New fields are **appended**; existing offsets are unchanged. `mello-sys`
  regenerates bindings via bindgen on build.

### Error handling

All functions return `MelloResult` enum. On failure, `mello_get_error()` returns a human-readable message. Error codes: `MELLO_OK`, `MELLO_ERROR_INVALID_PARAM`, `MELLO_ERROR_NOT_INITIALIZED`, `MELLO_ERROR_CAPTURE_FAILED`, `MELLO_ERROR_ENCODE_FAILED`, `MELLO_ERROR_DECODE_FAILED`, `MELLO_ERROR_TRANSPORT_FAILED`, `MELLO_ERROR_NO_HARDWARE`.

---

## 4. Audio Pipeline

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         AUDIO PIPELINE                                  │
│                                                                         │
│  CAPTURE PATH:                                                          │
│  ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐   │
│  │ WASAPI  │──▶│AEC3+AGC2│──▶│ RMS gate│──▶│ Silero  │──▶│RNNoise+ │   │
│  │ Capture │   │  (APM)  │   │pre-roll │   │   VAD   │   │  Opus   │   │
│  └─────────┘   └─────────┘   └─────────┘   └─────────┘   └─────────┘   │
│                                                │              │         │
│                                                ▼              ▼         │
│                                          VAD Callback    Packets Out   │
│                                                                         │
│  PLAYBACK PATH:                                                         │
│  ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐   │
│  │ SFU RTP │──▶│ NetEQ   │──▶│  Mixer  │──▶│ Output  │──▶│ WASAPI  │   │
│  │ frames  │   │per peer │   │(stereo, │   │  ring   │   │ Playback│   │
│  │   In    │   │decode,  │   │ limiter)│   │ 10 ms → │   │ (sinc   │   │
│  │         │   │FEC, PLC │   │         │   │ device  │   │resample)│   │
│  └─────────┘   └─────────┘   └─────────┘   └─────────┘   └─────────┘   │
│                                  │                                      │
│                                  ▼                                      │
│                          AEC render reference                           │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### Key design decisions

- **WebRTC APM v2.1 / M131 (AEC3 + AGC2):** Runs first on the raw mic after capture (AEC needs the speaker reference from mixed playback). The vendored tree was upgraded from v1.3/M88; `residual_echo_detector` and `voice_detection` knobs are gone, the APM handle is ref-counted, and transient suppression is inert (backend removed upstream). Optional WebRTC NS and HPF remain runtime-testable.
- **macOS VPIO duplex alternative:** When the echo toggle is on, capture and playback share one VoiceProcessingIO unit whose output bus carries our mix as Apple's AEC reference. Our APM capture pass is skipped on this path to avoid double processing; RNNoise and VAD run unchanged. Toggle off selects the plain HAL pair plus software AEC. Input-only VPIO never initializes; only the combined unit works.
- **Stream-delay hints:** `AudioPipeline` feeds measured device latency plus the output ring depth into APM (`set_stream_delay_ms`) on init and every device switch, so the delay estimator starts converged. NetEQ's buffer is not in the sum: the render reference is the mixer output, after NetEQ. CoreAudio sums unit latency, safety offset, and buffer size. WASAPI sums `GetStreamLatency` and the device period. Both legs sum because the hint is render-to-capture delay. The hint clamps to 0..500 ms. A failed query degrades to 0 and never fails init. A Windows ERLE-harness run confirmed the sum against 0 and +100 ms controls.
- **Neural residual-echo suppressor (flag-off):** A 1.3 M-parameter GRU mask model (MS AEC-Challenge 2022 baseline, ONNX) runs post-AEC on the software path: 48↔16 kHz FIR resampling, 320-point DFT front-end matching training, far-end reference ring, silence bypass. Settings flag `echo_suppression`, default off; missing model degrades to passthrough. Budgets: ≤10 ms inference, ≤20 MB RAM, ≤6 MB model file (ships 5.0 MB fp32).
- **Render reference accumulator:** Playback callbacks arrive in device-sized chunks (~512 frames), not APM 10 ms chunks. The render feed accumulates to 480-sample units instead of dropping tails, which kept AEC misaligned. Capture buffers are sized defensively (8192 frames) with a drop-and-log guard: VPIO has delivered 960-frame slices against a reported max of 512.
- **Adaptive speech gate:** A cheap RMS/noise-floor gate runs before neural work. It keeps a short pre-roll and hangover windows so speech starts/ends are not clipped. The pre-roll is kept as captured, before the expander, and leaves at its captured level. Silero starts each run with a chunk that ends with the current frame, and while the gate waits for speech it also decides at the end of a frame in which no chunk completed (`VoiceActivityDetector::peek`). Spec 10 §4.1.
- **Downward expander (AGC pump control):** `SpeechExpander` runs after AGC2, before encode. A quiet mic forces AGC2 to a large gain (field: ~+23 dB), which then blasts the noise floor in speech gaps and transmits crackle. AGC config cannot separate the speech gain from the floor gain. The expander attenuates the live frame toward a floor gain when the raw Silero probability is below threshold (not `is_speaking()`, which latches through the gaps), with fast attack and smoothed release. It does not touch the pre-roll, and it starts at unity when the gate opens. AGC2 also starts at `initial_gain_db = 0` (not the M131 default +15). See spec 10 §4.4.
- **RNNoise over alternatives:** Real-time, small model (<100KB), no GPU needed, well-tested in voice comms. It remains the default quality path, but now only runs during active speech/pre-roll/hangover windows.
- **Silero VAD:** ONNX-based neural VAD with hysteresis. It confirms candidate speech windows instead of running as the only gate for every frame forever.
- **Opus at 64kbps mono:** Good quality for voice, well within the bandwidth budget of one SFU uplink. Frame size is 20ms (960 samples at 48kHz). In-band FEC and DTX on.
- **NetEQ playout (WebRTC M131):** One `NetEq` per remote peer decodes, conceals (expand, in-band FEC), plays comfort noise, and time-stretches (accelerate, preemptive expand) to follow network jitter and clock drift. It replaces the custom jitter buffer, the per-peer Opus decoders and the fill PLC. The vendored code lives in the overlay `third_party/webrtc-neteq/` and builds as `webrtc_neteq` (option `MELLO_WEBRTC_NETEQ`, ON on every platform). Spec 10 section 5.
- **Delay manager tuning:** NetEQ reads the field trial `WebRTC-Audio-NetEqDelayManagerConfig` from a mello `FieldTrialsView`. The shipped value shortens the memory of a jitter burst so the delay returns within 5 s (upstream: 26 s). `VoicePlayout::default_delay_manager_config()`.
- **AudioMixerImpl with limiter:** One mixer `Source` per peer. Volume, pan and output gain apply in the source, before the AGC2 limiter of the mixer. The mix is stereo, 10 ms frames; a small output ring adapts them to the device callback size.
- **C++20 island:** The NetEQ headers need C++20 and `WIN32_LEAN_AND_MEAN`. Only `voice_playout.cpp` and `stream_resampler.cpp` include them; they build in the object library `mello_playout`, whose objects go into libmello. Their headers have no WebRTC include, so the rest of libmello stays C++17.
- **Sinc resampling at the WASAPI edges:** `StreamResampler` wraps WebRTC `PushSincResampler` (10 ms blocks, one per channel). It replaces linear interpolation on capture (device rate to 48 kHz mono) and playback (48 kHz to the device rate). CoreAudio and VPIO run at 48 kHz natively.


### 4.1 Ducking Prevention (Windows)

Windows automatically reduces ("ducks") the volume of all other applications when it detects a communications audio session. For a gaming voice app this is a dealbreaker — it quiets or mutes the game the user is playing.

#### How Windows decides to duck

When an application opens a WASAPI stream on the `eCommunications` endpoint, the OS classifies the session as communications activity and consults Sound Settings → Communications tab. The default is "Reduce the volume of other sounds by 80%." Some users have it set to "Mute all other sounds." This applies system-wide to every other audio source.

#### Primary defense: `eConsole` endpoint role

Both `WasapiCapture` and `WasapiPlayback` use `GetDefaultAudioEndpoint` with `eConsole` instead of `eCommunications`. This prevents Windows from classifying mello's sessions as communications activity, so ducking is never triggered. This is the same approach Discord uses.

When the user selects a specific device in settings, the `GetDevice()` path is used instead, bypassing the endpoint role entirely.

#### Secondary defense: `AudioSessionWin`

`AudioSessionWin` (`audio_session_win.hpp/cpp`) provides two additional layers of protection, both Windows-only:

1. **`SetDuckingPreference(TRUE)`** on the playback session — tells Windows not to duck mello's own audio if another communications app (Skype, Teams, etc.) triggers ducking. Called after `IAudioClient::Initialize()` but before `Start()`. Only applies to render endpoints; capture endpoints do not support this API.

2. **`IAudioVolumeDuckNotification`** — registers a duck notification handler on the default multimedia endpoint's session manager. Both `OnVolumeDuckNotification` and `OnVolumeUnduckNotification` are no-ops.

`AudioSessionWin` is owned by `AudioPipeline` and wired to `WasapiPlayback` via a non-owning pointer. All integration is behind `#ifdef _WIN32` guards. macOS CoreAudio does not have equivalent ducking behavior.

#### Lifecycle

| Phase | What happens |
|-------|-------------|
| `AudioPipeline::initialize()` | Creates `AudioSessionWin`, initializes COM (STA), registers duck notification handler, then passes it to `WasapiPlayback` before playback init |
| `WasapiPlayback::initialize()` | After `IAudioClient::Initialize()` succeeds, calls `disable_ducking_for_client` to set the ducking preference |
| `AudioPipeline::set_playback_device()` | Re-wires `AudioSessionWin` to the new playback instance before init |
| `AudioPipeline::shutdown()` | Unregisters the duck notification handler |

#### COM threading

All WASAPI threads use `CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED)`. Using `COINIT_MULTITHREADED` causes `SetDuckingPreference` and `RegisterDuckNotification` to silently succeed but not actually register with the audio engine's notification pump.

### 4.2 Device Switching Invariants

When `set_playback_device()` or `set_capture_device()` replaces the `AudioPlayback` / `AudioCapture` instance, the new object starts with **no callback**. The old object's callback does not transfer. The switching function **must** re-wire the callback before calling `start()`:

| Function | Must re-wire | Same wiring as |
|----------|-------------|----------------|
| `set_playback_device()` | `request_stereo_output()` before `initialize()`, then `wire_render_source()` (the `mix_output` lambda with the backend's `render_channels()`) | `initialize()` step 9/9 |
| `set_capture_device()` | `capture_->start(...)` with the `on_captured_audio` lambda | `start_capture()` |
| `switch_audio_backend()` (macOS) | **Both** of the above: the duplex unit spans both directions, so capture and playback instances are replaced together | `initialize()` pair setup |

Without this, the WASAPI playback thread falls through to `ring_.read()` (always empty in SFU/clip mode), producing permanent silence. The capture side drops all mic data.

**Why this matters at startup:** Settings restoration calls `set_playback_device()` / `set_capture_device()` ~100ms after `AudioPipeline::initialize()` completes. If the user has a saved device preference, the freshly-wired callback is immediately destroyed and replaced with `nullptr`.

> **Incident reference (April 2026):** `set_playback_device()` created a new `AudioPlayback` without re-setting `render_source_`, causing total silence on every Windows machine that had a saved output device preference. macOS was unaffected only because no tester had set an explicit output device. See `mello/logs/windows_device_no_sound.log`.

---

## 5. Video Pipeline

### Host (Capture → Encode)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         VIDEO PIPELINE (HOST)                           │
│                                                                         │
│  ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐                 │
│  │ Capture │──▶│  Color  │──▶│ Hardware│──▶│ Packet  │                 │
│  │ ladder  │   │ Convert │   │ Encode  │   │ Queue   │                 │
│  │         │   │ (GPU)   │   │ NVENC/  │   │         │                 │
│  │ D3D11   │   │ BGRA→   │   │ AMF/QSV │   │         │                 │
│  │ Texture │   │ NV12    │   │         │   │         │                 │
│  └─────────┘   └─────────┘   └─────────┘   └─────────┘                 │
│      ▲                                          │                       │
│      │ Zero-copy in VRAM                        ▼                       │
│                                            To Viewers                   │
└─────────────────────────────────────────────────────────────────────────┘
```

### Viewer (Decode → Display)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         VIDEO PIPELINE (VIEWER)                         │
│                                                                         │
│  ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐                 │
│  │ Packets │──▶│ Hardware│──▶│  Color  │──▶│  Frame  │                 │
│  │   In    │   │ Decode  │   │ Convert │   │ Buffer  │                 │
│  │         │   │ DXVA2/  │   │ (GPU)   │   │         │                 │
│  │         │   │ NVDEC   │   │ NV12→   │   │ RGBA    │                 │
│  │         │   │         │   │ RGBA    │   │ Pixels  │                 │
│  └─────────┘   └─────────┘   └─────────┘   └─────────┘                 │
│                                                 │                       │
│                                                 ▼                       │
│                                            To UI                        │
└─────────────────────────────────────────────────────────────────────────┘
```

### Key design decisions

- **Hardware encode only:** No software fallback. NVENC/AMF/QSV are fast enough (<5ms) and don't compete for CPU with the game being streamed. If no hardware encoder is detected, streaming is unavailable.
- **Zero-copy VRAM pipeline:** Every capture backend delivers a D3D11 texture, color conversion (BGRA→NV12) happens on GPU via compute shader, and the encoder reads directly from VRAM. One exception: a Direct3D 9 game that cannot share textures sends its frames through memory, and the client uploads them.
- **Capture ladder for games:** `ProcessCapture` tries the game capture hook (only when the backend hook policy allows the game, and only after the run-time checks in `hook_policy.cpp`), then WGC window, WGC monitor, and DXGI desktop duplication. The rules, the hook, and its policy are in `12-STREAMING.md` §3.1. The capture classes are in `14-VIDEO-PIPELINE.md` §4.
- **Game capture hook:** The hook DLL, the injector and the offsets helper are a separate CMake project in `hook/`, built for x86 and x64. `hook/include/mello_hook_protocol.h` is the contract between the hook and libmello. Breaking its safety rules crashes the game, not m3llo: read `hook/README.md` before you change it.
- **DXGI Desktop Duplication:** Captures all monitors at native resolution. Handles cursor compositing, display rotation, and secure desktop transitions. Requires Windows 8+. It cannot see an exclusive-fullscreen game.
- **Abstract encoder interface:** All three hardware encoders implement the same `Encoder` base class. `create_best_encoder()` probes available hardware at runtime.

---

## 6. Transport Layer

Each peer connection is the client side of one SFU session (`EXTERNAL-SFU.md`, mello-sfu `01-SFU.md`). It uses libdatachannel. The SFU is the only media transport.

| Channel | Ordered | Reliable | Used for |
|---------|---------|----------|----------|
| `control` | Yes | Yes | Control messages (voice slot map, speaking, stream pause), ping/pong |
| `audio` (voice peers only) | No | No | No media. The SFU voice join waits until it opens |

Media goes on RTP tracks: voice (Opus: the mic m-line and the receive slots), stream video (H.264) and stream audio (Opus). Incoming audio reaches mello-core as complete RTP packets (`mello_peer_set_audio_rtp_callback`).

### ICE/NAT traversal

- STUN servers for reflexive candidates
- TURN servers as fallback relay (time-limited credentials from Nakama `get_ice_servers` RPC)
- ICE trickle: candidates go through the SFU WebSocket signaling

### Signaling flow

1. The client gets an SFU endpoint and a token from Nakama (`voice_join`, `start_stream`, `watch_stream`).
2. The client opens the SFU WebSocket and joins the session.
3. The client creates the offer and the SFU answers. A later SFU offer goes to `mello_peer_handle_remote_offer`.
4. Both sides trickle ICE candidates on the WebSocket.

---

## 7. Thread Model

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          THREAD MODEL                                   │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │   Main Thread   │  ← API calls, state management                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │  Audio Capture  │  ← WASAPI event-driven, ~20ms wakeup              │
│  │     Thread      │                                                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │ Audio Playback  │  ← WASAPI event-driven                            │
│  │     Thread      │                                                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │  Video Capture  │  ← ~16ms wakeup for 60fps                         │
│  │     Thread      │                                                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │  Video Encode   │  ← Processes captured frames                      │
│  │     Thread      │                                                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  ┌─────────────────┐                                                    │
│  │  Network I/O    │  ← libdatachannel internal threads                │
│  │   (internal)    │                                                    │
│  └─────────────────┘                                                    │
│                                                                         │
│  Thread-safe queues connect threads. Minimal locking.                  │
│  Each thread documented in source with which functions it calls.       │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### Voice packet sink (`mello_voice_set_packet_sink`)

| Question | Answer |
|---|---|
| Which thread calls the sink | The audio capture thread, right after Opus encode. In inject mode: the thread that calls `mello_voice_inject_capture`. |
| Locks held during the call | The capture lock (`accum_mutex_`) and the sink lock (`packet_sink_mutex_`). |
| What the sink may do | Return fast. Call `mello_peer_send_audio_frame` and `mello_voice_feed_packet`. Read atomics, take short leaf locks, log rarely. |
| What the sink must not do | Block or wait on the network. Call `mello_voice_set_packet_sink`, or a capture control of the same context (start, stop, inject, push-to-talk, device switch): they take the capture or sink lock and wait for themselves. |
| Set and clear | Any thread, never from inside the sink. Both wait for a sink call that runs now. After a clear returns, the caller may free `user_data`. |
| Lock order | `accum_mutex_`, then `packet_sink_mutex_`, then `outgoing_mutex_`. `feed_packet` takes the `VoicePlayout` peer map mutex, which is after `accum_mutex_` also in `stop_capture`. |

### Voice send-leg feedback and encoder adaptation

| Question | Answer |
|---|---|
| Who reads the receiver reports | `SendFeedbackHandler`, last in the voice send track's media handler chain, on the libdatachannel thread. It only reads; the messages go on unchanged. |
| Who gets them | `MelloSendFeedbackCallback` (`mello_peer_set_send_feedback_callback`), on the same thread. The peer copies the callback under its `mutex_` and calls it outside. |
| Who runs the controller | The caller of `mello_voice_report_send_feedback` (in production that libdatachannel thread), under `adapt_mutex_`. `mello_voice_set_max_bitrate` (command loop) takes the same lock. |
| Hand-off to the encoder | Two atomics (bitrate, loss perc). The capture thread reads them before each encode and calls `opus_encoder_ctl` when a value changed. |
| Lock order | `adapt_mutex_` is a leaf. The capture thread never takes it, so a report never waits for the capture lock, and an encode never waits for a report. |

### Voice receive and playout (NetEQ)

| Question | Answer |
|---|---|
| Who inserts | The network thread: the libdatachannel track thread calls `mello_voice_feed_rtp` (SFU). The command loop calls `mello_voice_feed_packet` (mic test). The capture thread calls it for the mic test in an SFU call (inside the packet sink). |
| Who mixes | The audio device thread, in the render source (`mix_output`). It calls `AudioMixerImpl::Mix`, which calls `NetEq::GetAudio` once per peer every 10 ms. On the test backend: the thread that calls `mello_voice_test_pull_output`. |
| Locks | `VoicePlayout` guards its peer map with one mutex. The device thread never takes it. `NetEqImpl` locks itself, so an insert and a `GetAudio` on the same peer may overlap. The mixer locks its source list in `Mix`, `AddSource` and `RemoveSource`. |
| Lock order | The capture lock (`accum_mutex_`), then the peer map mutex, then the mixer lock, then a NetEQ lock. The device thread takes only the mixer lock, then NetEQ locks. Nothing takes them in another order. |
| Lifetime | A peer's NetEQ is created on its first packet (on the inserting thread) and added to the mixer. Peer removal, a new SSRC, a sender restart and `clear_remote_streams` take it out of the mixer first; `RemoveSource` waits for a `Mix` that runs now. An insert that holds a reference finishes on the old instance, and the last reference frees it. `VoicePlayout` lives as long as `AudioPipeline` and is never replaced. |
| Output ring | The device thread owns it. It holds less than one 10 ms stereo frame between callbacks. |

`mello_peer_send_audio_frame` is safe on the capture thread while another thread renegotiates the same peer. It holds the peer `mutex_` only to copy the track pointer and never takes `negotiation_mutex_`. Renegotiation holds `negotiation_mutex_` and takes `mutex_` only for short copies. libdatachannel `Track::send` is thread safe and does not block on the network.

libmello is **synchronous C++ by design** — no async runtimes. Threads are created internally and communicate via lock-free ring buffers and thread-safe queues. The C API is callable from any thread but `MelloContext` operations are not thread-safe (caller must serialize).

---

## 8. Performance Targets

| Metric | Target |
|--------|--------|
| Audio capture latency | <10ms |
| Active speech enhancement (RNNoise + VAD) | <5ms per 20ms speech frame |
| Opus encode | <2ms per 20ms frame |
| Video capture (DXGI) | <2ms |
| Video encode (NVENC) | <5ms |
| Video decode (DXVA2) | <5ms |
| Peer round-trip to a local SFU | <5ms |
| Library size | <5MB |

---

## 9. Testing

Tests use Google Test and live in `libmello/tests/`. Tests requiring audio hardware (WASAPI capture/playback) are separated and excluded from CI since they need physical devices.

`EchoCancellerTest` carries an ERLE harness: broadband synthetic loopback (frame-aligned plus a misaligned-delay variant for the delay estimator), a blind no-reference passthrough check, and render-accumulator coverage. `VpioDuplex` tests run against real CoreAudio devices where present and skip headless.

Run tests:
```bash
cd libmello/build && ctest --output-on-failure
```

---

*This spec defines libmello. For backend infrastructure, see [04-BACKEND.md](./04-BACKEND.md).*
