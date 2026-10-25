// The colour converter and scaler between capture and encode.
//
// The encode size is fixed for a stream. The source size is not: a window is
// resized, a display changes mode, the capture ladder moves to a method of
// another size. The whole new source must scale into the encode size with its
// aspect kept. A fixed source rectangle cropped a larger source to its old
// size (scorecard 4.1).
//
// The pure rectangle rule runs everywhere. The GPU tests need a D3D11 video
// processor, so they skip under CI like the other GPU tests in this suite.

#include <gtest/gtest.h>

#ifdef _WIN32

#include <windows.h>

#include <d3d11.h>
#include <wrl/client.h>

#include <cstdlib>
#include <string>
#include <vector>

#include "video/cursor.hpp"
#include "video/graphics_device.hpp"
#include "video/video_preprocessor.hpp"

using namespace mello::video;
using Microsoft::WRL::ComPtr;

namespace {

bool running_under_ci() {
    const char* ci = std::getenv("CI");
    return ci && *ci && std::string(ci) != "0" && std::string(ci) != "false";
}

// A BGRA texture as a capture backend hands it over: render target and shader
// resource, default usage.
ComPtr<ID3D11Texture2D> make_source(ID3D11Device* device, uint32_t width, uint32_t height,
                                    const std::vector<uint8_t>& bgra) {
    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = width;
    desc.Height = height;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    D3D11_SUBRESOURCE_DATA data{};
    data.pSysMem = bgra.data();
    data.SysMemPitch = width * 4;
    ComPtr<ID3D11Texture2D> texture;
    if (FAILED(device->CreateTexture2D(&desc, &data, &texture))) return nullptr;
    return texture;
}

// A picture of one grey, with a white band over its right tenth: the band is
// what a crop to the old, smaller size loses.
std::vector<uint8_t> grey_with_white_right_edge(uint32_t width, uint32_t height) {
    std::vector<uint8_t> pixels(static_cast<size_t>(width) * height * 4);
    for (uint32_t y = 0; y < height; ++y) {
        for (uint32_t x = 0; x < width; ++x) {
            const uint8_t v = x >= width - width / 10 ? 255 : 128;
            uint8_t* p = &pixels[(static_cast<size_t>(y) * width + x) * 4];
            p[0] = p[1] = p[2] = v;
            p[3] = 255;
        }
    }
    return pixels;
}

// The luma plane of an NV12 texture, read back through a staging copy.
struct Luma {
    uint32_t             width = 0;
    uint32_t             height = 0;
    std::vector<uint8_t> values;
    uint8_t at(uint32_t x, uint32_t y) const { return values[static_cast<size_t>(y) * width + x]; }
};

bool read_luma(ID3D11Device* device, ID3D11DeviceContext* context, ID3D11Texture2D* nv12,
               Luma* out) {
    D3D11_TEXTURE2D_DESC desc{};
    nv12->GetDesc(&desc);
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    desc.MiscFlags = 0;
    ComPtr<ID3D11Texture2D> staging;
    if (FAILED(device->CreateTexture2D(&desc, nullptr, &staging))) return false;
    context->CopyResource(staging.Get(), nv12);
    D3D11_MAPPED_SUBRESOURCE mapped{};
    if (FAILED(context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped))) return false;
    out->width = desc.Width;
    out->height = desc.Height;
    out->values.resize(static_cast<size_t>(desc.Width) * desc.Height);
    for (uint32_t y = 0; y < desc.Height; ++y) {
        memcpy(&out->values[static_cast<size_t>(y) * desc.Width],
               static_cast<const uint8_t*>(mapped.pData) + static_cast<size_t>(y) * mapped.RowPitch,
               desc.Width);
    }
    context->Unmap(staging.Get(), 0);
    return true;
}

// Studio-range luma of a grey level, BT.709.
int luma_of(int grey) { return 16 + (219 * grey + 127) / 255; }

}  // namespace

// --- The rectangle rule (no GPU) -------------------------------------------

TEST(VideoPipelineFit, TheSameAspectFillsTheOutput) {
    const FitRect r = VideoPreprocessor::fit(2560, 1440, 1280, 720);
    EXPECT_EQ(r.x, 0u);
    EXPECT_EQ(r.y, 0u);
    EXPECT_EQ(r.width, 1280u);
    EXPECT_EQ(r.height, 720u);
}

// encode_size_for rounds the width to a multiple of 16, so the encode aspect is
// a little off the source aspect. 3440x1440 gives 2288x960: a 0.2 % stretch is
// invisible, a black line above and below is not.
TEST(VideoPipelineFit, TheEncodeSizeRoundingIsNotLetterboxed) {
    const FitRect r = VideoPreprocessor::fit(3440, 1440, 2288, 960);
    EXPECT_EQ(r.width, 2288u);
    EXPECT_EQ(r.height, 960u);
}

TEST(VideoPipelineFit, ANarrowerSourceGetsBarsLeftAndRight) {
    // 4:3 into 16:9: full height, centred, even coordinates.
    const FitRect r = VideoPreprocessor::fit(960, 720, 320, 180);
    EXPECT_EQ(r.height, 180u);
    EXPECT_EQ(r.width, 240u);
    EXPECT_EQ(r.x, 40u);
    EXPECT_EQ(r.y, 0u);
}

TEST(VideoPipelineFit, AWiderSourceGetsBarsAboveAndBelow) {
    // 21:9 into 16:9.
    const FitRect r = VideoPreprocessor::fit(3440, 1440, 1280, 720);
    EXPECT_EQ(r.width, 1280u);
    EXPECT_EQ(r.height, 534u);
    EXPECT_EQ(r.x, 0u);
    EXPECT_EQ(r.y % 2, 0u);
    EXPECT_LE(r.y + r.height, 720u);
    EXPECT_GE(r.y, 90u);
}

// --- The scaler on the GPU -------------------------------------------------

class VideoPipelinePreprocessor : public ::testing::Test {
protected:
    GraphicsDevice                device{};
    ComPtr<ID3D11DeviceContext>   context;

    void SetUp() override {
        if (running_under_ci()) GTEST_SKIP() << "needs a GPU";
        device = create_d3d11_device();
        if (!device.d3d11()) GTEST_SKIP() << "no D3D11 device";
        device.d3d11()->GetImmediateContext(&context);
    }
    void TearDown() override {
        context.Reset();
        if (device.d3d11()) device.d3d11()->Release();
    }
};

// A window grew mid-stream. The preprocessor started at 640x360 into 320x180;
// a 1280x720 source must arrive whole, white right edge included.
TEST_F(VideoPipelinePreprocessor, ALargerSourceArrivesWholeAtTheEncodeSize) {
    VideoPreprocessor pre;
    ASSERT_TRUE(pre.initialize(device, 640, 360, 320, 180));

    auto first = make_source(device.d3d11(), 640, 360, grey_with_white_right_edge(640, 360));
    ASSERT_TRUE(first);
    ConvertResult r = pre.convert(first.Get());
    ASSERT_NE(r.texture, nullptr);
    EXPECT_FALSE(r.source_resized);

    auto larger = make_source(device.d3d11(), 1280, 720, grey_with_white_right_edge(1280, 720));
    ASSERT_TRUE(larger);
    r = pre.convert(larger.Get());
    ASSERT_NE(r.texture, nullptr);
    EXPECT_TRUE(r.source_resized);
    EXPECT_EQ(pre.source_width(), 1280u);

    Luma luma;
    ASSERT_TRUE(read_luma(device.d3d11(), context.Get(), r.texture, &luma));
    EXPECT_EQ(luma.width, 320u) << "the encode size changed";
    EXPECT_EQ(luma.height, 180u);
    // The white band covers x 288..319 of the output. A crop to the old
    // 640x360 source rectangle shows grey there.
    EXPECT_NEAR(luma.at(310, 90), luma_of(255), 4) << "the right edge of the source was cropped";
    EXPECT_NEAR(luma.at(100, 90), luma_of(128), 4);
}

// A source of another aspect keeps it: black bars, no stretch, no crop.
TEST_F(VideoPipelinePreprocessor, ASourceOfAnotherAspectIsPillarboxed) {
    VideoPreprocessor pre;
    ASSERT_TRUE(pre.initialize(device, 640, 360, 320, 180));

    auto first = make_source(device.d3d11(), 640, 360, grey_with_white_right_edge(640, 360));
    ASSERT_TRUE(first);
    ASSERT_NE(pre.convert(first.Get()).texture, nullptr);

    // 4:3 into 16:9: the picture is 240 wide at x 40..279.
    auto narrow = make_source(device.d3d11(), 960, 720, grey_with_white_right_edge(960, 720));
    ASSERT_TRUE(narrow);
    const ConvertResult r = pre.convert(narrow.Get());
    ASSERT_NE(r.texture, nullptr);

    Luma luma;
    ASSERT_TRUE(read_luma(device.d3d11(), context.Get(), r.texture, &luma));
    EXPECT_NEAR(luma.at(10, 90), 16, 3) << "no black bar on the left";
    EXPECT_NEAR(luma.at(310, 90), 16, 3) << "no black bar on the right";
    EXPECT_NEAR(luma.at(120, 90), luma_of(128), 4) << "the picture is not in the middle";
    EXPECT_NEAR(luma.at(272, 90), luma_of(255), 4) << "the right edge of the source was lost";
}

// --- The pointer -----------------------------------------------------------

// Desktop duplication gives the pointer apart from the desktop image, and
// nothing drew it: viewers of a screen share saw no pointer (scorecard 4.1).
// A white 16x16 pointer at (100, 50) over a black frame must land in the luma
// plane at that place, and nowhere else.
TEST_F(VideoPipelinePreprocessor, APointerIsDrawnIntoTheFrameAtItsPlace) {
    VideoPreprocessor pre;
    ASSERT_TRUE(pre.initialize(device, 256, 256, 256, 256));

    std::vector<uint8_t> black(256 * 256 * 4, 0);
    for (size_t i = 3; i < black.size(); i += 4) black[i] = 255;
    auto source = make_source(device.d3d11(), 256, 256, black);
    ASSERT_TRUE(source);

    // Opaque white, with a transparent last column: the alpha must hold.
    std::vector<uint8_t> shape(16 * 16 * 4, 255);
    for (uint32_t y = 0; y < 16; ++y) shape[(y * 16 + 15) * 4 + 3] = 0;
    CursorOverlay cursor{};
    cursor.x = 100;
    cursor.y = 50;
    cursor.width = 16;
    cursor.height = 16;
    cursor.bgra = shape.data();
    cursor.serial = 1;

    const ConvertResult r = pre.convert(source.Get(), &cursor);
    ASSERT_NE(r.texture, nullptr);
    Luma luma;
    ASSERT_TRUE(read_luma(device.d3d11(), context.Get(), r.texture, &luma));
    EXPECT_NEAR(luma.at(107, 57), 235, 3) << "the pointer is not in the frame";
    EXPECT_NEAR(luma.at(100, 50), 235, 3) << "the pointer's top-left corner is off";
    EXPECT_NEAR(luma.at(114, 65), 235, 3) << "the pointer's bottom-right is off";
    EXPECT_NEAR(luma.at(115, 57), 16, 3) << "a transparent pointer pixel was drawn";
    EXPECT_NEAR(luma.at(99, 57), 16, 3);
    EXPECT_NEAR(luma.at(107, 66), 16, 3);
    EXPECT_NEAR(luma.at(10, 10), 16, 3);

    // The capture's own texture is never written: the next frame from the
    // same duplication surface must not carry an old pointer.
    const ConvertResult plain = pre.convert(source.Get());
    ASSERT_NE(plain.texture, nullptr);
    ASSERT_TRUE(read_luma(device.d3d11(), context.Get(), plain.texture, &luma));
    EXPECT_NEAR(luma.at(107, 57), 16, 3) << "the pointer was drawn into the capture's texture";
}

// A pointer half outside the frame is clipped, not dropped.
TEST_F(VideoPipelinePreprocessor, APointerAtTheEdgeIsClipped) {
    VideoPreprocessor pre;
    ASSERT_TRUE(pre.initialize(device, 256, 256, 256, 256));
    std::vector<uint8_t> black(256 * 256 * 4, 0);
    auto source = make_source(device.d3d11(), 256, 256, black);
    ASSERT_TRUE(source);
    std::vector<uint8_t> shape(16 * 16 * 4, 255);
    CursorOverlay cursor{};
    cursor.x = 248;
    cursor.y = -8;
    cursor.width = 16;
    cursor.height = 16;
    cursor.bgra = shape.data();
    cursor.serial = 7;
    const ConvertResult r = pre.convert(source.Get(), &cursor);
    ASSERT_NE(r.texture, nullptr);
    Luma luma;
    ASSERT_TRUE(read_luma(device.d3d11(), context.Get(), r.texture, &luma));
    EXPECT_NEAR(luma.at(252, 4), 235, 3);
    EXPECT_NEAR(luma.at(244, 4), 16, 3);
}

// The three shape kinds of desktop duplication, as straight-alpha BGRA.
TEST(VideoPipelineCursorShape, AMonochromePointerKeepsItsMasks) {
    // 8x1: AND 1 XOR 0 transparent; AND 0 XOR 0 black; AND 0 XOR 1 white;
    // AND 1 XOR 1 inverts, shown black.
    const uint8_t data[] = {
        0b10010000,  // AND row
        0b00110000,  // XOR row
    };
    std::vector<uint8_t> out;
    ASSERT_TRUE(pointer_shape_to_bgra(PointerShapeKind::Monochrome, 4, 1, 1, data, sizeof(data), out));
    ASSERT_EQ(out.size(), 16u);
    EXPECT_EQ(out[3], 0) << "AND 1, XOR 0 is transparent";
    EXPECT_EQ(out[4 + 3], 255);
    EXPECT_EQ(out[4 + 0], 0) << "AND 0, XOR 0 is black";
    EXPECT_EQ(out[8 + 0], 255) << "AND 0, XOR 1 is white";
    EXPECT_EQ(out[12 + 0], 0) << "AND 1, XOR 1 inverts: shown black";
    EXPECT_EQ(out[12 + 3], 255);
}

TEST(VideoPipelineCursorShape, AMaskedColourPointerReplacesOrInverts) {
    const uint8_t data[] = {
        10, 20, 30, 0,      // mask 0: replaces, opaque
        0, 0, 0, 255,       // XOR with 0: transparent
        200, 200, 200, 255  // XOR with a colour: shown black
    };
    std::vector<uint8_t> out;
    ASSERT_TRUE(pointer_shape_to_bgra(PointerShapeKind::MaskedColor, 3, 1, 12, data, sizeof(data), out));
    EXPECT_EQ(out[0], 10);
    EXPECT_EQ(out[2], 30);
    EXPECT_EQ(out[3], 255);
    EXPECT_EQ(out[4 + 3], 0);
    EXPECT_EQ(out[8 + 0], 0);
    EXPECT_EQ(out[8 + 3], 255);
}

TEST(VideoPipelineCursorShape, AScaledPointerKeepsItsTransparentEdge) {
    // 2x1: opaque red, transparent black. Halved, the colour stays red.
    const std::vector<uint8_t> src = {0, 0, 255, 255, 0, 0, 0, 0};
    const std::vector<uint8_t> out = scale_bgra(src, 2, 1, 1, 1);
    ASSERT_EQ(out.size(), 4u);
    EXPECT_EQ(out[2], 255) << "the transparent pixel darkened the colour";
    EXPECT_NEAR(out[3], 128, 2);
}

#endif  // _WIN32
