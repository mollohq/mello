// The reference clip source and the software encoder of the streaming
// quality gate (plans/streaming-quality.md stage 1).
//
// The pure parts (clip size, schedule, I420 to NV12) run everywhere. The
// loopback runs the real pipeline: the reference source, the preprocessor,
// the OpenH264 encoder (MELLO_SOFT_ENCODER=1), the OpenH264 decoder
// (MELLO_SOFT_DECODER=1) and the CPU frame callback. It needs a D3D11 device
// with a video processor and the Cisco OpenH264 DLL, so it skips under CI
// like the other pipeline tests.
#include <gtest/gtest.h>
#include "video/capture_reference.hpp"
#include "video/encoder_factory.hpp"
#include "video/decoder_factory.hpp"
#include "video/video_pipeline.hpp"
#include "video/encoder_openh264.hpp"

#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <mutex>
#include <thread>
#include <vector>

#ifdef _WIN32
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#endif

using namespace mello::video;

TEST(ReferenceClip, FrameCountNeedsWholeFramesAndAnEvenSize) {
    EXPECT_EQ(reference_frame_bytes(1280, 720), 1382400u);
    EXPECT_EQ(reference_clip_frames(1382400ull * 1200, 1280, 720), 1200u);
    EXPECT_EQ(reference_clip_frames(1382400ull * 1200 + 1, 1280, 720), 0u);
    EXPECT_EQ(reference_clip_frames(0, 1280, 720), 0u);
    EXPECT_EQ(reference_clip_frames(1000, 0, 720), 0u);
    EXPECT_EQ(reference_clip_frames(reference_frame_bytes(1281, 720), 1281, 720), 0u);
}

TEST(ReferenceClip, DueTimesAreExactOverLoops) {
    // 60 fps: frame n is due at n * 1e6 / 60 us, with no drift after an hour.
    EXPECT_EQ(reference_due_us(1000, 0, 60), 1000u);
    EXPECT_EQ(reference_due_us(1000, 1, 60), 1000u + 16666u);
    EXPECT_EQ(reference_due_us(1000, 3, 60), 1000u + 50000u);
    EXPECT_EQ(reference_due_us(0, 216000, 60), 3600ull * 1000000ull);
}

TEST(ReferenceClip, I420PacksAsNv12) {
    const uint32_t w = 4, h = 2;
    std::vector<uint8_t> i420 = {
        1, 2, 3, 4, 5, 6, 7, 8,  // Y
        10, 11,                  // U
        20, 21,                  // V
    };
    std::vector<uint8_t> nv12(i420.size());
    reference_i420_to_nv12(i420.data(), w, h, nv12.data());
    const std::vector<uint8_t> want = {1, 2, 3, 4, 5, 6, 7, 8, 10, 20, 11, 21};
    EXPECT_EQ(nv12, want);
}

#ifdef _WIN32
namespace {

void set_env(const char* name, const char* value) {
    SetEnvironmentVariableA(name, value);
}

}  // namespace

TEST(SoftCodecSwitch, ReadsTheProcessEnvironment) {
    set_env("MELLO_SOFT_ENCODER", nullptr);
    set_env("MELLO_SOFT_DECODER", nullptr);
    EXPECT_FALSE(soft_encoder_requested());
    EXPECT_FALSE(soft_decoder_requested());
    set_env("MELLO_SOFT_ENCODER", "1");
    set_env("MELLO_SOFT_DECODER", "1");
    EXPECT_TRUE(soft_encoder_requested());
    EXPECT_TRUE(soft_decoder_requested());
    set_env("MELLO_SOFT_ENCODER", "0");
    EXPECT_FALSE(soft_encoder_requested());
    set_env("MELLO_SOFT_ENCODER", nullptr);
    set_env("MELLO_SOFT_DECODER", nullptr);
}

TEST(ReferenceLoopback, SoftwareEncodeAndDecodeKeepTheClip) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    if (!OpenH264Encoder::is_available()) GTEST_SKIP() << "OpenH264 DLL with encoder not found";

    // A 320x240 clip of 60 frames: a horizontal luma ramp that moves one
    // pixel per frame, neutral chroma.
    const uint32_t w = 320, h = 240, frames = 60;
    const auto path = std::filesystem::temp_directory_path() / "mello_reference_test.yuv";
    {
        std::vector<uint8_t> frame(reference_frame_bytes(w, h), 128);
        FILE* f = std::fopen(path.string().c_str(), "wb");
        ASSERT_NE(f, nullptr);
        for (uint32_t n = 0; n < frames; ++n) {
            for (uint32_t y = 0; y < h; ++y)
                for (uint32_t x = 0; x < w; ++x)
                    frame[y * w + x] = static_cast<uint8_t>(16 + ((x + n) * 219 / (w + frames)));
            std::fwrite(frame.data(), 1, frame.size(), f);
        }
        std::fclose(f);
    }

    set_env("MELLO_SOFT_ENCODER", "1");
    set_env("MELLO_SOFT_DECODER", "1");

    VideoPipeline host;
    if (!host.init_device()) GTEST_SKIP() << "No D3D11 device";
    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Reference;
    desc.reference.path = path.string();
    desc.reference.width = w;
    desc.reference.height = h;
    desc.reference.fps = 60;
    PipelineConfig cfg{};
    cfg.width = 0;
    cfg.height = 0;
    cfg.fps = 60;
    cfg.bitrate_kbps = 2000;

    std::mutex mu;
    std::vector<std::vector<uint8_t>> packets;
    std::vector<bool> keys;
    ASSERT_TRUE(host.start_host(desc, cfg, [&](const uint8_t* d, size_t n, bool key, uint64_t) {
        std::lock_guard<std::mutex> lock(mu);
        packets.emplace_back(d, d + n);
        keys.push_back(key);
    }));
    uint32_t ew = 0, eh = 0;
    host.get_host_resolution(ew, eh);
    EXPECT_EQ(ew, w);
    EXPECT_EQ(eh, h);
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
    while (std::chrono::steady_clock::now() < deadline) {
        {
            std::lock_guard<std::mutex> lock(mu);
            if (packets.size() >= 45) break;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    host.stop_host();
    ASSERT_GE(packets.size(), 45u) << "the reference source plays at 60 fps";
    EXPECT_TRUE(keys.front()) << "the first access unit is an IDR";

    VideoPipeline viewer;
    std::atomic<int> decoded{0};
    double worst = 0.0;
    PipelineConfig vcfg{};
    vcfg.width = w;
    vcfg.height = h;
    vcfg.fps = 60;
    ASSERT_TRUE(viewer.start_viewer(vcfg, [&](const uint8_t* rgba, uint32_t fw, uint32_t fh, uint64_t) {
        // Every row of a decoded frame keeps the ramp: luma rises left to right.
        double err = 0.0;
        for (uint32_t x = 8; x + 8 < fw; x += 8) {
            const int a = rgba[(fh / 2 * fw + x) * 4 + 1];
            const int b = rgba[(fh / 2 * fw + x + 8) * 4 + 1];
            if (b < a) err += a - b;
        }
        worst = (std::max)(worst, err);
        decoded++;
    }));
    for (size_t i = 0; i < packets.size(); ++i) {
        ASSERT_TRUE(viewer.feed_packet(packets[i].data(), packets[i].size(), keys[i]));
        std::this_thread::sleep_for(std::chrono::milliseconds(17));
        viewer.present_frame();
    }
    viewer.stop_viewer();
    set_env("MELLO_SOFT_ENCODER", nullptr);
    set_env("MELLO_SOFT_DECODER", nullptr);
    std::filesystem::remove(path);
    EXPECT_GE(decoded.load(), 30);
    EXPECT_LT(worst, 40.0) << "the decoded ramp is monotonic within coding noise";
}
#endif
