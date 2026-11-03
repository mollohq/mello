#pragma once
#include <cstdint>
#include <optional>

namespace mello::video {

/// A rectangle in the pixels of a captured picture.
struct CropRect {
    uint32_t x = 0;
    uint32_t y = 0;
    uint32_t width = 0;
    uint32_t height = 0;
};

/// The client area of a window inside its WGC window picture. WGC captures
/// the window's visible frame (DWM's extended frame bounds): a windowed game
/// comes with its title bar and its borders. `frame_left`, `frame_top` is
/// that frame and `client_left`, `client_top` the client area's origin, both
/// in screen pixels; `content_*` is the size of the captured picture.
///
/// Returns the client area in picture pixels. Returns nothing when the
/// picture is the client area already (a borderless or fullscreen game), or
/// when the client area does not lie inside the picture (a minimized window,
/// a size that changed between the reads): the picture then stays whole.
inline std::optional<CropRect> client_crop(int32_t frame_left, int32_t frame_top,
                                           int32_t client_left, int32_t client_top,
                                           uint32_t client_width, uint32_t client_height,
                                           uint32_t content_width, uint32_t content_height) {
    if (client_width == 0 || client_height == 0) return std::nullopt;
    const int64_t x = static_cast<int64_t>(client_left) - frame_left;
    const int64_t y = static_cast<int64_t>(client_top) - frame_top;
    if (x < 0 || y < 0 || x + client_width > content_width || y + client_height > content_height) {
        return std::nullopt;
    }
    if (x == 0 && y == 0 && client_width == content_width && client_height == content_height) {
        return std::nullopt;
    }
    return CropRect{static_cast<uint32_t>(x), static_cast<uint32_t>(y), client_width, client_height};
}

}  // namespace mello::video
