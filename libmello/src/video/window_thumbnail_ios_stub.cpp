// iOS window-thumbnail capture — Step 1 stub (no hosting in v1).
#include "window_thumbnail.hpp"

namespace mello::video {

int capture_window_thumbnail(void* /*hwnd*/,
                             uint32_t /*max_width*/, uint32_t /*max_height*/,
                             uint8_t* /*rgba_out*/, uint32_t* /*out_width*/, uint32_t* /*out_height*/) {
    return -1;
}

} // namespace mello::video
