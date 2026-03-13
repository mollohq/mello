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
  ├── ColorConverter  ← initialised with &device
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
#include "color_converter.hpp"
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
    std::unique_ptr<ColorConverter>  converter_;
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
// src/video/color_converter.hpp

#pragma once
#include "graphics_device.hpp"
#include <d3d11.h>
#include <wrl/client.h>

using Microsoft::WRL::ComPtr;

namespace mello::video {

class ColorConverter {
public:
    ColorConverter();
    ~ColorConverter();

    bool initialize(const GraphicsDevice& device, uint32_t width, uint32_t height);

    /// Convert BGRA source texture to NV12.
    /// Output texture is owned by ColorConverter and reused across calls.
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

---

## 6. Encoding

### 6.1 Codec Configuration

**Primary codec: H.264** — universal hardware support across all target GPU vendors.

Low-latency encode profile (mandatory for all hardware encoders):
- No B-frames (`num_b_frames = 0`)
- Rate control: CBR
- VBV buffer: 1× target bitrate (1-second maximum)
- Keyframe interval: 120 frames (2 seconds at 60fps) under normal conditions
- Look-ahead: disabled

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

/// Priority order: NVENC → AMF → QSV (oneVPL) → x264
/// Probes each in order; returns first that initialises successfully.
std::unique_ptr<Encoder> create_best_encoder(
    const GraphicsDevice& device,
    const EncoderConfig&  config
);

/// Returns all encoder types available on this machine.
std::vector<const char*> enumerate_encoders(const GraphicsDevice& device);

} // namespace mello::video
```

When x264 (software) is selected, the pipeline caps config to 720p30 before passing it to the encoder, and sets `EncoderStats::name = "x264"` so mello-core can surface the UI warning.

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

### 6.7 x264 Software Encoder (Fallback)

No D3D11 interop. Requires a readback from VRAM to system memory — this is the one CPU copy in the host pipeline, and it only occurs when no hardware encoder is available. Input is converted to I420 (x264's native format) before encoding.

Capped at 720p30 by `EncoderFactory` before `initialize()` is called.

```cpp
// src/video/encoder_x264.hpp

#pragma once
#include "encoder.hpp"
#include <x264.h>

namespace mello::video {

class X264Encoder : public Encoder {
public:
    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(ID3D11Texture2D* nv12_texture, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override { return codec == VideoCodec::H264; }
    const char* name() const override { return "x264"; }

private:
    x264_t*         encoder_   = nullptr;
    x264_picture_t  pic_in_{};
    x264_picture_t  pic_out_{};

    // Staging buffer for VRAM→CPU readback (only path with a CPU copy)
    Microsoft::WRL::ComPtr<ID3D11Texture2D>    staging_tex_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;

    bool        force_idr_ = false;
    EncoderStats stats_{};
};

} // namespace mello::video
```

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

/// Priority order: NVDEC → AMF → D3D11VA → Software (FFmpeg)
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

### 7.6 Software Decoder (FFmpeg Fallback)

Last resort. FFmpeg `libavcodec` with H.264 software decode. Outputs `AVFrame` in YUV420P; converted to RGBA in CPU before upload to a staging texture for Slint.

Bundled with libmello. Should never be the active decoder on any modern machine.

```cpp
// src/video/decoder_software.hpp

#pragma once
#include "decoder.hpp"

extern "C" {
#include <libavcodec/avcodec.h>
#include <libswscale/swscale.h>
}

namespace mello::video {

class SoftwareDecoder : public Decoder {
public:
    bool             initialize(const GraphicsDevice& device, const DecoderConfig& config) override;
    void             shutdown() override;
    bool             decode(const uint8_t* data, size_t size, bool is_keyframe) override;
    ID3D11Texture2D* get_frame() override;
    bool             supports_codec(VideoCodec codec) const override { return true; }
    const char*      name() const override { return "SW-FFmpeg"; }

private:
    AVCodecContext*  codec_ctx_  = nullptr;
    AVFrame*         frame_      = nullptr;
    AVPacket*        packet_     = nullptr;
    SwsContext*      sws_ctx_    = nullptr;

    // Upload buffer: CPU RGBA → GPU texture for Slint
    Microsoft::WRL::ComPtr<ID3D11Texture2D>         upload_tex_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext>      context_;
};

} // namespace mello::video
```

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
    MELLO_ENCODER_X264   = 3,   // Software fallback
} MelloEncoderBackend;

typedef enum MelloDecoderBackend {
    MELLO_DECODER_NVDEC   = 0,
    MELLO_DECODER_AMF     = 1,
    MELLO_DECODER_D3D11VA = 2,
    MELLO_DECODER_SW      = 3,  // Software fallback
} MelloDecoderBackend;

/// Returns available encoder backends on this machine, in priority order.
int mello_get_encoders(MelloContext* ctx, MelloEncoderBackend* out, int max_count);

/// Returns available decoder backends on this machine, in priority order.
int mello_get_decoders(MelloContext* ctx, MelloDecoderBackend* out, int max_count);

/// Returns whether the active encoder is software (x264).
/// Used by mello-core to surface the UI warning.
bool mello_encoder_is_software(MelloContext* ctx);

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
    char     encoder_name[32];  // "NVENC", "AMF", "QSV-oneVPL", "x264"
    char     decoder_name[32];  // "NVDEC", "AMF-Decode", "D3D11VA", "SW-FFmpeg"
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
├── color_converter.hpp / .cpp      # GPU BGRA→NV12 compute shader
├── encoder.hpp                     # Abstract encoder interface
├── encoder_factory.hpp / .cpp      # Probe + instantiate best encoder
├── encoder_nvenc.hpp / .cpp        # NVIDIA NVENC
├── encoder_amf.hpp / .cpp          # AMD AMF
├── encoder_qsv.hpp / .cpp          # Intel oneVPL
├── encoder_x264.hpp / .cpp         # x264 software fallback
├── decoder.hpp                     # Abstract decoder interface
├── decoder_factory.hpp / .cpp      # Probe + instantiate best decoder
├── decoder_nvdec.hpp / .cpp        # NVIDIA NVDEC
├── decoder_amf.hpp / .cpp          # AMD AMF decode
├── decoder_d3d11va.hpp / .cpp      # D3D11VA (Intel + generic HW)
├── decoder_software.hpp / .cpp     # FFmpeg software fallback
├── staging_texture.hpp / .cpp      # VRAM→CPU handoff for Slint
└── cursor.hpp / .cpp               # Cursor packet encode/decode
```
