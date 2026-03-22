#pragma once
#include <cstdint>

namespace mello::video {

/// Capture a thumbnail of a window into RGBA pixels.
/// Returns 0 on success, -1 on failure.
int capture_window_thumbnail(
    void* hwnd,
    uint32_t max_width, uint32_t max_height,
    uint8_t* rgba_out, uint32_t* out_width, uint32_t* out_height
);

} // namespace mello::video
