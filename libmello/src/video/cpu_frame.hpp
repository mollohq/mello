#pragma once
#if defined(__linux__)
#include <cstdint>

namespace mello::video {

/// The byte order of a 32-bit pixel in a CpuFrame. The fourth byte is
/// ignored: PipeWire's BGRA and RGBA arrive as BGRx and RGBx.
enum class CpuPixelFormat {
    BGRx,  ///< B, G, R, x in memory (SPA_VIDEO_FORMAT_BGRx / BGRA)
    RGBx,  ///< R, G, B, x in memory (SPA_VIDEO_FORMAT_RGBx / RGBA)
};

/// One captured picture in CPU memory (Linux: a PipeWire buffer). The pixels
/// belong to the capture and are valid only during the frame callback.
struct CpuFrame {
    CpuPixelFormat format = CpuPixelFormat::BGRx;
    const uint8_t* data = nullptr;
    uint32_t stride = 0;  ///< bytes per row
    uint32_t width = 0;
    uint32_t height = 0;
};

} // namespace mello::video
#endif
