# Video Pipeline Specification

> **Component:** libmello (C++)
> **Status:** v0.2 Target
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)
> **Related:** [03-LIBMELLO.md](./03-LIBMELLO.md), [12-STREAMING.md](./12-STREAMING.md)

---

## 1. Overview

This spec covers the complete libmello video pipeline: screen capture, GPU-side color conversion, hardware encode, hardware decode, and the staging texture handoff to the UI layer. All heavy work stays on the GPU — the CPU never touches pixel data in the normal path.

**Host pipeline (zero CPU copy):**
```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│   Capture    │────▶│ Color Convert│────▶│   HW Encode  │────▶│ Packet Queue │
│ DXGI / WGC   │     │  BGRA→NV12   │     │NVENC/AMF/QSV │     │              │
│ ID3D11Tex2D  │     │   (GPU CS)   │     │              │     │  To network  │
└──────────────┘     └──────────────┘     └──────────────┘     └──────────────┘
        ▲
        │  All transfers stay in VRAM on the same D3D11 device
```

**Viewer pipeline (streaming-quality stage 3):**
```
┌──────────────┐   ┌──────────────┐   ┌──────────────┐   ┌────────────────┐   ┌──────────────┐
│ H.264 RTP AU │──▶│   Receive    │──▶│  HW Decode   │──▶│ Present thread │──▶│    DComp     │
│ any order,   │   │   timeline   │   │ NVDEC/AMF/   │   │ render time,   │   │  swap chain  │
│ NACK repair  │   │ FrameBuffer, │   │ OpenH264,    │   │ keyed-mutex    │   │ (waitable)   │
│              │   │ VCMTiming    │   │ 3 slots      │   │ shared RGBA    │   │              │
└──────────────┘   └──────────────┘   └──────────────┘   └────────────────┘   └──────────────┘
```

Transport, AU gating, and presentation ownership are normative in
[12-STREAMING.md](./12-STREAMING.md); this document focuses on GPU pipeline details.

---

## 2. Platform Abstraction

### 2.1 GraphicsDevice

All pipeline components are initialised with a `GraphicsDevice` handle rather than a typed `ID3D11Device*`. This keeps the encoder/decoder/capture interfaces platform-agnostic and ready for a future Apple (Metal) backend without breaking changes.

```cpp
// src/video/graphics_device.hpp

#pragma once
#include <cstdint>

namespace mello::video {

enum class GraphicsBackend {
    D3D11,   // Windows — current
    Metal,   // macOS / iOS — future
};

struct GraphicsDevice {
    GraphicsBackend backend;
    void* handle;   // ID3D11Device* on D3D11, MTLDevice* on Metal

    // Convenience: cast to D3D11 device (asserts if backend != D3D11)
    struct ID3D11Device* d3d11() const;
};

/// Create the shared D3D11 device used by the entire pipeline.
/// Must be called once at startup. All pipeline components receive
/// a pointer to this device — they must not create their own.
GraphicsDevice create_d3d11_device();

} // namespace mello::video
```

### 2.2 Device Lifetime and Sharing Constraint

**The entire pipeline shares a single `GraphicsDevice`.** This is a hard requirement for zero-copy:

- NVENC and AMF require the texture they encode to be allocated on the same D3D11 device used to create the encoder session.
- DXGI Desktop Duplication and WGC produce textures on the device they are initialised with.
- Cross-device copies require a CPU readback, breaking zero-copy.

`create_d3d11_device()` is called once by `VideoPipeline` (§3) and its result is passed into every subsystem. No subsystem may create its own D3D11 device.

```
VideoPipeline
  │
  ├── GraphicsDevice (owns D3D11 device)
  │
  ├── CaptureSource   ← initialised with &device
  ├── VideoPreprocessor ← initialised with &device
  ├── Encoder         ← initialised with &device
  └── Decoder         ← initialised with &device
```

---

## 3. VideoPipeline

`VideoPipeline` is the top-level C++ class owned by `MelloStreamHost` / `MelloStreamView`. It wires the subsystems together and owns the shared `GraphicsDevice`.

```cpp
// src/video/video_pipeline.hpp

#pragma once
#include "graphics_device.hpp"
#include "capture_source.hpp"
#include "video_preprocessor.hpp"
#include "encoder.hpp"
#include "decoder.hpp"
#include <memory>
#include <functional>

namespace mello::video {

struct PipelineConfig {
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t bitrate_kbps;
    bool     low_latency = true;   // Always true for interactive streaming
};

class VideoPipeline {
public:
    using PacketCallback = std::function<void(const uint8_t* data, size_t size, bool is_keyframe, uint64_t ts)>;
    using FrameCallback  = std::function<void(const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t ts)>;

    VideoPipeline();
    ~VideoPipeline();

    // HOST SIDE
    bool start_host(const CaptureSourceDesc& source, const PipelineConfig& config, PacketCallback on_packet);
    // Every wait inside is bounded. See "Teardown never waits without a bound".
    void stop_host();
    void request_keyframe();
    void set_bitrate(uint32_t kbps);
    void get_stats(EncoderStats& out) const;

    // VIEWER SIDE
    bool start_viewer(const PipelineConfig& config, FrameCallback on_frame);
    void stop_viewer();
    bool feed_packet(const uint8_t* data, size_t size, bool is_keyframe);

    // CURSOR (host: returns latest cursor packet; viewer: apply cursor overlay)
    bool get_cursor_packet(uint8_t* buf, size_t* size);
    void apply_cursor_packet(const uint8_t* buf, size_t size);

private:
    GraphicsDevice        device_;
    std::unique_ptr<CaptureSource>   capture_;
    std::unique_ptr<VideoPreprocessor> preprocessor_;
    std::unique_ptr<Encoder>         encoder_;
    std::unique_ptr<Decoder>         decoder_;
    std::unique_ptr<StagingTexture>  staging_;
};

} // namespace mello::video
```

---

## 4. Capture

### 4.1 CaptureSource Abstraction

```cpp
// src/video/capture_source.hpp

#pragma once
#include "graphics_device.hpp"
#include <d3d11.h>
#include <cstdint>
#include <string>
#include <vector>
#include <functional>

namespace mello::video {

enum class CaptureMode {
    Monitor,    // Full display output   → DXGI DDI (WGC with prefer_wgc)
    Window,     // Specific HWND         → WGC backend
    Process,    // Game / app by PID     → ProcessCapture, the capture ladder
    Reference,  // Test only: a YUV clip → ReferenceCapture (§4.7)
};

struct ReferenceClip {          // Reference mode only
    std::string path;           // raw YUV 4:2:0, frames back to back, no header
    uint32_t    width, height;  // even
    uint32_t    fps = 60;
    bool        nv12 = false;   // false: I420
};

struct CaptureSourceDesc {
    CaptureMode mode;
    union {
        uint32_t monitor_index; // Monitor mode: 0 = primary
        void*    hwnd;          // Window mode: HWND
        uint32_t pid;           // Process mode: process ID
    };
    bool prefer_wgc = false;    // Monitor mode: WGC instead of DXGI (benchmark)
    bool allow_hook = false;    // Process mode: the backend policy allows the hook
    ReferenceClip reference{};  // Reference mode: the clip to play
};

class CaptureSource {
public:
    using FrameCallback = std::function<void(ID3D11Texture2D* texture, uint64_t timestamp_us)>;

    virtual ~CaptureSource() = default;

    virtual bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) = 0;
    virtual bool start(uint32_t target_fps, FrameCallback callback) = 0;
    virtual void stop() = 0;

    virtual uint32_t width()  const = 0;
    virtual uint32_t height() const = 0;
    virtual const char* backend_name() const = 0;

    // Ladder and status hooks, with defaults. See capture_source.hpp.
    virtual bool get_cursor(CursorData& out);
    virtual bool consume_swap_event();      // a method change: force a keyframe
    virtual bool failed() const;            // every method failed, with proof
    virtual std::string method_history() const;
    virtual CaptureState state() const;     // Capturing, WaitingMinimized, WaitingForGame, Failed
    virtual bool target_exited() const;     // the captured process quit
    virtual bool captures_while_minimized() const;  // true for the hook only
};

/// Factory — DXGI or WGC for Monitor and Window, ProcessCapture for Process,
/// ReferenceCapture for Reference (Windows only; nullptr on macOS).
std::unique_ptr<CaptureSource> create_capture_source(const CaptureSourceDesc& desc);

} // namespace mello::video
```

### 4.2 Backend Selection Rules

| Capture mode | Condition | Backend |
|---|---|---|
| `Monitor` | Default | DXGI DDI |
| `Monitor` | `prefer_wgc` (benchmark, `MELLO_CAPTURE_MONITOR_WGC`) | WGC on that monitor |
| `Window` | Always | WGC on the HWND |
| `Process` | `allow_hook` true | Ladder: Hook, WGC window, WGC monitor, DXGI DDI |
| `Process` | `allow_hook` false | Ladder: WGC window, WGC monitor, DXGI DDI |
| `Reference` | Test only (`MELLO_CAPTURE_REFERENCE`) | ReferenceCapture: plays the clip of `mello_test_set_reference_clip` (§4.7) |

`Process` mode runs the capture ladder (`capture_process.cpp`): the first
method that delivers a frame keeps the stream. `allow_hook` is the backend hook
policy decision, made in mello-core by IGDB id; the hook step still runs
libmello's run-time checks before it injects. See `12-STREAMING.md` §3.1 for
the rules and the hook, and `mello-backlog/plans/streaming-reliability.md` for
why.

### 4.3 DXGI Desktop Duplication Backend

Used for monitor capture, and as the last step of the process ladder. Acquires frames directly from the DXGI output without going through DWM. It cannot see an exclusive-fullscreen game: only the game capture hook can. On NVIDIA, `AcquireNextFrame` can block inside the driver and ignore its timeout, which is why the ladder tries it last (`12-STREAMING.md` §3.1).

```cpp
// src/video/capture_dxgi.hpp

#pragma once
#include "capture_source.hpp"
#include <dxgi1_2.h>
#include <wrl/client.h>
#include <thread>
#include <atomic>

using Microsoft::WRL::ComPtr;

namespace mello::video {

class DxgiCapture : public CaptureSource {
public:
    bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) override;
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width()  const override { return width_; }
    uint32_t height() const override { return height_; }
    const char* backend_name() const override { return "DXGI-DDI"; }

private:
    void capture_thread();

    ComPtr<ID3D11Device>           device_;
    ComPtr<ID3D11DeviceContext>    context_;
    ComPtr<IDXGIOutputDuplication> duplication_;

    uint32_t           width_      = 0;
    uint32_t           height_     = 0;
    uint32_t           target_fps_ = 60;
    std::thread        thread_;
    std::atomic<bool>  running_{false};
    FrameCallback      callback_;
};

} // namespace mello::video
```

**Display mode change:** duplication is revoked and rebuilt at the new size;
the preprocessor scales the new desktop into the fixed encode size. The rebuild
refused a new size until streaming-quality stage 8, and the stream gave up
after about 10 s.

**Cursor handling in DXGI:** `AcquireNextFrame` gives the pointer position and shape apart from the desktop image, which has no pointer. The backend turns every shape kind into straight-alpha BGRA (`pointer_shape_to_bgra`, `cursor.cpp`) and reports `CursorMode::Composite`; the preprocessor draws it into a copy of the frame (`VideoPreprocessor::convert` with a `CursorOverlay`). See `12-STREAMING.md` §10.

### 4.4 Windows Graphics Capture (WGC) Backend

Used for windowed and borderless windowed captures. WGC goes through DWM and works correctly with all modern windowed games. Minimum requirement: Windows 10 1803 (build 17134).

```cpp
// src/video/capture_wgc.hpp

#pragma once
#include "capture_source.hpp"
#include <winrt/Windows.Graphics.Capture.h>
#include <winrt/Windows.Graphics.DirectX.Direct3D11.h>
#include <wrl/client.h>
#include <thread>
#include <atomic>

namespace mello::video {

class WgcCapture : public CaptureSource {
public:
    bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) override;
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width()  const override { return width_; }
    uint32_t height() const override { return height_; }
    const char* backend_name() const override { return "WGC"; }

private:
    void on_frame_arrived(
        winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool const& pool,
        winrt::Windows::Foundation::IInspectable const&
    );

    winrt::Windows::Graphics::Capture::GraphicsCaptureItem    item_{nullptr};
    winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool frame_pool_{nullptr};
    winrt::Windows::Graphics::Capture::GraphicsCaptureSession session_{nullptr};

    Microsoft::WRL::ComPtr<ID3D11Device>        device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;

    uint32_t          width_  = 0;
    uint32_t          height_ = 0;
    std::atomic<bool> running_{false};
    FrameCallback     callback_;
};

} // namespace mello::video
```

**Window resize:** a frame whose `ContentSize` differs from the pool size is
dropped, and the pool is made again (`Recreate`) at the new size. The next
frame has the whole window.

**WGC and cursor:** `IsCursorCaptureEnabled(true)`. WGC draws the pointer into the captured texture where it is over the window or monitor (`CursorMode::InFrame`). Until streaming-quality stage 8 it was off, and no viewer saw a pointer.

### 4.5 ProcessCapture and the Capture Ladder

`ProcessCapture` (`src/video/capture_process.hpp/.cpp`) owns the ladder for
`Process` mode. It finds the game's main window, builds the first method in
the ladder, and moves to the next method only on evidence. The evidence rules,
the 15 s wait for a game that has drawn nothing, the walk back up on exclusive
fullscreen, and the 30 s retry are in `12-STREAMING.md` §3.1.

| Ladder step | Backend |
|---|---|
| `Hook` | `HookCapture` (`capture_hook.cpp`), the game capture hook. Only with `allow_hook`, and only after `hook::check_process` passes. |
| `WgcWindow` | `WgcCapture` on the main window |
| `WgcMonitor` | `WgcCapture` on the window's monitor |
| `Dxgi` | `DxgiCapture` on the window's monitor |

Exclusive fullscreen is detected with `SHQueryUserNotificationState`. It does
not swap to DXGI: no screen method sees exclusive fullscreen. It rebuilds the
current method and restarts the ladder from the top, so the hook gets another
chance.

A method change starts the next backend before it stops the old one. Each
backend delivers through a callback with its own token, and only the active
token reaches the pipeline. A walk back up keeps the old backend until the next
one delivers its first frame (2 s at most); on failure the next backend stops
and the old one keeps the stream. `ProcessCapture::activate` has the rules.

Every method change raises `consume_swap_event()`, and the pipeline forces a
keyframe so the viewer can resync. `state()` reports what the capture is doing
for the pause card, and `target_exited()` ends the stream when the game quits.

### 4.6 Game Process Enumeration

Used to populate the "Share a game" list in the host UI.

```cpp
// src/video/process_enum.hpp

#pragma once
#include <string>
#include <vector>
#include <cstdint>

namespace mello::video {

struct GameProcess {
    uint32_t    pid;
    std::string name;
    std::string exe;            // executable file name, the catalogue key
    bool        is_fullscreen;  // main window covers its monitor
    std::string path;           // full executable path; empty when windowless
    std::string window_title;   // empty when windowless
    bool        is_foreground = false;
    int64_t     started_at_ms = 0;  // with pid, identifies a process across restarts
};

/// Returns every running process. libmello does not filter.
std::vector<GameProcess> enumerate_game_processes();

} // namespace mello::video
```

mello-core decides which processes are games. It looks each one up in the
bundled game catalogue (`client/assets/catalogue/head.bin`) with
`Head::lookup_exe(exe, path)`, which is path-aware: a guarded entry such as
`javaw.exe` matches only when its marker is in the path. A catalogue match
gives the display name and the `igdb_id` that the hook policy uses. The picker
lists catalogue games first, then other processes that own a visible window. A
user can also pick "Share a window" (any visible HWND) or "Share a monitor".
See `17-GAME-SENSING.md` §2.

---

### 4.7 Reference Clip Backend (test only)

`capture_reference.{hpp,cpp}`. The capture-less source of the streaming
quality gate (`tools/stream-gate`, `TESTING.md` "Streaming quality gate").
Nothing in the product selects it.

| Item | Rule |
|---|---|
| Selection | `MelloCaptureSource.mode = MELLO_CAPTURE_REFERENCE`. The clip comes from the context: `mello_test_set_reference_clip(ctx, path, width, height, fps, format)`. A host without a clip fails to start. |
| Input | Raw YUV 4:2:0, I420 or NV12, 8 bit, BT.709 studio range, frames back to back. The file size must be a whole number of frames. |
| Thread | One source thread. Frame n is due at start + n × 10⁶ / fps µs. The thread waits on a high-resolution waitable timer to 1 ms before the due time, then yields. It never skips a frame: every clip frame reaches the pipeline in order, and the clip loops at its end. |
| Upload | The thread reads frame n, packs it as NV12 in memory, `UpdateSubresource` into a default-usage NV12 texture (bind flag render target: the video processor refuses an input view on a shader-resource NV12 texture with `E_INVALIDARG`), and a D3D11 video processor converts it to BGRA (input BT.709 studio range, output full-range RGB). |
| Callback | The same contract as DXGI and WGC: a BGRA `ID3D11Texture2D` (two slots, used in turn) and the due time in µs of the steady clock. The preprocessor then converts it back to NV12, so the encoder sees the clip's values up to rounding. |
| Late frames | A frame handed over more than 2 ms after its due time counts as late (`mello_stream_reference_info`). The timestamp stays the due time. |
| Clock | `mello_clock_now_us()` is the clock of the due times and of the viewer frame callback (`ts`), so a harness in the same process measures glass-to-glass. |
| Game audio (stage 7) | `mello_test_set_reference_audio(ctx, path)`: raw 48 kHz stereo int16, one clip loop long. The source then has a second thread: 10 ms chunk j is captured at start + j × 10 ms (frame 0 at start, the video's clock) and handed over 10 ms later, when it is complete, as a capture device does, through `CaptureSource::set_timed_audio_callback` with that capture time. `provides_timed_audio()` is true, so `mello_stream_start_audio` opens no loopback device. The audio loops with the clip. |

## 5. Color Conversion

Capture produces BGRA textures. Hardware encoders expect NV12 (YUV 4:2:0 planar). Conversion runs as a compute shader on the GPU — no CPU involvement, no texture copy to system memory.

```cpp
// src/video/video_preprocessor.hpp

#pragma once
#include "graphics_device.hpp"
#include <d3d11.h>
#include <wrl/client.h>

using Microsoft::WRL::ComPtr;

namespace mello::video {

class VideoPreprocessor {
public:
    VideoPreprocessor();
    ~VideoPreprocessor();

    bool initialize(const GraphicsDevice& device, uint32_t width, uint32_t height);

    /// Convert BGRA source texture to NV12 (and downscale if resolutions differ).
    /// Output texture is owned by VideoPreprocessor and reused across calls.
    /// Returns the NV12 texture pointer (valid until next call to convert()).
    ID3D11Texture2D* convert(ID3D11Texture2D* bgra_source);

    void shutdown();

private:
    ComPtr<ID3D11Device>            device_;
    ComPtr<ID3D11DeviceContext>     context_;
    ComPtr<ID3D11ComputeShader>     cs_bgra_to_nv12_;
    ComPtr<ID3D11ShaderResourceView>  srv_input_;
    ComPtr<ID3D11UnorderedAccessView> uav_output_y_;
    ComPtr<ID3D11UnorderedAccessView> uav_output_uv_;
    ComPtr<ID3D11Texture2D>           nv12_texture_;

    uint32_t width_  = 0;
    uint32_t height_ = 0;
};

} // namespace mello::video
```

The NV12 output texture is allocated once at `initialize()` time with `D3D11_USAGE_DEFAULT` and `D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE`. This is the same texture registered as input to the hardware encoder — it is never read back to CPU.

### 5.5 GPU Downscaling

When the capture source resolution exceeds the streaming preset's target resolution (e.g. a 2560×1440 monitor captured for a 1080p stream), the `VideoPreprocessor` performs **bilinear downscaling in the same GPU pass** as the BGRA→NV12 conversion. This avoids any additional GPU passes or CPU copies.

The `VideoPreprocessor::initialize()` overload accepts separate input and output dimensions:

```cpp
bool initialize(const GraphicsDevice& device,
                uint32_t in_w, uint32_t in_h,    // capture resolution
                uint32_t out_w, uint32_t out_h);  // encode resolution
```

The D3D11 Video Processor handles scaling automatically when `content_desc.InputWidth/Height` differs from `OutputWidth/Height`. Source and destination rectangles are set explicitly to ensure correct sampling.

In `VideoPipeline::start_host()`, `encode_size_for` sets the encode size from the capture size and `PipelineConfig::height` (the preset height). The width follows the source aspect, so the scale never stretches the picture. The rules are in `12-STREAMING.md` §3.2. All dimensions are even-aligned for NV12 compatibility.

The rectangles are always explicit (`VideoPreprocessor::fit`): the source
rectangle is the whole source, the destination rectangle keeps its aspect
inside the output, and the output background is studio black (Y 16, Cb and
Cr 128, given in YCbCr: given as RGB black the NVIDIA driver wrote Y 0). When a
frame arrives at a new size, `convert` makes the video processor again for the
new input size and keeps the NV12 ring textures, which the encoder registered.
`ConvertResult::source_resized` tells the pipeline, which counts the change in
`source_size_changes`.

This reduces encoded pixel count significantly (e.g. 2036×1392 → 1584×1080 = 40% fewer pixels), improving quality per bit and lowering bandwidth.

---

## 6. Encoding

### 6.1 Codec Configuration

**Primary codec: H.264** — universal hardware support across all target GPU vendors.

Low-latency encode profile (mandatory for all hardware encoders):
- No B-frames (`num_b_frames = 0`)
- Rate control: VBR with moderate headroom (`max = avg × 1.25`, `vbv = max × 0.5` with a floor of 4 frames of bits, `vbvInitialDelay = vbv / 2`) on every backend (`vbv_bits_for` in `encoder.hpp`; AMF and QSV had one frame before streaming-quality stage 4)
- Keyframe interval: 4 seconds (4 x the preset frame rate) plus IDRs on demand (streaming-quality stage 4; 2 s before; 10 s held at 4 s, see 12-STREAMING)
- H.264 level 5.1 on NVENC, AMF and QSV
- Look-ahead: disabled

The VBR headroom of 1.25× allows keyframes slightly more bits without large bandwidth spikes. The half-second VBV keeps rate control tight for interactive latency while leaving IDRs enough room — a one-frame VBV visibly starves keyframes. This applies to both initial configuration and dynamic `set_bitrate()` reconfiguration.

**Stretch goal: AV1** — activated only when `EncoderFactory` confirms AV1 support on the host GPU and `mello_get_decoders()` confirms AV1 decode on the viewer. Falls back to H.264 if either side can't support it. AV1 uses the same low-latency profile constraints.

### 6.2 Abstract Encoder Interface

```cpp
// src/video/encoder.hpp

#pragma once
#include "graphics_device.hpp"
#include <d3d11.h>
#include <cstdint>
#include <vector>

namespace mello::video {

enum class VideoCodec { H264, AV1 };

struct EncoderConfig {
    uint32_t   width;
    uint32_t   height;
    uint32_t   fps;
    uint32_t   bitrate_kbps;
    uint32_t   keyframe_interval = 120;   // the pipeline sets 4 x fps
    VideoCodec codec = VideoCodec::H264;
};

struct EncodedPacket {
    std::vector<uint8_t> data;
    uint64_t             timestamp_us;
    bool                 is_keyframe;
};

struct EncoderStats {
    uint32_t bitrate_kbps;
    uint32_t fps_actual;
    uint32_t keyframes_sent;
    uint64_t bytes_sent;
};

class Encoder {
public:
    virtual ~Encoder() = default;

    virtual bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) = 0;
    virtual void        shutdown() = 0;

    /// Encode one NV12 texture. Fills `out` and returns true if a packet is ready.
    virtual bool        encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) = 0;

    virtual void        request_keyframe() = 0;
    virtual void        set_bitrate(uint32_t kbps) = 0;
    virtual void        get_stats(EncoderStats& out) const = 0;
    virtual bool        supports_codec(VideoCodec codec) const = 0;
    virtual const char* name() const = 0;
};

} // namespace mello::video
```

### 6.3 EncoderFactory

```cpp
// src/video/encoder_factory.hpp

#pragma once
#include "encoder.hpp"
#include <memory>
#include <vector>

namespace mello::video {

/// Priority order: NVENC → AMF → QSV (oneVPL) → OpenH264 (§6.7)
/// Probes each in order; returns first that initialises successfully.
/// OpenH264 is the only candidate when MELLO_SOFT_ENCODER=1, and never one
/// without it. If none initialises, returns Err — streaming requires hardware
/// encode.
std::unique_ptr<Encoder> create_best_encoder(
    const GraphicsDevice& device,
    const EncoderConfig&  config
);

/// Returns all encoder types available on this machine.
std::vector<const char*> enumerate_encoders(const GraphicsDevice& device);

} // namespace mello::video
```

If no hardware encoder is found, `create_best_encoder()` returns `Err`. mello-core surfaces a user-facing error: *"Streaming requires a hardware encoder (NVIDIA, AMD, or Intel). None was found on this machine."* No software encode fallback exists.

### 6.4 NVENC Encoder

Interfaces with NVIDIA Video Codec SDK via `nvEncodeAPI.h`. Registers the NV12 texture as an `NV_ENC_REGISTERED_PTR` input resource — the encoder reads directly from VRAM with no CPU copy.

```cpp
// src/video/encoder_nvenc.hpp

#pragma once
#include "encoder.hpp"
#include <nvEncodeAPI.h>
#include <wrl/client.h>

namespace mello::video {

class NvencEncoder : public Encoder {
public:
    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override;
    const char* name() const override { return "NVENC"; }

    static bool is_available();

private:
    NV_ENCODE_API_FUNCTION_LIST fn_{};
    void*                       encoder_   = nullptr;
    NV_ENC_REGISTERED_PTR       reg_res_   = nullptr;
    NV_ENC_OUTPUT_PTR           out_buf_   = nullptr;
    bool                        force_idr_ = false;

    EncoderStats stats_{};
};

} // namespace mello::video
```

### 6.5 AMF Encoder

Interfaces with AMD Advanced Media Framework SDK. Wraps the NV12 texture in an `AMFSurface` using `AMF_SURFACE_DX11` memory type — same device, zero copy.

```cpp
// src/video/encoder_amf.hpp

#pragma once
#include "encoder.hpp"
#include <AMF/core/Factory.h>
#include <AMF/components/VideoEncoderVCE.h>
#include <AMF/components/VideoEncoderAV1.h>

namespace mello::video {

class AmfEncoder : public Encoder {
public:
    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override;
    const char* name() const override { return "AMF"; }

    static bool is_available();

private:
    amf::AMFFactory*         factory_   = nullptr;
    amf::AMFContextPtr       context_   = nullptr;
    amf::AMFComponentPtr     encoder_   = nullptr;
    VideoCodec               codec_     = VideoCodec::H264;
    bool                     force_idr_ = false;

    EncoderStats stats_{};
};

} // namespace mello::video
```

### 6.6 QSV / oneVPL Encoder (Intel)

Interfaces with Intel oneVPL (Video Processing Library). oneVPL deprecates Intel Media SDK and is the correct target for any hardware shipping from 2021 onwards.

D3D11 interop requires setting up a `mfxLoader` with the D3D11 device via `MFX_HANDLE_D3D11_DEVICE`. The NV12 texture is submitted as an `mfxFrameSurface1` with `MemType = MFX_MEMTYPE_D3D11_INT_BUFFER`.

```cpp
// src/video/encoder_qsv.hpp

#pragma once
#include "encoder.hpp"
#include <vpl/mfx.h>

namespace mello::video {

class QsvEncoder : public Encoder {
public:
    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override;
    const char* name() const override { return "QSV-oneVPL"; }

    static bool is_available();

private:
    mfxLoader  loader_  = nullptr;
    mfxSession session_ = nullptr;
    bool       force_idr_ = false;

    EncoderStats stats_{};
};

} // namespace mello::video
```

### 6.7 No Software Encoder in the Product

The test lane of the streaming quality gate has one software encoder,
`encoder_openh264.{hpp,cpp}`: Cisco's prebuilt OpenH264 DLL, the same DLL
that `openh264_dynload.cpp` loads for decode (`WelsCreateSVCEncoder`). It is
the last entry of the factory and the only entry when the process environment
has `MELLO_SOFT_ENCODER=1` (read with `GetEnvironmentVariableA`, so a harness
can set it after start). Without the variable the factory never tries it.

| Setting | Value |
|---|---|
| Usage | `CAMERA_VIDEO_REAL_TIME`, low complexity, one spatial and one temporal layer, one slice, one thread |
| Rate control | `RC_BITRATE_MODE`, maximum 1.25 × target (NVENC's VBR headroom), no frame skip |
| GOP | IDR every `keyframe_interval` frames and on `request_keyframe()`; constant SPS/PPS ids |
| Colour | BT.709 studio range in the VUI, as NVENC |
| Input | NV12 texture → staging copy → `Map` → I420 on the encode thread |
| Controls | `set_bitrate`, `set_framerate` and `request_keyframe` store atomics on any thread; the encode thread applies them before the next frame |

The API headers are vendored in `third_party/openh264/include/wels` (v2.6.0,
BSD-2, the version of the DLL); nothing links the library. The same switch
for the viewer is `MELLO_SOFT_DECODER=1` (§7.2).

There is no software encoder fallback in the product. Streaming requires a hardware encoder. This is an intentional product and licensing decision — GPL-licensed software encoders (x264, x265) are incompatible with Mello's Apache 2.0 licence, and AV1 software encoders are too slow for real-time use.

If `EncoderFactory` exhausts all hardware candidates without success, it returns `Err`. The stream cannot start and the user is shown a clear error message. In practice this affects only machines with no GPU at all — any system with NVIDIA, AMD, or Intel graphics from the last decade has hardware H.264 encode capability.

---

## 7. Decoding

Streaming-quality stage 3. The decoders run on the viewer pipeline's decode
thread (`ViewerPipeline`, `video_viewer.hpp`), which takes frames from the
viewer's receive timeline (12-STREAMING.md §7.2). This section covers the GPU
side: the decoder interface, the output slots and each decoder.

### 7.1 Abstract Decoder Interface

```cpp
// src/video/decoder.hpp (abridged)

struct DecodedPicture {                 // Windows
    ComPtr<ID3D11Texture2D> texture;    // keeps the slot texture alive
    DXGI_FORMAT format;                 // NV12, or R8 with the NV12 layout
    uint32_t width, height;             // visible size, from the SPS
    uint32_t uv_row;                    // R8 layout: first chroma row
};

class Decoder {
public:
    static constexpr int kOutputSlots = 3;
    virtual bool initialize(const GraphicsDevice&, const DecoderConfig&) = 0;
    virtual void shutdown() = 0;
    // FrameReady: the picture is in output slot `slot`.
    virtual DecodeFeedResult decode(const uint8_t* data, size_t size,
                                    bool is_keyframe, int slot) = 0;
#ifdef _WIN32
    virtual bool picture(int slot, DecodedPicture& out) const = 0;
#elif defined(__APPLE__)
    virtual void* get_frame_buffer() = 0;   // CVPixelBufferRef, BGRA
#endif
    virtual const char* name() const = 0;
};
```

Each decoder owns `kOutputSlots` textures and writes each picture into the slot
the caller names. The viewer's ring has the same 3 slots, so a slot the present
thread converts is never the slot of the next decode. A new picture size (a new
SPS) makes a new texture for a slot; the `ComPtr` in the ring keeps the old one
alive until the present thread lets it go.

All calls run on the decode thread. D3D11 work on the shared device is safe
across the decode and present threads because the device context is
multithread-protected (§2.2).

### 7.2 DecoderFactory

Order: NVDEC, AMF, OpenH264 (H.264 only; AV1 is never negotiated).
`MELLO_SOFT_DECODER=1` makes OpenH264 the only candidate (the CPU lane of the
streaming quality gate).

There is no D3D11VA decoder. The earlier one was a stub: it sent the raw access
unit as one bitstream buffer with no DXVA picture parameters and no slice
control, decoded nothing, and was chosen on Intel before OpenH264. Stage 3
removed it (plan decision: delete unless an Intel box is on hand for a full
implementation). An Intel viewer decodes with OpenH264. `MELLO_DECODER_D3D11VA`
stays in `mello.h` for ABI stability and is never reported.

### 7.3 NVDEC Decoder

CUDA-D3D11 interop, no host copy:

- The CUDA context is created on the CUDA device of libmello's adapter
  (`cuD3D11GetDevice`), so the registered textures are visible to it.
- Each output slot is an R8 texture, `width` × `height × 3/2`, with the NV12
  layout: luma rows, then interleaved chroma rows from row `height`.
  `cuGraphicsD3D11RegisterResource` registers it once.
- The parser callbacks only record the surface to show
  (`ulMaxDisplayDelay = 0`). After `cuvidParseVideoData` returns, still on the
  decode thread with the CUDA context current, the decoder maps the surface
  (`cuvidMapVideoFrame64`), maps the slot texture
  (`cuGraphicsMapResources`), copies luma and chroma with two `cuMemcpy2D`
  (device to array) and unmaps both. The unmap orders the copy before any later
  D3D11 use of the texture. `plan_nvdec_host_copy` (`nvdec_host_layout.hpp`)
  gives the rows of the two copies, so a stream with a larger coded height
  cannot write past the slot.
- The display size comes from the sequence callback (`display_area`).
- Each packet is one access unit and carries `CUVID_PKT_ENDOFPICTURE`: the
  parser shows the picture in the same `cuvidParseVideoData` call. Without
  the flag it shows each picture only when the next access unit comes, one
  frame late.
- A size change (an SPS at a new size) makes the decoder again in the
  sequence callback. A picture of the old sequence still waiting to be shown
  in that call indexes a surface of the old decoder; it is not shown. The
  slots take the new size at their next copy (`ensure_slot`), the converter
  makes the shared RGBA texture again at the new size, and the presenter
  resizes its swap chain to it.

### 7.4 AMF Decoder

The AMF output surface is a slice of a texture array in AMF's pool. The decoder
copies the slice (`AMFTextureArrayIndexGUID` private data on the texture) into
the slot's NV12 texture with `CopySubresourceRegion`, so the surface goes back
to the pool at once. Not verified on an AMD GPU in stage 3 (Bob's check).

### 7.5 OpenH264 Decoder

The Cisco DLL decodes on the CPU (`DecodeFrameNoDelay`). The I420 picture is
interleaved to NV12 and uploaded into the slot's NV12 texture with
`UpdateSubresource`. The picture size comes from the decoder's buffer info.

### 7.6 VideoToolbox Decoder (macOS)

Unchanged by stage 3 except the slot argument, which it ignores: the ring
retains each `CVPixelBuffer` the decoder hands out.

---

## 8. Viewer Conversion and Present

### 8.1 StagingTexture (the converter)

```cpp
// src/video/staging_texture.hpp (abridged)
class StagingTexture {
public:
    bool initialize(const GraphicsDevice&, bool enable_cpu_readback);
    // Picture → shared RGBA texture (keyed mutex), and staging when asked.
    bool convert(const DecodedPicture& picture, bool copy_to_cpu);
    bool read_rgba(uint8_t* out);      // last converted frame, w*h*4
    void* shared_rgba_handle() const;  // NT handle, key 0
};
```

The present thread converts the picture that is due with a compute shader
(`CSMain` for the R8 layout, `CSMainNv12` for NV12; BT.709 studio swing) into a
private RGBA texture, then copies it into the shared RGBA texture while it holds
the shared texture's keyed mutex (key 0, 100 ms timeout). The presenter reads
the shared texture under the same key. The targets follow the picture size; a
new size gives a new NT handle. Source views are cached per source texture (at
most 8); a cached view holds its texture, so a recycled address cannot reach a
stale view.

### 8.2 ViewerPipeline

| Thread | Work |
|---|---|
| Decode | `VideoReceiveTimeline::poll`: decode a released frame into a free ring slot (or the slot of the oldest frame not on screen), report the decode time or the error; wait as the timeline says. Without a timeline (`feed_packet`) a fed access unit decodes at once and is due at once. |
| Present | Sleep until the render time of the next ready frame, show the newest due frame, drop older due frames, convert, call the native frame callback (or read back for the CPU callback). |

The pipeline holds 1 ms timer resolution (`timeBeginPeriod(1)`) while it runs on
Windows. Counters: `frames_decoded`, `decode_errors`, `frames_presented`,
`frames_dropped_present`, `decode_ms_last`, `decode_ms_mean`,
`present_lateness_ms`, and the timeline's stats (`mello_stream_viewer_get_stats`).

### 8.3 The A/V sync input (streaming-quality stage 7)

The video follows the game audio (12-STREAMING.md §9.4). The input is on the
receive side, not in the pipeline: `mello_stream_viewer_attach_peer` makes one
`AvSyncInput` (`src/transport/av_sync.hpp`) over the view's audio playout and
gives it to the peer (`PeerConnectionImpl::set_av_sync`); every receiver
session of the peer (each video session) gets it.

| Item | Rule |
|---|---|
| Who runs it | The receiver session's worker, every 20 ms (`kAvSyncTickInterval`); it decides every 250 ms, every 100 ms while the last offset is outside +-40 ms. |
| Measurement | The audio heard now, on the video RTP clock (the game audio's A/V sync extension), against the time the frame of the same capture instant is shown: the last shown frame (`ViewerStats::last_shown_rtp`, `last_shown_us`, set by the present thread) moved by the RTP timestamps at 90 kHz. With no shown frame `VideoReceiveTimeline::render_time_us` stands in. The timeline's target minus its current delay counts in as the delay the video still has to reach. |
| Start buffer | `AvSyncInput::start_buffer_ms`: the audio playout asks it when it starts, with the newest held packet; it answers from the render time at the timeline's target delay. |
| Output | `VideoReceiveTimeline::set_min_delay_us` (at least 1 ms, at most the timeline's 150 ms maximum; the timeline applies it again after a timing reset), and for a late video the game audio's NetEQ minimum delay. |
| Steps | A raise of the minimum: at most 10 ms per decision. VCMTiming clamps the delay it applies to the minimum at once, so a raise moves the render times by the step at once; the current delay then follows at 100 ms/s. A lowered minimum: at most 40 ms; the current delay falls at 100 ms/s. |
| Stats | `MelloStreamStats` stage 7 fields: `av_offset_ms` (filtered), `av_video_min_delay_ms`, `av_audio_min_delay_ms`, `av_sync_changes`, `av_sync_active`. |

The timeline's interface was enough: stage 7 changed nothing in
`video_receive_timeline.*`.

---

## 9. Cursor Channel

The cursor is delivered as a separate lightweight packet, not composited into the video stream. This avoids two problems: cursor-shaped compression artefacts on P-frames, and unnecessary frame dirties when only the cursor moves.

### 9.1 Cursor Packet Format

```
// Binary format, sent via control DataChannel (type 0x04, subtype 0x02)

struct CursorPacket {
    uint8_t  subtype     = 0x02;
    int32_t  x;               // Cursor X in host screen coordinates
    int32_t  y;               // Cursor Y in host screen coordinates
    uint8_t  visible;         // 0 = hidden, 1 = visible
    uint8_t  shape_changed;   // 1 = shape_data follows
    uint16_t shape_w;         // Cursor bitmap width (if shape_changed)
    uint16_t shape_h;         // Cursor bitmap height (if shape_changed)
    // uint8_t shape_data[shape_w * shape_h * 4]  RGBA (if shape_changed)
};
```

Position packets (shape_changed = 0) are tiny (~10 bytes) and sent every captured frame. Shape packets are sent only when the cursor image changes (typically on UI element hover).

### 9.2 Host Side

**DXGI backend:** cursor position and shape are read from `DXGI_OUTDUPL_FRAME_INFO` after each `AcquireNextFrame()` call. `PointerPosition` gives x/y/visible; `PointerShapeInfo` gives the bitmap when it changes.

**WGC backend:** cursor capture is disabled on the `GraphicsCaptureSession`. Cursor position and shape are read via `GetCursorInfo()` and `GetIconInfo()` on each frame.

Both backends produce a `CursorPacket` that is returned from `VideoPipeline::get_cursor_packet()`.

### 9.3 Viewer Side

`VideoPipeline::apply_cursor_packet()` deserialises the packet and stores the current cursor state. The Slint render loop reads cursor state from mello-core and composites it over the decoded video frame at render time — not at decode time.

---

## 10. libmello C API Additions

The following additions to the public C API in `mello.h` expose video pipeline capabilities to mello-core (Rust FFI):

```c
// ---- Capability queries ----

typedef enum MelloCodec {
    MELLO_CODEC_H264 = 0,
    MELLO_CODEC_AV1  = 1,
} MelloCodec;

typedef enum MelloEncoderBackend {
    MELLO_ENCODER_NVENC  = 0,
    MELLO_ENCODER_AMF    = 1,
    MELLO_ENCODER_QSV    = 2,
    // No software encoder — streaming requires hardware encode
} MelloEncoderBackend;

typedef enum MelloDecoderBackend {
    MELLO_DECODER_NVDEC   = 0,
    MELLO_DECODER_AMF     = 1,
    MELLO_DECODER_D3D11VA = 2,
    MELLO_DECODER_OPENH264 = 3, // OpenH264 software fallback (H.264)
    MELLO_DECODER_DAV1D    = 4, // dav1d software fallback (AV1)
} MelloDecoderBackend;

/// Returns available encoder backends on this machine, in priority order.
int mello_get_encoders(MelloContext* ctx, MelloEncoderBackend* out, int max_count);

/// Returns available decoder backends on this machine, in priority order.
int mello_get_decoders(MelloContext* ctx, MelloDecoderBackend* out, int max_count);

/// Returns whether no hardware encoder was found (stream cannot start).
bool mello_encoder_available(MelloContext* ctx);

// ---- Capture source ----

typedef enum MelloCaptureMode {
    MELLO_CAPTURE_MONITOR = 0,
    MELLO_CAPTURE_WINDOW  = 1,
    MELLO_CAPTURE_PROCESS = 2,
} MelloCaptureMode;

typedef struct MelloCaptureSource {
    MelloCaptureMode mode;
    uint32_t         monitor_index; // CAPTURE_MONITOR
    void*            hwnd;          // CAPTURE_WINDOW
    uint32_t         pid;           // CAPTURE_PROCESS
} MelloCaptureSource;

/// List running processes matching the bundled game list.
typedef struct MelloGameProcess {
    uint32_t pid;
    char     name[128];
    char     exe[260];
    bool     is_fullscreen;
} MelloGameProcess;

int mello_enumerate_games(MelloContext* ctx, MelloGameProcess* out, int max_count);

/// Start hosting with a specific capture source.
/// Replaces the existing mello_stream_start_host() signature.
MelloStreamHost* mello_stream_start_host_ex(
    MelloContext*             ctx,
    const MelloCaptureSource* source,
    const MelloStreamConfig*  config
);

// ---- Bitrate control ----

/// Hot-reconfigure encoder bitrate without restarting the session.
MelloResult mello_stream_set_bitrate(MelloStreamHost* host, uint32_t bitrate_kbps);

// ---- Stats ----

typedef struct MelloStreamStats {
    uint32_t bitrate_kbps;
    uint32_t fps_actual;
    uint32_t keyframes_sent;
    uint64_t bytes_sent;
    char     encoder_name[32];  // "NVENC", "AMF", "QSV-oneVPL"
    char     decoder_name[32];  // "NVDEC", "AMF-Decode", "OpenH264", "VideoToolbox"
} MelloStreamStats;

void mello_stream_get_stats(MelloStreamHost* host, MelloStreamStats* stats);

// ---- Cursor ----

/// Get latest cursor packet. Returns packet size, or 0 if no update since last call.
int mello_stream_get_cursor_packet(MelloStreamHost* host, uint8_t* buf, int buf_size);

/// Apply a received cursor packet on the viewer side.
MelloResult mello_stream_apply_cursor_packet(
    MelloStreamView* view,
    const uint8_t*   buf,
    int              size
);

/// Get current cursor state for the viewer render loop.
typedef struct MelloCursorState {
    int32_t  x;
    int32_t  y;
    bool     visible;
    uint8_t* shape_rgba;    // Null if no custom shape (use system default)
    uint32_t shape_w;
    uint32_t shape_h;
} MelloCursorState;

void mello_stream_get_cursor_state(MelloStreamView* view, MelloCursorState* out);
```

---

## 11. Platform Future-Proofing

The `GraphicsDevice` abstraction (§2.1) is the primary future-proofing mechanism. All encoder, decoder, and capture interfaces take `const GraphicsDevice&` and cast the handle internally. No interface change is needed when adding Apple platform support.

Expected Apple implementations:
- **Capture:** ScreenCaptureKit (`SCStream`) — per-window or per-display, produces `CMSampleBuffer` with `IOSurface`
- **Encode/Decode:** VideoToolbox (`VTCompressionSession` / `VTDecompressionSession`) — H.264 and HEVC hardware, AV1 decode on Apple silicon
- **Color conversion:** Metal compute shader, mirrors the D3D11 CS approach
- **Staging:** `IOSurface` mapped to CPU, equivalent to the D3D11 staging texture

When implementing macOS support, add `GraphicsBackend::Metal` to `GraphicsDevice` and implement platform-specific versions of `CaptureSource`, `Encoder`, `Decoder`, and `StagingTexture`. The `VideoPipeline` class and all code above it (mello-core, mello-client) require no changes.

---

## 12. File Structure

```
libmello/src/video/
├── graphics_device.hpp / .cpp      # Shared D3D11 device, GraphicsDevice type
├── video_pipeline.hpp / .cpp       # Top-level orchestrator
├── capture_source.hpp              # Abstract capture interface + factory
├── capture_dxgi.hpp / .cpp         # DXGI Desktop Duplication backend
├── capture_wgc.hpp / .cpp          # Windows Graphics Capture backend
├── capture_process.hpp / .cpp      # Process mode: auto-selects + hot-swaps
├── process_enum.hpp / .cpp         # Game process enumeration
├── video_preprocessor.hpp / .cpp   # GPU BGRA→NV12 + downscale (D3D11 Video Processor)
├── encoder.hpp                     # Abstract encoder interface
├── encoder_factory.hpp / .cpp      # Probe + instantiate best encoder
├── encoder_nvenc.hpp / .cpp        # NVIDIA NVENC
├── encoder_amf.hpp / .cpp          # AMD AMF
├── encoder_qsv.hpp / .cpp          # Intel oneVPL
│   (no software encoder)           # HW required; see §6.7
├── decoder.hpp                     # Abstract decoder interface
├── decoder_factory.hpp / .cpp      # Probe + instantiate best decoder
├── decoder_nvdec.hpp / .cpp        # NVIDIA NVDEC
├── decoder_amf.hpp / .cpp          # AMD AMF decode
├── decoder_openh264.hpp / .cpp     # OpenH264 (H.264) software fallback
├── video_viewer.hpp / .cpp         # Viewer: decode thread, 3-slot ring, present thread
├── staging_texture.hpp / .cpp      # Converter: picture → keyed-mutex shared RGBA
└── cursor.hpp / .cpp               # Cursor packet encode/decode
```

---

## 13. Logging Guidelines

These guidelines describe what to log and when within libmello's video pipeline. No new logging infrastructure is required — use the existing libmello log callback (registered at `mello_init()` time) and emit at the appropriate level. mello-core bridges all libmello log output into its own `tracing` subscriber (see `15-DEBUG-TELEMETRY.md`).

### Log Levels

| Level | When to use |
|---|---|
| `ERROR` | Unrecoverable failure — pipeline cannot continue without intervention |
| `WARN` | Unexpected but recoverable event — pipeline continues but something changed |
| `INFO` | Normal lifecycle events — start, stop, configuration, selection |
| `DEBUG` | Diagnostic detail useful during development — periodic stats, probe results |

### 13.1 D3D11 Device Initialisation

**Level:** `INFO` on success, `ERROR` on failure.

Log the selected GPU adapter name, available VRAM (dedicated and shared), and D3D11 feature level. If `create_d3d11_device()` fails, log the HRESULT error code and a human-readable description. Example output:

```
[video/device] D3D11 device created: adapter="NVIDIA GeForce RTX 4070" vram=8176MB feature_level=D3D_FEATURE_LEVEL_11_1
[video/device] ERROR: D3D11 device creation failed: hr=0x887A0004 (DXGI_ERROR_DRIVER_INTERNAL_ERROR)
```

### 13.2 Encoder and Decoder Probing

**Level:** `DEBUG` for each candidate probed, `INFO` for the final selection, `WARN` if falling back to software.

The factory probes candidates in priority order. Log each probe attempt and its outcome so it is clear why a particular encoder/decoder was or was not selected.

```
[video/encoder] Probing NVENC... ok
[video/encoder] Selected encoder: NVENC codec=H264 resolution=1920x1080 fps=60 bitrate=12000kbps
```

```
[video/encoder] Probing NVENC... not available (no NVIDIA GPU)
[video/encoder] Probing AMF... not available (AMD driver not found)
[video/encoder] Probing QSV... not available (oneVPL runtime missing)
[video/encoder] ERROR: No hardware encoder found — streaming unavailable on this machine
```

Same pattern applies to `DecoderFactory`.

### 13.3 Capture Backend Selection

**Level:** `INFO` on selection, `DEBUG` for detection reasoning.

For `Monitor` and `Window` modes the backend is fixed — log the selection and source description. For `Process` mode, log the detection result that drove the decision.

```
[video/capture] Source: Monitor(0) backend=DXGI-DDI resolution=2560x1440
[video/capture] Source: Process(pid=18432 "Minecraft") exclusive_fullscreen=false → backend=WGC hwnd=0x000A01C0
[video/capture] Source: Process(pid=9812 "FortniteClient") exclusive_fullscreen=true output=0 → backend=DXGI-DDI
```

### 13.4 Capture Backend Hot-swap

**Level:** `WARN` — this is unexpected mid-session and always worth flagging.

Log the old backend, new backend, and the reason (fullscreen gained or lost).

```
[video/capture] WARN: Hot-swap triggered for pid=9812 — exclusive_fullscreen gained → switching WGC → DXGI-DDI
[video/capture] WARN: Hot-swap triggered for pid=9812 — exclusive_fullscreen lost → switching DXGI-DDI → WGC
[video/capture] Hot-swap complete, keyframe requested
```

### 13.5 Pipeline Start and Stop

**Level:** `INFO`

Log the full effective configuration on start so it is unambiguous what the pipeline is running with. Log elapsed time on stop.

```
[video/pipeline] Host pipeline starting: encoder=NVENC decoder=none codec=H264 capture=WGC res=1920x1080 fps=60 bitrate=12000kbps low_latency=true
[video/pipeline] Host pipeline stopped: uptime=142s frames_encoded=8520 keyframes=4 bytes_out=213MB
[video/pipeline] Viewer pipeline starting: decoder=NVDEC codec=H264 res=1920x1080
[video/pipeline] Viewer pipeline stopped: uptime=140s frames_decoded=8390 frames_dropped=12 bytes_in=211MB
```

### 13.6 Encode and Decode Errors

**Level:** `ERROR` always — these should never be silently swallowed.

Log the SDK-specific error code and, where available, a description. Include the frame sequence number so errors can be correlated with packet logs in mello-core.

```
[video/encoder] ERROR: NVENC encode failed: NV_ENC_ERR_INVALID_PARAM (seq=14302)
[video/decoder] ERROR: NVDEC decode failed: CUDA_ERROR_INVALID_VALUE (seq=9871 keyframe=false)
[video/decoder] ERROR: AMF decode: SubmitInput failed: 9 (seq=2201)
```

### 13.7 Keyframe Events

**Level:** `DEBUG`

Log every keyframe encode/request with the reason. This is invaluable for understanding loss recovery behaviour.

```
[video/encoder] Keyframe encoded (reason=scheduled interval seq=3600)
[video/encoder] Keyframe encoded (reason=viewer_joined viewer_id=abc123 seq=3714)
[video/encoder] Keyframe encoded (reason=loss_recovery seq=4100)
```

### 13.8 Periodic Stats (Hot Path)

**Level:** `DEBUG`

Emitted every 300 frames (~5 seconds at 60fps) from the host encoder and viewer decoder. These are the primary tool for monitoring pipeline health during development. Do not log more frequently than this on the hot path.

Host:
```
[video/stats] enc=NVENC fps=59.8 bitrate=11840kbps keyframes=2 frames=300 bytes=24.6MB
```

Viewer:
```
[video/stats] dec=NVDEC fps=59.6 frames=300 dropped=1 staging_copy_avg=0.4ms bytes=24.4MB
```

### 13.9 Staging Texture

**Level:** `WARN` if `Map()` stalls beyond 2ms (indicates GPU pipeline pressure).

Normal operation is silent. Only log when something is wrong.

```
[video/staging] WARN: Map() stall 4.2ms — possible GPU pipeline pressure (frame seq=7823)
```


## Teardown never waits without a bound

`stop_host` calls into a capture backend, a graphics driver and a hardware
encoder. Each of them can block for as long as it likes, and each has:

| Step | Bound | What happens when it passes |
|---|---|---|
| Capture stop | 5 s | The capture thread is detached and the backend is leaked, never destroyed |
| Encode thread join | 5 s | The thread is left running, and its encoder, preprocessor and frames are leaked |

A leak is the cheap outcome. Destroying an encoder or a texture under a thread
that is still inside the driver is a crash, and holding the teardown is a frozen
client: on 2026-09-15 a stuck capture stop took the command loop with it, and
voice, the END STREAM button and hangup all died with it. On 2026-09-16 a
stop sat in the encode-thread join for 25 minutes.

Both leaks are counted and reported in host stats, so a machine that hits them
is visible rather than merely slow.
