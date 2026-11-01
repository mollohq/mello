// The Linux OpenH264 decoder (decoder_openh264_linux.cpp): an encode and a
// decode through the same library, into the viewer's output slots. Skips
// where no OpenH264 with soname 8 is installed (the CI image has soname 6).
#if defined(__linux__)
#include "video/decoder_openh264.hpp"
#include "video/decoder_nvdec.hpp"
#include "video/decoder_gst_va.hpp"
#include "video/encoder_gst_va.hpp"
#include "video/openh264_dynload.hpp"
#include "video/graphics_device.hpp"
#include <gtest/gtest.h>
#include <cstdlib>
#include <chrono>
#include <cstring>
#include <thread>
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

// The VA-API encoder's stream decodes: a flat grey BGRx picture, scaled
// from 640x360 to 320x180 on the GPU, comes back at the encode size and
// close to its grey, from the first access unit (a key frame).
TEST(LinuxHardwareEncoder, VaApiEncodesAStreamOpenH264Reads) {
    if (!GstVaEncoder::is_available()) GTEST_SKIP() << "no GStreamer VA encoder";
    if (!OpenH264Decoder::is_available()) GTEST_SKIP() << "no OpenH264 (soname 8)";
    GstVaEncoder enc;
    EncoderConfig cfg{};
    cfg.width = 320;
    cfg.height = 180;
    cfg.fps = 60;
    cfg.bitrate_kbps = 2000;
    cfg.keyframe_interval = 60;
    if (!enc.initialize(create_cpu_device(), cfg)) GTEST_SKIP() << "VA encoder does not start here";

    std::vector<uint8_t> bgrx(640 * 360 * 4);
    for (size_t i = 0; i < bgrx.size(); i += 4) {
        bgrx[i] = bgrx[i + 1] = bgrx[i + 2] = 128;
    }
    CpuFrame frame{CpuPixelFormat::BGRx, bgrx.data(), 640 * 4, 640, 360};
    std::vector<EncodedPacket> aus;
    for (int i = 0; i < 10; ++i) {
        EncodedPacket au;
        if (enc.encode(frame, 1000 + static_cast<uint64_t>(i) * 16667, au)) aus.push_back(std::move(au));
    }
    // One picture stays in flight; drain gives it once the GPU is done.
    for (int tries = 0; tries < 50 && aus.size() < 10; ++tries) {
        EncodedPacket au;
        if (enc.drain(au)) {
            aus.push_back(std::move(au));
        } else {
            std::this_thread::sleep_for(std::chrono::milliseconds(5));
        }
    }
    enc.shutdown();
    // Every picture gives exactly one access unit, in capture order.
    ASSERT_EQ(aus.size(), 10u) << "the encoder lost or held back access units";
    for (size_t i = 1; i < aus.size(); ++i) EXPECT_GT(aus[i].timestamp_us, aus[i - 1].timestamp_us);
    EXPECT_TRUE(aus.front().is_keyframe);

    // The NAL unit types of each access unit, for a failure message.
    std::string nals;
    for (size_t i = 0; i < aus.size() && i < 3; ++i) {
        const auto& d = aus[i].data;
        nals += "AU" + std::to_string(i) + "(" + std::to_string(d.size()) + "B):";
        for (size_t k = 0; k + 3 < d.size(); ++k) {
            if (d[k] == 0 && d[k + 1] == 0 && d[k + 2] == 1) nals += " " + std::to_string(d[k + 3] & 0x1f);
        }
        nals += "; ";
    }

    // The access unit after which each decoder shows its first picture.
    auto first_picture_at = [&](Decoder& d) {
        if (!d.initialize(create_cpu_device(), DecoderConfig{320, 180})) return -2;
        for (size_t i = 0; i < aus.size(); ++i) {
            if (d.decode(aus[i].data.data(), aus[i].data.size(), aus[i].is_keyframe, 0) == DecodeFeedResult::FrameReady) {
                return static_cast<int>(i);
            }
        }
        return -1;
    };
    GstVaDecoder va;
    const int va_at = GstVaDecoder::is_available() ? first_picture_at(va) : -3;
    va.shutdown();
    OpenH264Decoder dec;
    const int oh_at = first_picture_at(dec);
    // The VA decoder shows the key frame at once. OpenH264 shows it one unit
    // later: the stream does not tell it that no frame is reordered (known
    // gap, plans/linux-client.md), which costs its viewers one frame.
    if (va_at != -3) EXPECT_EQ(va_at, 0) << nals;
    ASSERT_GE(oh_at, 0) << nals;
    EXPECT_LE(oh_at, 1) << nals;
    CpuPicture pic{};
    ASSERT_TRUE(dec.picture(0, pic));
    EXPECT_EQ(pic.width, 320u);
    EXPECT_EQ(pic.height, 180u);
    // RGB 128 grey is Y 126 in BT.709 studio swing.
    EXPECT_NEAR(pic.y[90 * pic.y_stride + 160], 126, 6);
    dec.shutdown();
}

} // namespace
} // namespace mello::video
#endif
