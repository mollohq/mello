#pragma once
#include "graphics_device.hpp"
#include <cstdint>
#include <functional>
#include <memory>
#include <string>
#include <vector>

#ifdef _WIN32
#include <d3d11.h>
#endif

namespace mello::video {

enum class CaptureMode {
    Monitor,
    Window,
    Process,
};

struct CaptureSourceDesc {
    CaptureMode mode;
    union {
        uint32_t monitor_index;
        void*    hwnd;
        uint32_t pid;
    };
};

struct CursorData {
    int32_t  x = 0;
    int32_t  y = 0;
    bool     visible = true;
    bool     shape_changed = false;
    uint16_t shape_w = 0;
    uint16_t shape_h = 0;
    std::vector<uint8_t> shape_rgba;
};

class CaptureSource {
public:
#ifdef _WIN32
    using FrameCallback = std::function<void(ID3D11Texture2D* texture, uint64_t timestamp_us)>;
#else
    using FrameCallback = std::function<void(void* texture, uint64_t timestamp_us)>;
#endif

    virtual ~CaptureSource() = default;

    virtual bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) = 0;
    virtual bool start(uint32_t target_fps, FrameCallback callback) = 0;
    virtual void stop() = 0;

    virtual uint32_t width()  const = 0;
    virtual uint32_t height() const = 0;
    virtual const char* backend_name() const = 0;

    virtual bool get_cursor(CursorData& out) { (void)out; return false; }
    // Backends with runtime source/backend switching can raise a swap event.
    // The pipeline consumes this to force a keyframe and accelerate recovery.
    virtual bool consume_swap_event() { return false; }
};

std::unique_ptr<CaptureSource> create_capture_source(const CaptureSourceDesc& desc);

} // namespace mello::video
