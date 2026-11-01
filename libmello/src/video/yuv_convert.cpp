#if defined(__linux__)
#include "yuv_convert.hpp"

namespace mello::video {

static inline uint8_t clamp_u8(int v) {
    return static_cast<uint8_t>(v < 0 ? 0 : (v > 255 ? 255 : v));
}

void i420_to_rgba(const CpuPicture& pic, uint8_t* rgba) {
    const uint32_t w = pic.width;
    const uint32_t h = pic.height;
    for (uint32_t row = 0; row < h; ++row) {
        const uint8_t* y_row = pic.y + static_cast<size_t>(row) * pic.y_stride;
        const uint8_t* u_row = pic.u + static_cast<size_t>(row / 2) * pic.uv_stride;
        const uint8_t* v_row = pic.v + static_cast<size_t>(row / 2) * pic.uv_stride;
        uint8_t* out = rgba + static_cast<size_t>(row) * w * 4;
        for (uint32_t col = 0; col < w; ++col) {
            const int c = static_cast<int>(y_row[col]) - 16;
            const int d = static_cast<int>(u_row[col / 2]) - 128;
            const int e = static_cast<int>(v_row[col / 2]) - 128;
            const int luma = 298 * c + 128;
            out[0] = clamp_u8((luma + 459 * e) >> 8);
            out[1] = clamp_u8((luma - 55 * d - 136 * e) >> 8);
            out[2] = clamp_u8((luma + 541 * d) >> 8);
            out[3] = 255;
            out += 4;
        }
    }
}

} // namespace mello::video
#endif
