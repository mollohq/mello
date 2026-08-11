#include <gtest/gtest.h>
#include "video/video_pipeline.hpp"
#include <vector>
#include <mutex>
#include <atomic>
#include <thread>
#include <chrono>
#include <cstdlib>
#include <cstring>
#include <fstream>

using namespace mello::video;

struct CapturedPacket {
    std::vector<uint8_t> data;
    bool                 is_keyframe;
    uint64_t             timestamp;
};

class VideoPipelineTest : public ::testing::Test {
protected:
    VideoPipeline pipeline;

    void SetUp() override {
        // Hardware-dependent (GPU capture/encode): skipped under CI, same
        // contract as `CI=true cargo test --workspace` for the Rust harness.
        if (std::getenv("CI")) {
            GTEST_SKIP() << "hardware-dependent test skipped under CI";
        }
        if (!pipeline.init_device()) {
            GTEST_SKIP() << "No D3D11 device available";
        }
        if (!pipeline.encoder_available()) {
            GTEST_SKIP() << "No hardware encoder available";
        }
    }

    CaptureSourceDesc monitor_source(uint32_t index = 0) {
        CaptureSourceDesc desc{};
        desc.mode = CaptureMode::Monitor;
        desc.monitor_index = index;
        return desc;
    }

    PipelineConfig default_config() {
        PipelineConfig cfg{};
        cfg.width        = 1280;
        cfg.height       = 720;
        cfg.fps          = 30;
        cfg.bitrate_kbps = 5000;
        cfg.low_latency  = true;
        return cfg;
    }
};

TEST_F(VideoPipelineTest, EncoderAvailabilityCheck) {
    EXPECT_TRUE(pipeline.encoder_available());
}

TEST_F(VideoPipelineTest, HostEncodesPackets) {
    std::vector<CapturedPacket> packets;
    std::mutex mtx;

    auto source = monitor_source();
    auto config = default_config();

    auto on_packet = [&](const uint8_t* data, size_t size, bool is_keyframe, uint64_t ts) {
        std::lock_guard<std::mutex> lock(mtx);
        packets.push_back({
            std::vector<uint8_t>(data, data + size),
            is_keyframe,
            ts
        });
    };

    ASSERT_TRUE(pipeline.start_host(source, config, on_packet));
    EXPECT_TRUE(pipeline.is_host_running());

    std::this_thread::sleep_for(std::chrono::seconds(2));

    pipeline.stop_host();
    EXPECT_FALSE(pipeline.is_host_running());

    std::lock_guard<std::mutex> lock(mtx);
    EXPECT_GT(packets.size(), 0u) << "No packets produced in 2 seconds";

    bool has_keyframe = false;
    for (auto& p : packets) {
        if (p.is_keyframe) { has_keyframe = true; break; }
    }
    EXPECT_TRUE(has_keyframe) << "No keyframe in captured packets";
}

TEST_F(VideoPipelineTest, HostToViewerLoopback) {
    // Phase 1: capture + encode
    std::vector<CapturedPacket> packets;
    std::mutex pkt_mtx;

    auto source = monitor_source();
    auto config = default_config();

    auto on_packet = [&](const uint8_t* data, size_t size, bool is_keyframe, uint64_t ts) {
        std::lock_guard<std::mutex> lock(pkt_mtx);
        packets.push_back({
            std::vector<uint8_t>(data, data + size),
            is_keyframe,
            ts
        });
    };

    ASSERT_TRUE(pipeline.start_host(source, config, on_packet));
    std::this_thread::sleep_for(std::chrono::seconds(2));
    pipeline.stop_host();

    {
        std::lock_guard<std::mutex> lock(pkt_mtx);
        ASSERT_GT(packets.size(), 0u) << "No packets to feed to viewer";
    }

    // Phase 2: decode
    std::atomic<uint32_t> frames_decoded{0};
    uint32_t last_w = 0, last_h = 0;

    auto on_frame = [&](const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t ts) {
        last_w = w;
        last_h = h;
        frames_decoded++;
    };

    ASSERT_TRUE(pipeline.start_viewer(config, on_frame));
    EXPECT_TRUE(pipeline.is_viewer_running());

    // Feed from first keyframe onward
    bool seen_keyframe = false;
    for (auto& p : packets) {
        if (!seen_keyframe) {
            if (p.is_keyframe) seen_keyframe = true;
            else continue;
        }
        pipeline.feed_packet(p.data.data(), p.data.size(), p.is_keyframe);
    }

    // Decoder may need a moment to flush
    std::this_thread::sleep_for(std::chrono::milliseconds(200));

    pipeline.stop_viewer();

    EXPECT_GT(frames_decoded.load(), 0u) << "No frames decoded";
    EXPECT_EQ(last_w, config.width);
    EXPECT_EQ(last_h, config.height);
}

TEST_F(VideoPipelineTest, SaveDecodedFrame) {
    // Capture a couple seconds, decode, save one frame as BMP for visual inspection.
    // This test always passes — check the output file manually.
    std::vector<CapturedPacket> packets;
    std::mutex pkt_mtx;

    auto source = monitor_source();
    auto config = default_config();

    auto on_packet = [&](const uint8_t* data, size_t size, bool is_keyframe, uint64_t ts) {
        std::lock_guard<std::mutex> lock(pkt_mtx);
        packets.push_back({
            std::vector<uint8_t>(data, data + size),
            is_keyframe,
            ts
        });
    };

    ASSERT_TRUE(pipeline.start_host(source, config, on_packet));
    std::this_thread::sleep_for(std::chrono::seconds(1));
    pipeline.stop_host();

    if (packets.empty()) {
        std::cerr << "[SaveDecodedFrame] No packets captured, skipping save\n";
        return;
    }

    std::vector<uint8_t> saved_rgba;
    uint32_t saved_w = 0, saved_h = 0;

    auto on_frame = [&](const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t ts) {
        if (saved_rgba.empty()) {
            saved_rgba.assign(rgba, rgba + (size_t)w * h * 4);
            saved_w = w;
            saved_h = h;
        }
    };

    ASSERT_TRUE(pipeline.start_viewer(config, on_frame));

    bool seen_keyframe = false;
    for (auto& p : packets) {
        if (!seen_keyframe) {
            if (p.is_keyframe) seen_keyframe = true;
            else continue;
        }
        pipeline.feed_packet(p.data.data(), p.data.size(), p.is_keyframe);
        if (!saved_rgba.empty()) break;
    }

    std::this_thread::sleep_for(std::chrono::milliseconds(100));
    pipeline.stop_viewer();

    if (saved_rgba.empty()) {
        std::cerr << "[SaveDecodedFrame] No frames decoded\n";
        return;
    }

    // Write raw BMP (RGBA -> BGR for BMP, flip rows)
    const char* path = "decoded_frame.bmp";
    std::ofstream f(path, std::ios::binary);
    if (!f) return;

    uint32_t row_bytes = saved_w * 3;
    uint32_t row_padded = (row_bytes + 3) & ~3u;
    uint32_t pixel_size = row_padded * saved_h;
    uint32_t file_size = 54 + pixel_size;

    uint8_t hdr[54] = {};
    hdr[0] = 'B'; hdr[1] = 'M';
    memcpy(hdr + 2, &file_size, 4);
    uint32_t offset = 54; memcpy(hdr + 10, &offset, 4);
    uint32_t dib = 40; memcpy(hdr + 14, &dib, 4);
    int32_t w = saved_w; memcpy(hdr + 18, &w, 4);
    int32_t h = saved_h; memcpy(hdr + 22, &h, 4);
    uint16_t planes = 1; memcpy(hdr + 26, &planes, 2);
    uint16_t bpp = 24; memcpy(hdr + 28, &bpp, 2);
    memcpy(hdr + 34, &pixel_size, 4);
    f.write(reinterpret_cast<char*>(hdr), 54);

    std::vector<uint8_t> row(row_padded, 0);
    for (int32_t y = saved_h - 1; y >= 0; --y) {
        for (uint32_t x = 0; x < saved_w; ++x) {
            size_t src = ((size_t)y * saved_w + x) * 4;
            row[x * 3 + 0] = saved_rgba[src + 2]; // B
            row[x * 3 + 1] = saved_rgba[src + 1]; // G
            row[x * 3 + 2] = saved_rgba[src + 0]; // R
        }
        f.write(reinterpret_cast<char*>(row.data()), row_padded);
    }

    std::cout << "[SaveDecodedFrame] Wrote " << saved_w << "x" << saved_h
              << " frame to " << path << "\n";
}

// ─────────────────────────────────────────────────────────────────────────────
// Encoder overload policy
//
// These run everywhere, deliberately: the GPUs that trigger a downgrade are the
// ones we do not have on the bench, so the decision has to be provable without
// one. No device, no encoder, no capture — pure arithmetic.
// ─────────────────────────────────────────────────────────────────────────────

namespace {

VideoPipeline::EncoderLoadSample healthy_sample() {
    VideoPipeline::EncoderLoadSample s{};
    s.frames_captured = 600;   // ~10s at 60fps
    s.queue_drops     = 0;
    s.mean_encode_ms  = 6.0;   // comfortable inside a 16.7ms budget
    s.target_fps      = 60;
    return s;
}

} // namespace

TEST(EncoderOverloadPolicy, HealthyEncoderIsNotDowngraded) {
    EXPECT_FALSE(VideoPipeline::encoder_is_overloaded(healthy_sample()));
}

TEST(EncoderOverloadPolicy, SustainedQueueDropsTriggerDowngrade) {
    auto s = healthy_sample();
    s.queue_drops = 30; // 5% of offered frames evicted unencoded
    EXPECT_TRUE(VideoPipeline::encoder_is_overloaded(s));
}

TEST(EncoderOverloadPolicy, EncodeTimeNearFrameBudgetTriggersDowngrade) {
    auto s = healthy_sample();
    // 14ms against a 16.7ms budget: no headroom left for a harder scene, and
    // the encode queue is only two deep.
    s.mean_encode_ms = 14.0;
    EXPECT_TRUE(VideoPipeline::encoder_is_overloaded(s));
}

TEST(EncoderOverloadPolicy, BudgetScalesWithTargetFramerate) {
    auto s = healthy_sample();
    s.mean_encode_ms = 20.0;
    s.target_fps     = 60; // 16.7ms budget — over
    EXPECT_TRUE(VideoPipeline::encoder_is_overloaded(s));
    s.target_fps     = 30; // 33.3ms budget — comfortable
    EXPECT_FALSE(VideoPipeline::encoder_is_overloaded(s));
}

TEST(EncoderOverloadPolicy, ShortWindowsCannotTriggerDowngrade) {
    auto s = healthy_sample();
    s.frames_captured = 10;
    s.queue_drops     = 10;   // everything dropped, but far too few samples
    s.mean_encode_ms  = 100.0;
    EXPECT_FALSE(VideoPipeline::encoder_is_overloaded(s))
        << "a startup hitch must not permanently downgrade quality";
}

TEST(EncoderOverloadPolicy, ZeroTargetFpsIsNotTreatedAsOverload) {
    auto s = healthy_sample();
    s.target_fps = 0; // guards a divide-by-zero on an unconfigured pipeline
    EXPECT_FALSE(VideoPipeline::encoder_is_overloaded(s));
}

// ─────────────────────────────────────────────────────────────────────────────
// Framerate decimation cadence
// ─────────────────────────────────────────────────────────────────────────────

namespace {

// Runs `frames` arrivals at `source_fps` through the decimator and returns how
// many were accepted. Synthetic clock — no capture device involved.
int decimated_count(uint32_t source_fps, uint32_t target_fps, int frames) {
    uint64_t deadline = 0;
    int accepted = 0;
    for (int i = 0; i < frames; ++i) {
        const uint64_t ts =
            static_cast<uint64_t>(i) * (1'000'000ULL / source_fps);
        if (VideoPipeline::decimation_accepts(ts, deadline, target_fps)) {
            ++accepted;
        }
    }
    return accepted;
}

} // namespace

TEST(FramerateDecimation, ZeroTargetAcceptsEveryFrame) {
    // 0 means "no decimation" — every captured frame is encoded.
    uint64_t deadline = 500;
    EXPECT_TRUE(VideoPipeline::decimation_accepts(1000, deadline, 0));
    EXPECT_TRUE(VideoPipeline::decimation_accepts(901, deadline, 0));
}

TEST(FramerateDecimation, FirstFrameIsAlwaysAccepted) {
    uint64_t deadline = 0;
    EXPECT_TRUE(VideoPipeline::decimation_accepts(12345, deadline, 30));
    EXPECT_GT(deadline, 12345u) << "deadline must be armed off the first frame";
}

TEST(FramerateDecimation, HalvesA60fpsSourceTo30) {
    const int accepted = decimated_count(60, 30, 60);
    EXPECT_GE(accepted, 29);
    EXPECT_LE(accepted, 31);
}

TEST(FramerateDecimation, HoldsTargetWhenSourceIsNotAMultiple) {
    // The regression this whole mechanism exists for. A 165Hz display delivering
    // ~83fps into a 60fps target is not an integer ratio, so a naive
    // "interval minus tolerance" deadline cannot express the answer: too small a
    // tolerance emits every other frame (41fps), too large emits every frame
    // (83fps). A real host was measured doing exactly the latter, and the
    // encoder — configured for 60 — overshot its bitrate by 83/60.
    const int accepted = decimated_count(83, 60, 83);
    EXPECT_GE(accepted, 58) << "under-delivering: cadence collapsed onto the source rate";
    EXPECT_LE(accepted, 62) << "over-delivering: tolerance swallowed a whole interval";
}

TEST(FramerateDecimation, HoldsTargetFromAVeryFastSource) {
    // 240Hz source, 60fps target — every fourth frame.
    const int accepted = decimated_count(240, 60, 240);
    EXPECT_GE(accepted, 59);
    EXPECT_LE(accepted, 61);
}

TEST(FramerateDecimation, PassesEverythingWhenSourceMatchesTarget) {
    // Equal rates must not decimate: dropping here would be a visible stutter
    // on the overwhelmingly common 60Hz/60fps case.
    const int accepted = decimated_count(60, 60, 60);
    EXPECT_GE(accepted, 59);
}

TEST(FramerateDecimation, RejectsFramesInsideTheInterval) {
    // Well inside the interval — genuinely too early, must be dropped.
    uint64_t deadline = 0;
    EXPECT_TRUE(VideoPipeline::decimation_accepts(1'000, deadline, 30));
    EXPECT_FALSE(VideoPipeline::decimation_accepts(2'000, deadline, 30));
}

TEST(FramerateDecimation, StallDoesNotProduceACatchUpBurst) {
    // After a long capture gap the deadline is many intervals in the past.
    // Emitting one frame per missed interval would dump a burst into an encode
    // queue two deep, evicting live frames to encode stale ones.
    uint64_t deadline = 0;
    EXPECT_TRUE(VideoPipeline::decimation_accepts(0, deadline, 60));
    const uint64_t after_stall = 5'000'000;  // 5s gap
    EXPECT_TRUE(VideoPipeline::decimation_accepts(after_stall, deadline, 60));
    EXPECT_GT(deadline, after_stall) << "deadline left in the past; next frames would all pass";
    EXPECT_FALSE(VideoPipeline::decimation_accepts(after_stall + 1'000, deadline, 60));
}

TEST(FramerateDecimation, ClockResetDoesNotWedgeTheStream) {
    // A backend hot-swap can restart timestamps. Accepting on a backwards jump
    // keeps frames flowing instead of stalling until the old clock is passed.
    uint64_t deadline = 9'000'000;
    EXPECT_TRUE(VideoPipeline::decimation_accepts(5, deadline, 30));
}
