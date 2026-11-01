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

// A w x h picture of one RGB color, as PipeWire delivers it.
std::vector<uint8_t> rgbx_solid(uint32_t w, uint32_t h, CpuPixelFormat f, uint8_t r, uint8_t g, uint8_t b) {
    std::vector<uint8_t> px(static_cast<size_t>(w) * h * 4);
    for (size_t i = 0; i < px.size(); i += 4) {
        px[i + 0] = f == CpuPixelFormat::BGRx ? b : r;
        px[i + 1] = g;
        px[i + 2] = f == CpuPixelFormat::BGRx ? r : b;
        px[i + 3] = 0xAB;  // ignored
    }
    return px;
}

struct I420 {
    std::vector<uint8_t> y, u, v;
    I420(uint32_t w, uint32_t h) : y(static_cast<size_t>(w) * h), u(y.size() / 4), v(y.size() / 4) {}
};

TEST(RgbxToI420, WhiteBlackAndRedGiveStudioSwingBt709) {
    struct Case { uint8_t r, g, b, y, u, v; };
    // Y, U, V of BT.709 studio swing, within one step of rounding.
    for (const Case c : {Case{255, 255, 255, 235, 128, 128}, Case{0, 0, 0, 16, 128, 128},
                         Case{255, 0, 0, 63, 102, 240}}) {
        auto px = rgbx_solid(4, 2, CpuPixelFormat::BGRx, c.r, c.g, c.b);
        CpuFrame f{CpuPixelFormat::BGRx, px.data(), 16, 4, 2};
        I420 out(4, 2);
        rgbx_to_i420(f, 4, 2, out.y.data(), out.u.data(), out.v.data());
        EXPECT_NEAR(out.y[0], c.y, 1) << int(c.r) << "," << int(c.g) << "," << int(c.b);
        EXPECT_NEAR(out.u[0], c.u, 1);
        EXPECT_NEAR(out.v[0], c.v, 1);
    }
}

TEST(RgbxToI420, BothByteOrdersGiveTheSamePicture) {
    auto bgr = rgbx_solid(4, 4, CpuPixelFormat::BGRx, 200, 40, 90);
    auto rgb = rgbx_solid(4, 4, CpuPixelFormat::RGBx, 200, 40, 90);
    I420 a(4, 4), b(4, 4);
    rgbx_to_i420(CpuFrame{CpuPixelFormat::BGRx, bgr.data(), 16, 4, 4}, 4, 4, a.y.data(), a.u.data(), a.v.data());
    rgbx_to_i420(CpuFrame{CpuPixelFormat::RGBx, rgb.data(), 16, 4, 4}, 4, 4, b.y.data(), b.u.data(), b.v.data());
    EXPECT_EQ(a.y, b.y);
    EXPECT_EQ(a.u, b.u);
    EXPECT_EQ(a.v, b.v);
}

// A 8x4 picture, left half black and right half white, scaled to 4x2:
// the halves stay where they were.
TEST(RgbxToI420, ADownscaleKeepsThePictureInPlace) {
    const uint32_t w = 8, h = 4, stride = w * 4 + 16;
    std::vector<uint8_t> px(static_cast<size_t>(stride) * h, 0);
    for (uint32_t y = 0; y < h; ++y) {
        for (uint32_t x = w / 2; x < w; ++x) {
            uint8_t* p = &px[y * stride + x * 4];
            p[0] = p[1] = p[2] = 255;
        }
    }
    I420 out(4, 2);
    rgbx_to_i420(CpuFrame{CpuPixelFormat::BGRx, px.data(), stride, w, h}, 4, 2, out.y.data(), out.u.data(),
                 out.v.data());
    EXPECT_EQ(out.y[0], 16);
    EXPECT_EQ(out.y[1], 16);
    EXPECT_EQ(out.y[2], 235);
    EXPECT_EQ(out.y[3], 235);
    EXPECT_EQ(out.y[4], 16);
    EXPECT_EQ(out.y[7], 235);
}

// The host's conversion and the viewer's conversion are inverses, within
// the rounding of two 8-bit steps.
TEST(RgbxToI420, TheViewerGetsTheColorBack) {
    auto px = rgbx_solid(4, 4, CpuPixelFormat::RGBx, 30, 160, 220);
    I420 yuv(4, 4);
    rgbx_to_i420(CpuFrame{CpuPixelFormat::RGBx, px.data(), 16, 4, 4}, 4, 4, yuv.y.data(), yuv.u.data(),
                 yuv.v.data());
    CpuPicture pic{CpuPictureFormat::I420, yuv.y.data(), yuv.u.data(), yuv.v.data(), 4, 2, 4, 4};
    auto rgba = convert(pic);
    EXPECT_NEAR(rgba[0], 30, 3);
    EXPECT_NEAR(rgba[1], 160, 3);
    EXPECT_NEAR(rgba[2], 220, 3);
}

} // namespace
} // namespace mello::video
#endif
