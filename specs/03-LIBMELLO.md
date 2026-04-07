# LIBMELLO Specification

> **Component:** libmello (Low-Level C++ Library)  
> **Language:** C++17  
> **Status:** Beta Scope  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

libmello is the C++ library that handles all low-level audio/video capture, encoding, decoding, and P2P transport. It exposes a pure C API for FFI compatibility with Rust and future mobile platforms.

**Key Responsibilities:**
- Audio capture (WASAPI), processing (WebRTC AEC3 + AGC2, RNNoise, Silero VAD), and encoding (Opus)
- Video capture (DXGI), encoding (NVENC/AMF/QSV), and decoding
- P2P transport (libdatachannel)
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
│   │   ├── opus_encoder.hpp/cpp
│   │   ├── opus_decoder.hpp/cpp
│   │   └── jitter_buffer.hpp       # Audio jitter buffer
│   │
│   ├── video/
│   │   ├── video_pipeline.hpp/cpp
│   │   ├── capture_dxgi.hpp/cpp    # Desktop Duplication API
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
| `MelloPeerConnection` | Single P2P peer connection |

### API surface groups

| Group | Key functions | Notes |
|-------|---------------|-------|
| **Context** | `mello_init`, `mello_destroy`, `mello_get_error` | One context per app |
| **Voice** | `mello_voice_start_capture`, `stop_capture`, `set_mute`, `set_deafen`, `is_speaking`, `set_vad_callback`, `get_packet`, `feed_packet` | Mute stops sending but capture continues for VAD |
| **Stream Host** | `mello_stream_start_host`, `stop_host`, `get_video_packet`, `request_keyframe` | Config struct controls resolution/bitrate/encoder |
| **Stream View** | `mello_stream_start_view`, `stop_view`, `feed_video_packet`, `get_frame`, `free_frame` | Caller must free frames after use |
| **P2P Transport** | `mello_peer_create`, `destroy`, `set_ice_servers`, `create_offer`, `create_answer`, `set_remote_description`, `add_ice_candidate`, `send_unreliable`, `send_reliable`, `recv` | Two data channels per peer: reliable (control) + unreliable (media) |
| **Devices** | `mello_get_audio_inputs`, `get_audio_outputs`, `set_audio_input`, `set_audio_output`, `get_encoders` | Query/switch audio devices and video encoders at runtime |

### Callbacks

- `MelloVoiceActivityCallback` — fires on speaking state change (used for UI VAD indicators)
- `MelloAudioFrameCallback` / `MelloVideoFrameCallback` — raw frame delivery
- `MelloIceCandidateCallback` — ICE trickle candidate generated
- `MelloPeerStateCallback` — peer connection state change

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
│  │ WASAPI  │──▶│AEC3+AGC2│──▶│ RNNoise │──▶│ Silero  │──▶│  Opus   │   │
│  │ Capture │   │  (APM)  │   │ Denoise │   │   VAD   │   │ Encode  │   │
│  └─────────┘   └─────────┘   └─────────┘   └─────────┘   └─────────┘   │
│                                                │              │         │
│                                                ▼              ▼         │
│                                          VAD Callback    Packets Out   │
│                                                                         │
│  PLAYBACK PATH:                                                         │
│  ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐                 │
│  │ Packets │──▶│  Opus   │──▶│ Jitter  │──▶│ WASAPI  │                 │
│  │   In    │   │ Decode  │   │ Buffer  │   │ Playback│                 │
│  └─────────┘   └─────────┘   └─────────┘   └─────────┘                 │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### Key design decisions

- **WebRTC APM (AEC3 + AGC2):** Runs first on the raw mic after WASAPI capture (AEC needs the speaker reference from mixed playback). Then RNNoise → Silero VAD → Opus.
- **RNNoise over alternatives:** Real-time, small model (<100KB), no GPU needed, well-tested in voice comms. Runs at 48kHz which matches Opus.
- **Silero VAD:** ONNX-based neural VAD with hysteresis (3 speech frames to activate, 15 silence frames to deactivate). More accurate than RNNoise's built-in VAD for detecting speech vs. keyboard/ambient noise.
- **Opus at 64kbps stereo:** Good quality for voice, well within P2P bandwidth budget. Frame size is 20ms (960 samples at 48kHz).
- **Jitter buffer:** Adaptive, compensates for P2P network jitter. Uses PLC (packet loss concealment) via Opus decoder when packets are late.


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

---

## 5. Video Pipeline

### Host (Capture → Encode)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         VIDEO PIPELINE (HOST)                           │
│                                                                         │
│  ┌─────────┐   ┌─────────┐   ┌─────────┐   ┌─────────┐                 │
│  │  DXGI   │──▶│  Color  │──▶│ Hardware│──▶│ Packet  │                 │
│  │ Capture │   │ Convert │   │ Encode  │   │ Queue   │                 │
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
- **Zero-copy VRAM pipeline:** DXGI captures to a D3D11 texture, color conversion (BGRA→NV12) happens on GPU via compute shader, and the encoder reads directly from VRAM. No GPU→CPU→GPU round-trips for the host.
- **DXGI Desktop Duplication:** Captures all monitors at native resolution. Handles cursor compositing, display rotation, and secure desktop transitions. Requires Windows 8+.
- **Abstract encoder interface:** All three hardware encoders implement the same `Encoder` base class. `create_best_encoder()` probes available hardware at runtime.

---

## 6. Transport Layer

Each P2P connection uses libdatachannel and creates two data channels:

| Channel | Ordered | Reliable | Used for |
|---------|---------|----------|----------|
| `reliable` | Yes | Yes | Control messages, signaling, chat relay |
| `unreliable` | No | No | Audio packets, video packets |

### ICE/NAT traversal

- STUN servers for reflexive candidates (>90% of typical NATs)
- TURN servers as fallback relay (time-limited credentials from Nakama `get_ice_servers` RPC)
- ICE trickle: candidates sent as they're discovered, via Nakama channel messages

### Signaling flow

1. Lower user ID creates offer → sends via Nakama channel
2. Higher user ID receives offer → creates answer → sends via Nakama channel
3. Both sides trickle ICE candidates via Nakama channel
4. Connection established → switch to direct P2P

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

libmello is **synchronous C++ by design** — no async runtimes. Threads are created internally and communicate via lock-free ring buffers and thread-safe queues. The C API is callable from any thread but `MelloContext` operations are not thread-safe (caller must serialize).

---

## 8. Performance Targets

| Metric | Target |
|--------|--------|
| Audio capture latency | <10ms |
| RNNoise + VAD processing | <5ms per 20ms frame |
| Opus encode | <2ms per 20ms frame |
| Video capture (DXGI) | <2ms |
| Video encode (NVENC) | <5ms |
| Video decode (DXVA2) | <5ms |
| P2P round-trip | <5ms (local) |
| Library size | <5MB |

---

## 9. Testing

Tests use Google Test and live in `libmello/tests/`. Tests requiring audio hardware (WASAPI capture/playback) are separated and excluded from CI since they need physical devices.

Run tests:
```bash
cd libmello/build && ctest --output-on-failure
```

---

*This spec defines libmello. For backend infrastructure, see [04-BACKEND.md](./04-BACKEND.md).*
