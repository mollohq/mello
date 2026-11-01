// The I420 to RGBA conversion of the Linux viewer (yuv_convert.cpp).
#if defined(__linux__)
#include "video/yuv_convert.hpp"
#include <gtest/gtest.h>
#include <vector>

namespace mello::video {
namespace {

// A picture of one color, with strides wider than the picture as OpenH264
// gives them.
struct Solid {
    std::vector<uint8_t> y, u, v;
    CpuPicture pic;
    Solid(uint32_t w, uint32_t h, uint8_t yv, uint8_t uv, uint8_t vv) {
        const uint32_t ys = w + 32, cs = w / 2 + 16;
        y.assign(static_cast<size_t>(ys) * h, yv);
        u.assign(static_cast<size_t>(cs) * (h / 2), uv);
        v.assign(static_cast<size_t>(cs) * (h / 2), vv);
        pic = CpuPicture{CpuPictureFormat::I420, y.data(), u.data(), v.data(), ys, cs, w, h};
    }
};

std::vector<uint8_t> convert(const CpuPicture& pic) {
    std::vector<uint8_t> out(static_cast<size_t>(pic.width) * pic.height * 4, 0);
    i420_to_rgba(pic, out.data());
    return out;
}

TEST(YuvConvert, StudioBlackAndWhiteAreFullRange) {
    auto black = convert(Solid(4, 2, 16, 128, 128).pic);
    EXPECT_EQ(black[0], 0);
    EXPECT_EQ(black[1], 0);
    EXPECT_EQ(black[2], 0);
    EXPECT_EQ(black[3], 255);
    auto white = convert(Solid(4, 2, 235, 128, 128).pic);
    EXPECT_EQ(white[0], 255);
    EXPECT_EQ(white[1], 255);
    EXPECT_EQ(white[2], 255);
}

// BT.709 studio red (Y 63, Cb 102, Cr 240) is close to full red. The
// expected values are the shader formula, rounded to nearest.
TEST(YuvConvert, RedUsesTheBt709Matrix) {
    auto px = convert(Solid(2, 2, 63, 102, 240).pic);
    // c = 47, d = -26, e = 112
    EXPECT_EQ(px[0], 255);                                  // (14006 + 51408 + 128) >> 8, clamped
    EXPECT_EQ(px[1], (298 * 47 + 55 * 26 - 136 * 112 + 128) >> 8);
    EXPECT_EQ(px[2], 0);                                    // negative, clamped
}

TEST(YuvConvert, EachTwoByTwoBlockSharesOneChromaSample) {
    Solid s(4, 4, 128, 128, 128);
    // Chroma sample (1, 1) covers pixels (2..3, 2..3).
    s.u[1 * s.pic.uv_stride + 1] = 200;
    auto px = convert(s.pic);
    auto blue = [&](uint32_t x, uint32_t y) { return px[(y * 4 + x) * 4 + 2]; };
    const uint8_t gray = blue(0, 0);
    EXPECT_EQ(blue(1, 1), gray);
    EXPECT_GT(blue(2, 2), gray);
    EXPECT_EQ(blue(3, 3), blue(2, 2));
    EXPECT_EQ(blue(2, 1), gray);
    EXPECT_EQ(blue(1, 2), gray);
}

// NV12 and I420 of the same picture convert to the same pixels.
TEST(YuvConvert, Nv12MatchesI420) {
    const uint32_t w = 4, h = 4, ys = 8, cs = 2, nv_stride = 6;
    std::vector<uint8_t> y(ys * h), u(cs * 2), v(cs * 2), uv(nv_stride * 2);
    for (size_t i = 0; i < y.size(); ++i) y[i] = static_cast<uint8_t>(20 + i * 7);
    for (uint32_t r = 0; r < 2; ++r) {
        for (uint32_t c = 0; c < 2; ++c) {
            const uint8_t uu = static_cast<uint8_t>(60 + r * 50 + c * 30);
            const uint8_t vv = static_cast<uint8_t>(200 - r * 40 - c * 25);
            u[r * cs + c] = uu;
            v[r * cs + c] = vv;
            uv[r * nv_stride + c * 2] = uu;
            uv[r * nv_stride + c * 2 + 1] = vv;
        }
    }
    CpuPicture i420{CpuPictureFormat::I420, y.data(), u.data(), v.data(), ys, cs, w, h};
    CpuPicture nv12{CpuPictureFormat::NV12, y.data(), uv.data(), nullptr, ys, nv_stride, w, h};
    EXPECT_EQ(convert(i420), convert(nv12));
}

// The interleaved plane is read as U then V, not V then U.
TEST(YuvConvert, Nv12ReadsUBeforeV) {
    std::vector<uint8_t> y(4, 126), uv(2);
    uv[0] = 240; // U high: blue
    uv[1] = 128;
    CpuPicture pic{CpuPictureFormat::NV12, y.data(), uv.data(), nullptr, 2, 2, 2, 2};
    auto px = convert(pic);
    EXPECT_GT(px[2], px[0]);
}

TEST(YuvConvert, TheStrideIsNotThePictureWidth) {
    Solid s(4, 2, 16, 128, 128);
    // Bytes past the visible width must not show.
    for (uint32_t x = 4; x < s.pic.y_stride; ++x) s.y[x] = 235;
    auto px = convert(s.pic);
    for (size_t i = 0; i < px.size(); i += 4) EXPECT_EQ(px[i], 0) << i;
}

} // namespace
} // namespace mello::video
#endif
