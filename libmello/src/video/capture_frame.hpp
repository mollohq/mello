#pragma once
#if defined(__linux__)
#include <cstddef>
#include <cstdint>
#include <memory>
#include <vector>

namespace mello::video {

/// The byte order of a 32-bit pixel in a CaptureFrame. The fourth byte is
/// ignored: PipeWire's BGRA and RGBA arrive as BGRx and RGBx.
enum class CpuPixelFormat {
    BGRx,  ///< B, G, R, x in memory (SPA_VIDEO_FORMAT_BGRx / BGRA; DRM XR24)
    RGBx,  ///< R, G, B, x in memory (SPA_VIDEO_FORMAT_RGBx / RGBA; DRM XB24)
};

/// A DMA-BUF layout an encoder can import: a DRM fourcc and its modifiers.
struct DmaBufFormat {
    uint32_t drm_fourcc = 0;
    std::vector<uint64_t> modifiers;
};

/// Held by whoever still reads a DMA-BUF frame. The capture gets its buffer
/// back when the last holder drops it, from any thread.
struct FrameRelease {
    virtual ~FrameRelease() = default;
};

/// One captured picture (Linux: a PipeWire buffer). Either in CPU memory
/// (`data`), valid only during the frame callback, or a DMA-BUF in GPU
/// memory (`dmabuf_fd` >= 0, `data` null): the GPU reads it in place, and
/// the capture reuses the buffer once every copy of `release` is gone.
struct CaptureFrame {
    CpuPixelFormat format = CpuPixelFormat::BGRx;
    const uint8_t* data = nullptr;
    uint32_t stride = 0;  ///< bytes per row (both kinds)
    uint32_t width = 0;
    uint32_t height = 0;

    int dmabuf_fd = -1;
    uint32_t dmabuf_offset = 0;
    size_t dmabuf_size = 0;
    /// DMA-BUF: the whole buffer; `width` x `height` is the visible part at
    /// (`crop_x`, `crop_y`), a window inside a larger buffer.
    uint32_t buffer_width = 0;
    uint32_t buffer_height = 0;
    uint32_t crop_x = 0;
    uint32_t crop_y = 0;
    uint32_t drm_fourcc = 0;
    uint64_t modifier = 0;
    std::shared_ptr<FrameRelease> release;

    bool is_dmabuf() const { return dmabuf_fd >= 0; }
};

/// DRM fourcc codes for the formats above (drm_fourcc.h).
constexpr uint32_t drm_fourcc(char a, char b, char c, char d) {
    return static_cast<uint32_t>(a) | (static_cast<uint32_t>(b) << 8) | (static_cast<uint32_t>(c) << 16) |
           (static_cast<uint32_t>(d) << 24);
}
constexpr uint32_t kDrmXRGB8888 = drm_fourcc('X', 'R', '2', '4');  // BGRx in memory
constexpr uint32_t kDrmARGB8888 = drm_fourcc('A', 'R', '2', '4');  // BGRA in memory
constexpr uint32_t kDrmXBGR8888 = drm_fourcc('X', 'B', '2', '4');  // RGBx in memory
constexpr uint32_t kDrmABGR8888 = drm_fourcc('A', 'B', '2', '4');  // RGBA in memory

} // namespace mello::video
#endif
