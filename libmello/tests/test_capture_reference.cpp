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
#include "video/video_viewer.hpp"

#include <atomic>
#include <cmath>
#include <string>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <functional>
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
namespace {

// Encode `frames` frames of a w x h clip with the OpenH264 host pipeline:
// the left half dark (luma 40), the right half bright (luma 200), neutral
// chroma. Returns the access units and their key-frame flags.
bool encode_clip(uint32_t w, uint32_t h, uint32_t frames, const char* name,
                 const std::function<uint8_t(uint32_t n, uint32_t x)>& luma,
                 std::vector<std::vector<uint8_t>>& packets, std::vector<bool>& keys) {
    const auto path = std::filesystem::temp_directory_path() / name;
    {
        std::vector<uint8_t> frame(reference_frame_bytes(w, h), 128);
        FILE* f = std::fopen(path.string().c_str(), "wb");
        if (!f) return false;
        for (uint32_t n = 0; n < frames; ++n) {
            for (uint32_t y = 0; y < h; ++y)
                for (uint32_t x = 0; x < w; ++x) frame[y * w + x] = luma(n, x);
            std::fwrite(frame.data(), 1, frame.size(), f);
        }
        std::fclose(f);
    }
    VideoPipeline host;
    if (!host.init_device()) return false;
    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Reference;
    desc.reference.path = path.string();
    desc.reference.width = w;
    desc.reference.height = h;
    desc.reference.fps = 60;
    PipelineConfig cfg{};
    cfg.fps = 60;
    cfg.bitrate_kbps = 2000;
    std::mutex mu;
    const size_t start = packets.size();
    if (!host.start_host(desc, cfg, [&](const uint8_t* d, size_t n, bool key, uint64_t) {
            std::lock_guard<std::mutex> lock(mu);
            packets.emplace_back(d, d + n);
            keys.push_back(key);
        })) {
        return false;
    }
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
    while (std::chrono::steady_clock::now() < deadline) {
        {
            std::lock_guard<std::mutex> lock(mu);
            if (packets.size() - start >= frames / 2) break;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    host.stop_host();
    std::filesystem::remove(path);
    return packets.size() - start >= frames / 2 && keys[start];
}

bool encode_halves(uint32_t w, uint32_t h, uint32_t frames, const char* name,
                   std::vector<std::vector<uint8_t>>& packets, std::vector<bool>& keys) {
    return encode_clip(w, h, frames, name,
                       [w](uint32_t, uint32_t x) { return static_cast<uint8_t>(x < w / 2 ? 40 : 200); },
                       packets, keys);
}

struct ShownFrame {
    uint32_t w = 0, h = 0;
    bool halves = false;  // dark left, bright right, grey in all four quadrants
};

ShownFrame inspect(const uint8_t* rgba, uint32_t w, uint32_t h) {
    ShownFrame f{w, h, true};
    auto px = [&](uint32_t x, uint32_t y) { return rgba + (static_cast<size_t>(y) * w + x) * 4; };
    for (uint32_t y : {h / 4, h * 3 / 4}) {
        for (uint32_t x : {w / 4, w * 3 / 4}) {
            const uint8_t* p = px(x, y);
            const int want = x < w / 2 ? 40 : 200;
            // BT.709 limited range: luma 40 and 200 are RGB about 28 and 214.
            const int rgb = (want - 16) * 255 / 219;
            for (int c = 0; c < 3; ++c) {
                if (std::abs(static_cast<int>(p[c]) - rgb) > 24) f.halves = false;
            }
        }
    }
    return f;
}

// Feed a stream that changes its size at an IDR (new SPS) into a viewer and
// return what was shown.
std::vector<ShownFrame> show(const std::vector<std::vector<uint8_t>>& packets,
                             const std::vector<bool>& keys, uint32_t w, uint32_t h,
                             std::string& decoder) {
    VideoPipeline viewer;
    std::mutex m;
    std::vector<ShownFrame> shown;
    PipelineConfig vcfg{};
    vcfg.width = w;
    vcfg.height = h;
    vcfg.fps = 60;
    if (!viewer.start_viewer(vcfg, [&](const uint8_t* rgba, uint32_t fw, uint32_t fh, uint64_t) {
            const ShownFrame f = inspect(rgba, fw, fh);
            std::lock_guard<std::mutex> lock(m);
            shown.push_back(f);
        })) {
        return {};
    }
    for (size_t i = 0; i < packets.size(); ++i) {
        viewer.feed_packet(packets[i].data(), packets[i].size(), keys[i]);
        std::this_thread::sleep_for(std::chrono::milliseconds(17));
        viewer.present_frame();
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(100));
    viewer.present_frame();
    ViewerStats stats;
    viewer.get_viewer_stats(stats);
    decoder = stats.decoder_name;
    viewer.stop_viewer();
    std::lock_guard<std::mutex> lock(m);
    return shown;
}

void expect_size_change_is_shown(bool soft_decoder) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    if (!OpenH264Encoder::is_available()) GTEST_SKIP() << "OpenH264 DLL with encoder not found";
    set_env("MELLO_SOFT_ENCODER", "1");
    // 640x360 then 320x180, as a host that steps from 1080p to 540p: neither
    // height is a multiple of 16, so the coded size is larger than the
    // picture in both.
    std::vector<std::vector<uint8_t>> packets;
    std::vector<bool> keys;
    const bool encoded = encode_halves(640, 360, 40, "mello_size_a.yuv", packets, keys) &&
                         encode_halves(320, 180, 40, "mello_size_b.yuv", packets, keys);
    set_env("MELLO_SOFT_ENCODER", nullptr);
    if (!encoded) GTEST_SKIP() << "no D3D11 video processor for the reference source";

    set_env("MELLO_SOFT_DECODER", soft_decoder ? "1" : nullptr);
    std::string decoder;
    const std::vector<ShownFrame> shown = show(packets, keys, 640, 360, decoder);
    set_env("MELLO_SOFT_DECODER", nullptr);
    if (!soft_decoder && decoder.find("OpenH264") != std::string::npos) {
        GTEST_SKIP() << "no hardware decoder";
    }

    size_t at_first = 0, at_second = 0, wrong = 0;
    for (const ShownFrame& f : shown) {
        if (f.w == 640 && f.h == 360 && f.halves) ++at_first;
        else if (f.w == 320 && f.h == 180 && f.halves) ++at_second;
        else ++wrong;
    }
    EXPECT_GE(at_first, 10u) << decoder;
    EXPECT_GE(at_second, 10u) << decoder << ": the frames after the size change are shown at their size";
    EXPECT_EQ(wrong, 0u) << decoder << ": no frame is shown at the old size or with a broken picture";
}

}  // namespace

// Stage 3 item 3: a decoded size change (an SPS at a new size, as after a
// resolution rung) gives new slots and a converter at the new size; the
// frames after it are whole pictures at that size.
TEST(ViewerSizeChange, TheSoftwareDecoderShowsTheNewSize) {
    expect_size_change_is_shown(true);
}

TEST(ViewerSizeChange, TheHardwareDecoderShowsTheNewSize) {
    expect_size_change_is_shown(false);
}
// Stage 3: the hardware decoder shows the picture of an access unit when it
// gets that access unit, as the software decoder does. Without the
// end-of-picture flag the NVDEC parser shows each picture only when the next
// access unit comes: one frame late, and at a size change the last picture
// of the old size indexed a surface of the new decoder.
TEST(ViewerSizeChange, TheHardwareDecoderShowsEachPictureWithItsAccessUnit) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    if (!OpenH264Encoder::is_available()) GTEST_SKIP() << "OpenH264 DLL with encoder not found";
    set_env("MELLO_SOFT_ENCODER", "1");
    std::vector<std::vector<uint8_t>> packets;
    std::vector<bool> keys;
    // Each frame has one luma level, from a cycle of six.
    const bool encoded = encode_clip(
        320, 240, 60, "mello_levels.yuv",
        [](uint32_t n, uint32_t) { return static_cast<uint8_t>(40 + (n % 6) * 30); }, packets, keys);
    set_env("MELLO_SOFT_ENCODER", nullptr);
    if (!encoded) GTEST_SKIP() << "no D3D11 video processor for the reference source";

    // The level shown after each access unit, for one decoder.
    auto levels = [&](bool soft, std::string& decoder) {
        set_env("MELLO_SOFT_DECODER", soft ? "1" : nullptr);
        VideoPipeline viewer;
        std::atomic<int> last{-1};
        PipelineConfig vcfg{};
        vcfg.width = 320;
        vcfg.height = 240;
        vcfg.fps = 60;
        std::vector<int> out;
        if (viewer.start_viewer(vcfg, [&](const uint8_t* rgba, uint32_t fw, uint32_t fh, uint64_t) {
                last = rgba[(fh / 2 * fw + fw / 2) * 4 + 1];
            })) {
            for (size_t i = 0; i < packets.size(); ++i) {
                viewer.feed_packet(packets[i].data(), packets[i].size(), keys[i]);
                std::this_thread::sleep_for(std::chrono::milliseconds(40));
                viewer.present_frame();
                std::this_thread::sleep_for(std::chrono::milliseconds(10));
                out.push_back(last.load());
            }
            ViewerStats stats;
            viewer.get_viewer_stats(stats);
            decoder = stats.decoder_name;
            viewer.stop_viewer();
        }
        set_env("MELLO_SOFT_DECODER", nullptr);
        return out;
    };
    std::string soft_name, hw_name;
    const std::vector<int> soft = levels(true, soft_name);
    const std::vector<int> hw = levels(false, hw_name);
    if (hw_name.find("OpenH264") != std::string::npos) GTEST_SKIP() << "no hardware decoder";
    ASSERT_EQ(soft.size(), hw.size());
    size_t same = 0;
    for (size_t i = 0; i < soft.size(); ++i) {
        if (soft[i] >= 0 && std::abs(soft[i] - hw[i]) <= 12) ++same;
    }
    EXPECT_GE(same, soft.size() - 2) << hw_name << " shows the picture of the access unit it just got";
}

// Streaming-quality stage 4, geometry rungs: set_encode_height changes the
// encode size mid-stream. The preprocessor is made again at the new size,
// the encoder changes its size (OpenH264: a full re-initialize; NVENC: in
// place), and the first access unit at the new size is an IDR. Before stage
// 4 the encode size was fixed for the whole stream.
namespace {

void run_geometry_change(bool soft, bool bitrate_step = false) {
    const uint32_t w = 640, h = 480, frames = 60;
    const auto path = std::filesystem::temp_directory_path() / "mello_geometry_test.yuv";
    {
        std::vector<uint8_t> frame(reference_frame_bytes(w, h), 128);
        FILE* f = std::fopen(path.string().c_str(), "wb");
        ASSERT_NE(f, nullptr);
        for (uint32_t n = 0; n < frames; ++n) {
            for (uint32_t y = 0; y < h; ++y)
                for (uint32_t x = 0; x < w; ++x)
                    frame[y * w + x] = static_cast<uint8_t>(16 + ((x + y + n) % 200));
            std::fwrite(frame.data(), 1, frame.size(), f);
        }
        std::fclose(f);
    }
    set_env("MELLO_SOFT_ENCODER", soft ? "1" : nullptr);

    VideoPipeline host;
    if (!host.init_device()) GTEST_SKIP() << "No D3D11 device";
    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Reference;
    desc.reference.path = path.string();
    desc.reference.width = w;
    desc.reference.height = h;
    desc.reference.fps = 60;
    PipelineConfig cfg{};
    cfg.width = w;
    cfg.height = h;
    cfg.fps = 60;
    cfg.bitrate_kbps = 2000;

    std::mutex mu;
    std::vector<bool> keys;
    ASSERT_TRUE(host.start_host(desc, cfg, [&](const uint8_t*, size_t, bool key, uint64_t) {
        std::lock_guard<std::mutex> lock(mu);
        keys.push_back(key);
    }));
    const auto wait_for = [&](size_t count) {
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
        while (std::chrono::steady_clock::now() < deadline) {
            {
                std::lock_guard<std::mutex> lock(mu);
                if (keys.size() >= count) return true;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(5));
        }
        return false;
    };
    ASSERT_TRUE(wait_for(20));
    size_t before = 0;
    {
        std::lock_guard<std::mutex> lock(mu);
        before = keys.size();
    }
    if (bitrate_step) {
        // A 75 % down-step: the old rule forced an IDR on any step over 25 %.
        host.set_bitrate(500);
        const auto until = std::chrono::steady_clock::now() + std::chrono::seconds(5);
        while (std::chrono::steady_clock::now() < until) {
            {
                std::lock_guard<std::mutex> lock(mu);
                if (keys.size() >= before + 20) break;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(5));
        }
        host.stop_host();
        set_env("MELLO_SOFT_ENCODER", nullptr);
        std::filesystem::remove(path);
        std::lock_guard<std::mutex> lock(mu);
        ASSERT_GE(keys.size(), before + 20);
        for (size_t i = before; i < keys.size(); ++i) {
            EXPECT_FALSE(keys[i]) << "IDR after the bitrate step, frame " << i;
        }
        return;
    }
    ASSERT_TRUE(host.set_encode_height(240));
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(5);
    while (host.geometry_changes() == 0 && std::chrono::steady_clock::now() < deadline) {
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    ASSERT_EQ(host.geometry_changes(), 1u);
    ASSERT_TRUE(wait_for(before + 20));
    uint32_t ew = 0, eh = 0;
    host.get_host_resolution(ew, eh);
    host.stop_host();
    set_env("MELLO_SOFT_ENCODER", nullptr);
    std::filesystem::remove(path);

    EXPECT_EQ(ew, 320u);
    EXPECT_EQ(eh, 240u);
    // A keyframe after the request: the first frame at the new size.
    bool key_after = false;
    for (size_t i = before; i < keys.size(); ++i) key_after = key_after || keys[i];
    EXPECT_TRUE(key_after) << "no IDR after the size change";
    // The GOP is 4 s: no periodic IDR in the first 20 frames but the first.
    for (size_t i = 1; i < before; ++i) EXPECT_FALSE(keys[i]) << "IDR at frame " << i;
}

} // namespace

TEST(ReferenceGeometry, ARungChangesTheEncodeSizeWithAnIdrOpenH264) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    if (!OpenH264Encoder::is_available()) GTEST_SKIP() << "OpenH264 DLL with encoder not found";
    run_geometry_change(true);
}

TEST(ReferenceGeometry, ARungChangesTheEncodeSizeWithAnIdrHardware) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    run_geometry_change(false);
}
// Streaming-quality stage 4: a bitrate change never forces an IDR. A
// down-step of more than 25 % did, and under goog_cc every congestion event
// is one: 55 IDRs in 30 s on the gate's congested profile.
TEST(ReferenceBitrate, ALargeDownStepGivesNoIdrOpenH264) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    if (!OpenH264Encoder::is_available()) GTEST_SKIP() << "OpenH264 DLL with encoder not found";
    run_geometry_change(true, true);
}

TEST(ReferenceBitrate, ALargeDownStepGivesNoIdrHardware) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    run_geometry_change(false, true);
}
// Streaming-quality stage 4: a frame rate change (the 30 fps rung) keeps the
// bitrate. NVENC rebuilt every reconfigure from its initialize config, so
// after set_framerate the encoder ran at the start bitrate again: 8.4 Mbps on
// a 2 Mbps target in the gate. A noise clip at 2 Mbps, the bitrate set to
// 400 kbps, then 30 fps: the bytes of the next second stay near 400 kbps.
TEST(ReferenceBitrate, AFrameRateChangeKeepsTheBitrateHardware) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    const uint32_t w = 640, h = 480, frames = 120;
    const auto path = std::filesystem::temp_directory_path() / "mello_rate_test.yuv";
    {
        std::vector<uint8_t> frame(reference_frame_bytes(w, h), 128);
        FILE* f = std::fopen(path.string().c_str(), "wb");
        ASSERT_NE(f, nullptr);
        uint32_t seed = 12345;
        for (uint32_t n = 0; n < frames; ++n) {
            for (size_t i = 0; i < static_cast<size_t>(w) * h; ++i) {
                seed = seed * 1103515245u + 12345u;
                frame[i] = static_cast<uint8_t>(16 + ((seed >> 16) % 200));
            }
            std::fwrite(frame.data(), 1, frame.size(), f);
        }
        std::fclose(f);
    }
    set_env("MELLO_SOFT_ENCODER", nullptr);
    VideoPipeline host;
    if (!host.init_device()) GTEST_SKIP() << "No D3D11 device";
    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Reference;
    desc.reference.path = path.string();
    desc.reference.width = w;
    desc.reference.height = h;
    desc.reference.fps = 60;
    PipelineConfig cfg{};
    cfg.width = w;
    cfg.height = h;
    cfg.fps = 60;
    cfg.bitrate_kbps = 2000;
    std::mutex mu;
    std::vector<std::pair<uint64_t, size_t>> sizes;  // (arrival us, bytes)
    const auto now_us = [] {
        return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count());
    };
    ASSERT_TRUE(host.start_host(desc, cfg, [&](const uint8_t*, size_t n, bool, uint64_t) {
        std::lock_guard<std::mutex> lock(mu);
        sizes.emplace_back(now_us(), n);
    }));
    std::this_thread::sleep_for(std::chrono::milliseconds(500));
    host.set_bitrate(400);
    std::this_thread::sleep_for(std::chrono::milliseconds(500));
    host.set_output_fps(30);
    std::this_thread::sleep_for(std::chrono::milliseconds(500));
    const uint64_t from = now_us();
    std::this_thread::sleep_for(std::chrono::milliseconds(1500));
    const uint64_t to = now_us();
    host.stop_host();
    std::filesystem::remove(path);
    uint64_t bytes = 0;
    {
        std::lock_guard<std::mutex> lock(mu);
        for (const auto& [at, n] : sizes) {
            if (at >= from && at < to) bytes += n;
        }
    }
    const double kbps = static_cast<double>(bytes) * 8.0 / (static_cast<double>(to - from) / 1e6) / 1000.0;
    std::printf("[rate] after the 30 fps change: %.0f kbps for a 400 kbps target\n", kbps);
    EXPECT_LT(kbps, 800.0) << "the frame rate change put the start bitrate back";
}
#endif
