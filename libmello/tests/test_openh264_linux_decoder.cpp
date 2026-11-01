// The Linux OpenH264 decoder (decoder_openh264_linux.cpp): an encode and a
// decode through the same library, into the viewer's output slots. Skips
// where no OpenH264 with soname 8 is installed (the CI image has soname 6).
#if defined(__linux__)
#include "video/decoder_openh264.hpp"
#include "video/decoder_nvdec.hpp"
#include "video/decoder_gst_va.hpp"
#include "video/openh264_dynload.hpp"
#include "video/graphics_device.hpp"
#include <gtest/gtest.h>
#include <cstdlib>
#include <cstring>
#include <vector>

#include <stdbool.h>
namespace mello::video::wels_abi {
#include "wels/codec_api.h"
}

namespace mello::video {
namespace {

namespace wa = wels_abi;

// One IDR access unit of a flat picture, from the OpenH264 encoder.
std::vector<uint8_t> encode_flat(uint32_t w, uint32_t h, uint8_t y, uint8_t u, uint8_t v) {
    std::vector<uint8_t> out;
    void* raw = nullptr;
    if (openh264::encoder_api().create_encoder(&raw) != 0 || !raw) return out;
    auto* enc = static_cast<wa::ISVCEncoder*>(raw);

    wa::SEncParamBase param{};
    param.iUsageType = wa::CAMERA_VIDEO_REAL_TIME;
    param.iPicWidth = static_cast<int>(w);
    param.iPicHeight = static_cast<int>(h);
    param.iTargetBitrate = 500000;
    param.fMaxFrameRate = 30.0f;
    if (enc->Initialize(&param) == 0) {
        std::vector<uint8_t> py(static_cast<size_t>(w) * h, y);
        std::vector<uint8_t> pu(static_cast<size_t>(w / 2) * (h / 2), u);
        std::vector<uint8_t> pv(static_cast<size_t>(w / 2) * (h / 2), v);
        wa::SSourcePicture pic{};
        pic.iColorFormat = wa::videoFormatI420;
        pic.iPicWidth = static_cast<int>(w);
        pic.iPicHeight = static_cast<int>(h);
        pic.iStride[0] = static_cast<int>(w);
        pic.iStride[1] = static_cast<int>(w / 2);
        pic.iStride[2] = static_cast<int>(w / 2);
        pic.pData[0] = py.data();
        pic.pData[1] = pu.data();
        pic.pData[2] = pv.data();
        wa::SFrameBSInfo info{};
        if (enc->EncodeFrame(&pic, &info) == 0) {
            for (int l = 0; l < info.iLayerNum; ++l) {
                const wa::SLayerBSInfo& layer = info.sLayerInfo[l];
                size_t bytes = 0;
                for (int n = 0; n < layer.iNalCount; ++n) bytes += static_cast<size_t>(layer.pNalLengthInByte[n]);
                out.insert(out.end(), layer.pBsBuf, layer.pBsBuf + bytes);
            }
        }
        enc->Uninitialize();
    }
    openh264::encoder_api().destroy_encoder(enc);
    return out;
}

class OpenH264LinuxDecoder : public ::testing::Test {
protected:
    void SetUp() override {
        if (!OpenH264Decoder::is_available()) GTEST_SKIP() << "no OpenH264 (soname 8)";
        if (!openh264::encoder_api_loaded()) GTEST_SKIP() << "OpenH264 without an encoder";
    }
};

TEST_F(OpenH264LinuxDecoder, APictureLandsInTheChosenSlotAtTheStreamSize) {
    // 36 rows: the encoder crops a 48-row coded picture, the decoder must not
    // show the padding.
    const uint32_t w = 64, h = 36;
    const auto au = encode_flat(w, h, 120, 90, 160);
    ASSERT_FALSE(au.empty());

    OpenH264Decoder dec;
    ASSERT_TRUE(dec.initialize(create_cpu_device(), DecoderConfig{1280, 720}));
    ASSERT_EQ(dec.decode(au.data(), au.size(), true, 2), DecodeFeedResult::FrameReady);

    CpuPicture pic{};
    EXPECT_FALSE(dec.picture(0, pic));
    ASSERT_TRUE(dec.picture(2, pic));
    EXPECT_EQ(pic.width, w);
    EXPECT_EQ(pic.height, h);
    // A flat picture decodes close to its own values.
    EXPECT_NEAR(pic.y[0], 120, 3);
    EXPECT_NEAR(pic.y[(h - 1) * pic.y_stride + (w - 1)], 120, 3);
    EXPECT_NEAR(pic.u[0], 90, 3);
    EXPECT_NEAR(pic.v[(h / 2 - 1) * pic.uv_stride + (w / 2 - 1)], 160, 3);
    dec.shutdown();
}

TEST_F(OpenH264LinuxDecoder, ANewDecodeLeavesTheOtherSlotsAlone) {
    const auto dark = encode_flat(64, 64, 40, 128, 128);
    const auto light = encode_flat(64, 64, 200, 128, 128);
    ASSERT_FALSE(dark.empty());
    ASSERT_FALSE(light.empty());

    OpenH264Decoder dec;
    ASSERT_TRUE(dec.initialize(create_cpu_device(), DecoderConfig{64, 64}));
    ASSERT_EQ(dec.decode(dark.data(), dark.size(), true, 0), DecodeFeedResult::FrameReady);
    ASSERT_EQ(dec.decode(light.data(), light.size(), true, 1), DecodeFeedResult::FrameReady);

    CpuPicture first{}, second{};
    ASSERT_TRUE(dec.picture(0, first));
    ASSERT_TRUE(dec.picture(1, second));
    EXPECT_NEAR(first.y[0], 40, 3);
    EXPECT_NEAR(second.y[0], 200, 3);
    dec.shutdown();
}

TEST_F(OpenH264LinuxDecoder, ASlotOutsideTheRingIsAnError) {
    OpenH264Decoder dec;
    ASSERT_TRUE(dec.initialize(create_cpu_device(), DecoderConfig{64, 64}));
    const uint8_t junk[4] = {0, 0, 0, 1};
    EXPECT_EQ(dec.decode(junk, sizeof(junk), false, Decoder::kOutputSlots), DecodeFeedResult::Error);
    CpuPicture pic{};
    EXPECT_FALSE(dec.picture(-1, pic));
    dec.shutdown();
}

// The hardware decoders take the same stream: one IDR of a flat picture at
// a size that is not a multiple of 16, so the crop must be right too.
void expect_flat_decode(Decoder& dec) {
    if (!openh264::load() || !openh264::encoder_api_loaded()) GTEST_SKIP() << "no OpenH264 encoder";
    const uint32_t w = 128, h = 72;
    const auto au = encode_flat(w, h, 150, 100, 170);
    ASSERT_FALSE(au.empty());
    if (!dec.initialize(create_cpu_device(), DecoderConfig{w, h})) GTEST_SKIP() << dec.name() << " not here";
    DecodeFeedResult r = dec.decode(au.data(), au.size(), true, 1);
    // A pipelined decoder may give the picture on the next call.
    for (int i = 0; i < 5 && r == DecodeFeedResult::Accepted; ++i) r = dec.decode(au.data(), au.size(), true, 1);
    ASSERT_EQ(r, DecodeFeedResult::FrameReady) << dec.name();
    CpuPicture pic{};
    ASSERT_TRUE(dec.picture(1, pic));
    EXPECT_EQ(pic.format, CpuPictureFormat::NV12);
    EXPECT_EQ(pic.width, w);
    EXPECT_EQ(pic.height, h);
    EXPECT_NEAR(pic.y[0], 150, 3);
    EXPECT_NEAR(pic.y[(h - 1) * pic.y_stride + (w - 1)], 150, 3);
    EXPECT_NEAR(pic.u[0], 100, 3);   // U
    EXPECT_NEAR(pic.u[1], 170, 3);   // V
    EXPECT_NEAR(pic.u[(h / 2 - 1) * pic.uv_stride + (w - 2)], 100, 3);
    dec.shutdown();
}

TEST(LinuxHardwareDecoder, NvdecDecodesTheOpenH264Stream) {
    if (!NvdecDecoder::is_available()) GTEST_SKIP() << "no NVIDIA driver";
    NvdecDecoder dec;
    expect_flat_decode(dec);
}

TEST(LinuxHardwareDecoder, VaApiDecodesTheOpenH264Stream) {
    if (!GstVaDecoder::is_available()) GTEST_SKIP() << "no GStreamer VA decoder";
    GstVaDecoder dec;
    expect_flat_decode(dec);
}

} // namespace
} // namespace mello::video
#endif
