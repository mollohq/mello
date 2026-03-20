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

**Viewer pipeline:**
```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│ Packet Queue │────▶│   HW Decode  │────▶│ Staging Tex  │────▶│  Slint RGBA  │
│              │     │NVDEC/AMF/    │     │  Map() →CPU  │     │   (one copy) │
│ From network │     │  D3D11VA     │     │              │     │   To UI      │
└──────────────┘     └──────────────┘     └──────────────┘     └──────────────┘
```

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
    Monitor,    // Full display output   → DXGI DDI backend
    Window,     // Specific HWND         → WGC backend
    Process,    // Game / app by PID     → WGC (or DXGI DDI if exclusive fullscreen)
};

struct CaptureSourceDesc {
    CaptureMode mode;
    union {
        uint32_t monitor_index; // Monitor mode: 0 = primary
        void*    hwnd;          // Window mode: HWND
        uint32_t pid;           // Process mode: process ID
    };
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
};

/// Factory — selects DXGI or WGC based on desc and runtime conditions.
std::unique_ptr<CaptureSource> create_capture_source(const CaptureSourceDesc& desc);

} // namespace mello::video
```

### 4.2 Backend Selection Rules

| Capture mode | Condition | Backend |
|---|---|---|
| `Monitor` | Always | DXGI DDI |
| `Window` | Always | WGC |
| `Process` | Target process owns a DXGI output (exclusive fullscreen) | DXGI DDI on that output |
| `Process` | Otherwise (windowed / borderless) | WGC on process's main HWND |

Backend selection for `Process` mode is performed at `start()` time and re-evaluated periodically (see §4.5 Hot-swap).

### 4.3 DXGI Desktop Duplication Backend

Used for monitor capture and exclusive fullscreen games. Acquires frames directly from the DXGI output without going through DWM — lowest possible capture latency.

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

**Cursor handling in DXGI:** `IDXGIOutputDuplication::AcquireNextFrame()` returns cursor shape and position separately from the desktop texture in `DXGI_OUTDUPL_FRAME_INFO`. The cursor data is extracted and forwarded as a cursor packet (§6) — it is never composited into the captured texture.

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

**WGC and cursor:** WGC composites the cursor into the captured texture by default. To match the DXGI path (cursor as separate channel), WGC capture must disable cursor capture via `GraphicsCaptureSession::IsCursorCaptureEnabled = false` and read cursor position via `GetCursorInfo()` instead.

### 4.5 Exclusive Fullscreen Detection and Hot-swap

For `Process` mode, the capture backend must automatically switch when a game transitions between windowed and exclusive fullscreen (common during game startup sequences).

```cpp
// src/video/capture_source.cpp (factory + hot-swap logic)

namespace mello::video {

/// Check whether a process currently owns a DXGI output (exclusive fullscreen).
/// Returns the adapter/output index if true, -1 if windowed.
static int query_exclusive_fullscreen_output(uint32_t pid);

class ProcessCapture : public CaptureSource {
public:
    bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) override;
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width()  const override;
    uint32_t height() const override;
    const char* backend_name() const override;

private:
    void monitor_thread();  // Polls for fullscreen transition every 500ms
    void swap_to_dxgi();
    void swap_to_wgc();

    uint32_t                         pid_;
    GraphicsDevice                   device_;
    FrameCallback                    callback_;
    uint32_t                         target_fps_ = 60;

    std::unique_ptr<CaptureSource>   active_;    // Current backend (DXGI or WGC)
    std::thread                      monitor_thread_;
    std::atomic<bool>                running_{false};
};

} // namespace mello::video
```

Hot-swap sequence:
1. `monitor_thread` detects fullscreen state change.
2. Calls `active_->stop()` on the current backend.
3. Instantiates the new backend, calls `initialize()` and `start()` with the same `callback_`.
4. Swap is seamless from the pipeline's perspective — `on_packet` callbacks resume on the new backend within one frame interval.
5. A keyframe is requested immediately after swap so the viewer can resync.

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
    std::string name;           // e.g. "Minecraft"
    std::string exe;            // e.g. "javaw.exe"
    bool        is_fullscreen;  // Currently in exclusive fullscreen
};

/// Returns running processes that match the known game list.
/// The known list is a bundled JSON file: assets/games.json
std::vector<GameProcess> enumerate_game_processes();

} // namespace mello::video
```

`assets/games.json` format:
```json
[
  { "name": "Minecraft",        "exe": "javaw.exe"        },
  { "name": "Fortnite",         "exe": "FortniteClient-Win64-Shipping.exe" },
  { "name": "League of Legends","exe": "League of Legends.exe" }
]
```

The list is bundled with the client and updated via the auto-updater. Processes not on the list are not shown. A user can also pick "Share a window" (any visible HWND, not filtered) or "Share a monitor" as alternatives.

---

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

In `VideoPipeline::start_host()`, the target encode resolution is determined from `PipelineConfig::width/height` (passed from the Rust preset). If the config resolution is smaller than the capture resolution, the pipeline uses the config resolution for encoding; otherwise it uses the capture resolution. All dimensions are even-aligned for NV12 compatibility.

This reduces encoded pixel count significantly (e.g. 2036×1392 → 1920×1080 = 27% fewer pixels), improving quality per bit and lowering bandwidth.

---

## 6. Encoding

### 6.1 Codec Configuration

**Primary codec: H.264** — universal hardware support across all target GPU vendors.

Low-latency encode profile (mandatory for all hardware encoders):
- No B-frames (`num_b_frames = 0`)
- Rate control: VBR with moderate headroom (`max = avg × 1.25`, `vbv = avg × 1`)
- Keyframe interval: 120 frames (2 seconds at 60fps) under normal conditions
- Look-ahead: disabled

The VBR headroom of 1.25× allows keyframes slightly more bits without large bandwidth spikes. The tight VBV (1× average bitrate) keeps rate control smooth for P2P links. This applies to both initial configuration and dynamic `set_bitrate()` reconfiguration.

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
    uint32_t   keyframe_interval = 120;
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

/// Priority order: NVENC → AMF → QSV (oneVPL)
/// Probes each in order; returns first that initialises successfully.
/// If none are available, returns Err — streaming requires hardware encode.
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

### 6.7 No Software Encoder

There is no software encoder fallback. Streaming requires a hardware encoder. This is an intentional product and licensing decision — GPL-licensed software encoders (x264, x265) are incompatible with Mello's Apache 2.0 licence, and AV1 software encoders are too slow for real-time use.

If `EncoderFactory` exhausts all hardware candidates without success, it returns `Err`. The stream cannot start and the user is shown a clear error message. In practice this affects only machines with no GPU at all — any system with NVIDIA, AMD, or Intel graphics from the last decade has hardware H.264 encode capability.

---

## 7. Decoding

### 7.1 Abstract Decoder Interface

```cpp
// src/video/decoder.hpp

#pragma once
#include "graphics_device.hpp"
#include <d3d11.h>
#include <cstdint>

namespace mello::video {

struct DecoderConfig {
    uint32_t   width;
    uint32_t   height;
    VideoCodec codec = VideoCodec::H264;
};

class Decoder {
public:
    virtual ~Decoder() = default;

    virtual bool initialize(const GraphicsDevice& device, const DecoderConfig& config) = 0;
    virtual void shutdown() = 0;

    /// Feed an encoded packet. Returns true if a frame is ready (call get_frame).
    virtual bool decode(const uint8_t* data, size_t size, bool is_keyframe) = 0;

    /// Get decoded frame as NV12 texture in VRAM.
    /// Valid until the next call to decode().
    virtual ID3D11Texture2D* get_frame() = 0;

    virtual bool        supports_codec(VideoCodec codec) const = 0;
    virtual const char* name() const = 0;
};

} // namespace mello::video
```

### 7.2 DecoderFactory

```cpp
// src/video/decoder_factory.hpp

#pragma once
#include "decoder.hpp"
#include <memory>
#include <vector>

namespace mello::video {

/// Priority order: NVDEC → AMF → D3D11VA → OpenH264 (H.264 SW) / dav1d (AV1 SW)
std::unique_ptr<Decoder> create_best_decoder(
    const GraphicsDevice& device,
    const DecoderConfig&  config
);

std::vector<const char*> enumerate_decoders(const GraphicsDevice& device);

} // namespace mello::video
```

### 7.3 NVDEC Decoder

Uses NVIDIA Video Codec SDK decode API. Outputs decoded NV12 frames as `ID3D11Texture2D` on the shared device.

```cpp
// src/video/decoder_nvdec.hpp

#pragma once
#include "decoder.hpp"
#include <nvcuvid.h>

namespace mello::video {

class NvdecDecoder : public Decoder {
public:
    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    bool             decode(const uint8_t* data, size_t size, bool is_keyframe) override;
    ID3D11Texture2D* get_frame() override;
    bool             supports_codec(VideoCodec codec) const override;
    const char*      name() const override { return "NVDEC"; }

    static bool is_available();

private:
    CUvideodecoder   decoder_    = nullptr;
    CUvideoparser    parser_     = nullptr;

    Microsoft::WRL::ComPtr<ID3D11Texture2D> frame_tex_;
};

} // namespace mello::video
```

### 7.4 AMF Decoder

Uses AMD AMF decode component. Outputs `AMFSurface` with `AMF_SURFACE_DX11` memory — unwrapped to `ID3D11Texture2D` for the pipeline.

```cpp
// src/video/decoder_amf.hpp

#pragma once
#include "decoder.hpp"
#include <AMF/components/VideoDecoderUVD.h>

namespace mello::video {

class AmfDecoder : public Decoder {
public:
    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    bool             decode(const uint8_t* data, size_t size, bool is_keyframe) override;
    ID3D11Texture2D* get_frame() override;
    bool             supports_codec(VideoCodec codec) const override;
    const char*      name() const override { return "AMF-Decode"; }

    static bool is_available();

private:
    amf::AMFContextPtr   context_  = nullptr;
    amf::AMFComponentPtr decoder_  = nullptr;
};

} // namespace mello::video
```

### 7.5 D3D11VA Decoder (Intel + Generic Hardware Fallback)

D3D11VA is the Windows hardware decode API and works across all GPU vendors. This is the correct path for Intel iGPU decoding (rather than oneVPL, which adds unnecessary complexity on the decode side). It also serves as the generic hardware fallback for any GPU not covered by NVDEC or AMF.

```cpp
// src/video/decoder_d3d11va.hpp

#pragma once
#include "decoder.hpp"
#include <d3d11.h>
#include <wrl/client.h>

namespace mello::video {

class D3D11vaDecoder : public Decoder {
public:
    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    bool             decode(const uint8_t* data, size_t size, bool is_keyframe) override;
    ID3D11Texture2D* get_frame() override;
    bool             supports_codec(VideoCodec codec) const override;
    const char*      name() const override { return "D3D11VA"; }

    static bool is_available();

private:
    Microsoft::WRL::ComPtr<ID3D11VideoDevice>        video_device_;
    Microsoft::WRL::ComPtr<ID3D11VideoContext>        video_context_;
    Microsoft::WRL::ComPtr<ID3D11VideoDecoder>        decoder_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>           frame_tex_;
};

} // namespace mello::video
```

### 7.6 Software Decoders (OpenH264 + dav1d)

Two software decode libraries cover the fallback path, one per codec. Both are BSD-licensed and compatible with Mello's Apache 2.0 licence. FFmpeg is not used.

**OpenH264** (Cisco, BSD 2-clause) — H.264 software decode. Cisco distributes pre-built binaries and covers MPEG-LA patent licensing for those binaries. Mello must distribute the official Cisco-built `openh264.dll` rather than building from source to remain within the patent covenant.

**dav1d** (VideoLAN, BSD 2-clause) — AV1 software decode. No patent concerns. Statically linkable or distributed as a DLL. Fastest available AV1 software decoder; real-time 1080p is achievable on modest hardware.

Both decoders output a CPU-side frame buffer (I420/YUV420P) which is converted to RGBA and uploaded to a staging texture for the Slint handoff — the same CPU copy path used by all software decoders. The codec field in `DecoderConfig` determines which library is instantiated.

```cpp
// src/video/decoder_software.hpp

#pragma once
#include "decoder.hpp"
#include <wrl/client.h>

// OpenH264
#include <wels/codec_api.h>

// dav1d
#include <dav1d/dav1d.h>

namespace mello::video {

/// Software decoder — OpenH264 for H.264, dav1d for AV1.
/// Instantiated by DecoderFactory only when all hardware decoders fail.
class SoftwareDecoder : public Decoder {
public:
    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    bool             decode(const uint8_t* data, size_t size, bool is_keyframe) override;
    ID3D11Texture2D* get_frame() override;
    bool             supports_codec(VideoCodec codec) const override { return true; }
    const char*      name() const override;   // "OpenH264" or "dav1d" depending on codec

private:
    VideoCodec codec_ = VideoCodec::H264;

    // OpenH264 (H.264 path)
    ISVCDecoder* oh264_decoder_ = nullptr;

    // dav1d (AV1 path)
    Dav1dContext*   dav1d_ctx_   = nullptr;
    Dav1dPicture    dav1d_pic_{};

    // CPU→GPU upload for Slint handoff
    Microsoft::WRL::ComPtr<ID3D11Texture2D>      upload_tex_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext>   context_;
};

} // namespace mello::video
```

**Distribution:**
- `openh264.dll` — must be the official Cisco pre-built binary (for patent coverage). Distributed alongside `mello.exe`.
- `dav1d.dll` — can be built from source via vcpkg (`x64-windows` triplet) or statically linked (`x64-windows-static`). No patent obligations.

---

## 8. Staging Texture (Viewer → Slint Handoff)

The one unavoidable copy in the viewer pipeline: decoded NV12 frame in VRAM → CPU-accessible RGBA buffer for Slint. A staging texture is used instead of `Map()`-ing the decoder surface directly, which would stall the GPU pipeline.

```cpp
// src/video/staging_texture.hpp

#pragma once
#include "graphics_device.hpp"
#include <d3d11.h>
#include <wrl/client.h>
#include <cstdint>
#include <vector>

namespace mello::video {

class StagingTexture {
public:
    bool initialize(const GraphicsDevice& device, uint32_t width, uint32_t height);

    /// Async GPU copy: decoded NV12 VRAM surface → staging texture.
    /// Non-blocking — returns immediately.
    void copy_from(ID3D11Texture2D* nv12_source);

    /// Map staging texture and convert NV12 → RGBA into `out`.
    /// Blocks until the async copy above is complete (typically 0–1ms).
    /// `out` must be pre-allocated: width * height * 4 bytes.
    void read_rgba(uint8_t* out);

    void shutdown();

private:
    Microsoft::WRL::ComPtr<ID3D11Device>        device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D>     staging_; // CPU_READ, STAGING usage

    uint32_t width_  = 0;
    uint32_t height_ = 0;
};

} // namespace mello::video
```

`copy_from()` issues a `CopyResource()` call (GPU → GPU, async). `read_rgba()` calls `Map()` on the staging texture, performs NV12→RGBA conversion in a tight CPU loop, then `Unmap()`. The result is passed to mello-core's `FrameCallback`, which wraps it in a `SharedPixelBuffer` for Slint as shown in `01-CLIENT.md §6.3`.

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
    char     decoder_name[32];  // "NVDEC", "AMF-Decode", "D3D11VA", "OpenH264", "dav1d"
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
├── decoder_d3d11va.hpp / .cpp      # D3D11VA (Intel + generic HW)
├── decoder_software.hpp / .cpp     # OpenH264 (H.264) + dav1d (AV1) fallback
├── staging_texture.hpp / .cpp      # VRAM→CPU handoff for Slint
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
[video/decoder] ERROR: D3D11VA decode failed: hr=0x88960003 (seq=2201)
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
