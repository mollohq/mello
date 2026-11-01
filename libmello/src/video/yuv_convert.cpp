#if defined(__linux__)
#include "yuv_convert.hpp"

#include <vector>

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

// BT.709 studio swing, the coefficients times 256:
//   Y = 16  + ( 47 R + 157 G +  16 B) / 256
//   U = 128 + (-26 R -  86 G + 112 B) / 256
//   V = 128 + (112 R - 102 G -  10 B) / 256
void rgbx_to_i420(const CaptureFrame& src, uint32_t dst_w, uint32_t dst_h, uint8_t* y, uint8_t* u, uint8_t* v) {
    const bool bgr = src.format == CpuPixelFormat::BGRx;
    const int ri = bgr ? 2 : 0;
    const int bi = bgr ? 0 : 2;
    const bool same = src.width == dst_w && src.height == dst_h;
    // Source column of each destination column, once per call.
    thread_local std::vector<uint32_t> col_of;
    col_of.resize(dst_w);
    for (uint32_t x = 0; x < dst_w; ++x) {
        col_of[x] = same ? x : static_cast<uint32_t>((static_cast<uint64_t>(x) * src.width + src.width / 2) / dst_w);
        if (col_of[x] >= src.width) col_of[x] = src.width - 1;
    }
    auto src_row = [&](uint32_t row) {
        uint32_t sr = same ? row : static_cast<uint32_t>((static_cast<uint64_t>(row) * src.height + src.height / 2) / dst_h);
        if (sr >= src.height) sr = src.height - 1;
        return src.data + static_cast<size_t>(sr) * src.stride;
    };
    for (uint32_t row = 0; row < dst_h; row += 2) {
        const uint8_t* s0 = src_row(row);
        const uint8_t* s1 = src_row(row + 1);
        uint8_t* y0 = y + static_cast<size_t>(row) * dst_w;
        uint8_t* y1 = y0 + dst_w;
        uint8_t* ur = u + static_cast<size_t>(row / 2) * (dst_w / 2);
        uint8_t* vr = v + static_cast<size_t>(row / 2) * (dst_w / 2);
        for (uint32_t x = 0; x < dst_w; x += 2) {
            int rs = 0, gs = 0, bs = 0;
            for (int k = 0; k < 4; ++k) {
                const uint8_t* line = (k < 2) ? s0 : s1;
                const uint8_t* px = line + static_cast<size_t>(col_of[x + (k & 1)]) * 4;
                const int r = px[ri], g = px[1], b = px[bi];
                (k < 2 ? y0 : y1)[x + (k & 1)] = static_cast<uint8_t>(16 + ((47 * r + 157 * g + 16 * b + 128) >> 8));
                rs += r;
                gs += g;
                bs += b;
            }
            ur[x / 2] = clamp_u8(128 + ((-26 * rs - 86 * gs + 112 * bs + 512) >> 10));
            vr[x / 2] = clamp_u8(128 + ((112 * rs - 102 * gs - 10 * bs + 512) >> 10));
        }
    }
}

void rgbx_to_nv12(const CaptureFrame& src, uint32_t dst_w, uint32_t dst_h, uint8_t* nv12) {
    const size_t luma = static_cast<size_t>(dst_w) * dst_h;
    thread_local std::vector<uint8_t> u, v;
    u.resize(luma / 4);
    v.resize(luma / 4);
    rgbx_to_i420(src, dst_w, dst_h, nv12, u.data(), v.data());
    uint8_t* uv = nv12 + luma;
    for (size_t i = 0; i < luma / 4; ++i) {
        uv[2 * i] = u[i];
        uv[2 * i + 1] = v[i];
    }
}

} // namespace mello::video
#endif
