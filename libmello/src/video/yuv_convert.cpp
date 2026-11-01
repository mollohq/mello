#if defined(__linux__)
#include "yuv_convert.hpp"

namespace mello::video {

static inline uint8_t clamp_u8(int v) {
    return static_cast<uint8_t>(v < 0 ? 0 : (v > 255 ? 255 : v));
}

static inline void put_pixel(uint8_t* out, int y, int d, int e) {
    const int luma = 298 * (y - 16) + 128;
    out[0] = clamp_u8((luma + 459 * e) >> 8);
    out[1] = clamp_u8((luma - 55 * d - 136 * e) >> 8);
    out[2] = clamp_u8((luma + 541 * d) >> 8);
    out[3] = 255;
}

void i420_to_rgba(const CpuPicture& pic, uint8_t* rgba) {
    const uint32_t w = pic.width;
    const uint32_t h = pic.height;
    const bool nv12 = pic.format == CpuPictureFormat::NV12;
    for (uint32_t row = 0; row < h; ++row) {
        const uint8_t* y_row = pic.y + static_cast<size_t>(row) * pic.y_stride;
        const uint8_t* u_row = pic.u + static_cast<size_t>(row / 2) * pic.uv_stride;
        uint8_t* out = rgba + static_cast<size_t>(row) * w * 4;
        if (nv12) {
            for (uint32_t col = 0; col < w; ++col) {
                const uint8_t* uv = u_row + (col & ~1u);
                put_pixel(out, y_row[col], static_cast<int>(uv[0]) - 128, static_cast<int>(uv[1]) - 128);
                out += 4;
            }
        } else {
            const uint8_t* v_row = pic.v + static_cast<size_t>(row / 2) * pic.uv_stride;
            for (uint32_t col = 0; col < w; ++col) {
                put_pixel(out, y_row[col], static_cast<int>(u_row[col / 2]) - 128,
                          static_cast<int>(v_row[col / 2]) - 128);
                out += 4;
            }
        }
    }
}

} // namespace mello::video
#endif
