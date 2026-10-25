#include <gtest/gtest.h>
#include "video/video_pipeline.hpp"
#include "video/frame_cadence.hpp"
#include "video/encoder_control.hpp"
#include "video/nvdec_host_layout.hpp"
#include <future>
#include <vector>
#include <algorithm>
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

    // Phase 2: decode, at the size the host encoded (the client does the same
    // through mello_stream_get_host_resolution). It keeps the source aspect,
    // so it is not the preset size on every monitor.
    pipeline.get_host_resolution(config.width, config.height);
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
        if (frames_decoded.load() > 0) break;
    }

    // The viewer does not push frames: the client pulls one per tick via
    // present_frame(). Poll it until a frame arrives or the wait expires
    // (same shape as HookLoopback::run_loopback).
    for (int i = 0; i < 400 && frames_decoded.load() == 0; ++i) {
        pipeline.present_frame();
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }

    pipeline.stop_viewer();

    EXPECT_GT(frames_decoded.load(), 0u) << "No frames decoded";
    EXPECT_EQ(last_w, config.width);
    EXPECT_EQ(last_h, config.height);
}

#ifdef _WIN32
// The viewer of a Windows client without an NVIDIA GPU: a decoder that emits
// NV12 (here OpenH264, forced with MELLO_SOFT_DECODER=1) and only a native
// frame callback, no CPU callback. Every presented frame must reach the
// native callback with a shared RGBA handle. Before stage 2 present_frame
// returned false on every frame of this path.
TEST_F(VideoPipelineTest, AnNv12DecoderReachesTheNativeCallback) {
    // The Cisco DLL is fetched by scripts/stream-gate.sh (stage 1), not built.
    bool openh264 = false;
    for (const char* name : {"openh264-2.6.0-win64.dll", "openh264-2.5.0-win64.dll", "openh264.dll"}) {
        if (HMODULE dll = LoadLibraryA(name)) {
            FreeLibrary(dll);
            openh264 = true;
            break;
        }
    }
    if (!openh264) GTEST_SKIP() << "the Cisco OpenH264 DLL is not next to the tests";

    std::vector<CapturedPacket> packets;
    std::mutex pkt_mtx;
    auto config = default_config();
    ASSERT_TRUE(pipeline.start_host(monitor_source(), config,
        [&](const uint8_t* data, size_t size, bool is_keyframe, uint64_t ts) {
            std::lock_guard<std::mutex> lock(pkt_mtx);
            packets.push_back({std::vector<uint8_t>(data, data + size), is_keyframe, ts});
        }));
    std::this_thread::sleep_for(std::chrono::seconds(2));
    pipeline.stop_host();
    ASSERT_FALSE(packets.empty()) << "No packets to feed to viewer";
    pipeline.get_host_resolution(config.width, config.height);

    struct SoftDecoder {
        SoftDecoder() { SetEnvironmentVariableA("MELLO_SOFT_DECODER", "1"); }
        ~SoftDecoder() { SetEnvironmentVariableA("MELLO_SOFT_DECODER", nullptr); }
    } soft_decoder;

    std::atomic<uint32_t> native_frames{0};
    std::atomic<bool> null_handle{false};
    std::atomic<uint32_t> native_format{0};
    pipeline.set_native_frame_callback(
        [&](void* handle, uint32_t, uint32_t, uint32_t format, uint32_t, uint64_t) {
            if (!handle) null_handle = true;
            native_format = format;
            native_frames++;
        });
    ASSERT_TRUE(pipeline.start_viewer(config, nullptr));

    bool seen_keyframe = false;
    for (auto& p : packets) {
        if (!seen_keyframe) {
            if (!p.is_keyframe) continue;
            seen_keyframe = true;
        }
        pipeline.feed_packet(p.data.data(), p.data.size(), p.is_keyframe);
    }
    for (int i = 0; i < 400 && native_frames.load() == 0; ++i) {
        pipeline.present_frame();
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    pipeline.stop_viewer();

    EXPECT_GT(native_frames.load(), 0u) << "no frame reached the native callback";
    EXPECT_FALSE(null_handle.load());
    EXPECT_EQ(native_format.load(), 1u) << "RGBA8";
}
#endif

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

    pipeline.get_host_resolution(config.width, config.height);
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
        // Pull decoded frames; the viewer only fires on_frame from
        // present_frame().
        pipeline.present_frame();
    }

    for (int i = 0; i < 200 && saved_rgba.empty(); ++i) {
        pipeline.present_frame();
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
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

// ─────────────────────────────────────────────────────────────────────────────
// The capture backend throttles (hook, WGC, DXGI) and the pipeline decimator
// share one rule, frame_cadence_accepts. Before stage 2 each backend had its
// own: the hook gave 48 fps from a 144 fps game for a 60 fps target (50 from
// 100, 37.5 from 75), WGC counted a dropped frame's time twice, and DXGI reset
// to the arrival time.
// ─────────────────────────────────────────────────────────────────────────────

namespace {

// Deterministic jitter in [-amplitude, +amplitude] microseconds.
struct Jitter {
    uint32_t state = 12345;
    int64_t next(int64_t amplitude) {
        if (amplitude == 0) return 0;
        state = state * 1103515245u + 12345u;
        return static_cast<int64_t>((state >> 8) % (2 * amplitude + 1)) - amplitude;
    }
};

uint64_t present_time_us(double source_fps, int frame, uint64_t offset_us, Jitter& jitter,
                         int64_t jitter_us) {
    return 1'000'000ULL + offset_us +
           static_cast<uint64_t>(static_cast<double>(frame) * 1'000'000.0 / source_fps) +
           static_cast<uint64_t>(jitter.next(jitter_us));
}

} // namespace

TEST(FrameCadence, EveryGameRateGivesTheTarget) {
    for (const double source_fps : {144.0, 100.0, 75.0, 60.0}) {
        uint64_t deadline = 0;
        Jitter jitter;
        int delivered = 0;
        const int frames = static_cast<int>(source_fps * 10);  // 10 s
        for (int i = 0; i < frames; ++i) {
            if (frame_cadence_accepts(present_time_us(source_fps, i, 0, jitter, 0), deadline, 60)) {
                ++delivered;
            }
        }
        const double fps = delivered / 10.0;
        EXPECT_NEAR(fps, 60.0, 1.0) << "source " << source_fps << " fps";
    }
}

// A backend throttle feeds the pipeline decimator, and the two cadences start
// at different times (the ladder swaps backends during a stream). The second
// stage must pass what the first passes, at every phase. With the old pipeline
// rule (a hard reset after a late frame) a 75 fps game with a 7 ms phase gave
// 45 fps here.
TEST(FrameCadence, ABackendThrottleIntoThePipelineKeepsTheRate) {
    for (const double source_fps : {144.0, 100.0, 75.0, 60.0}) {
        for (uint64_t offset_ms = 0; offset_ms < 17; ++offset_ms) {
            for (const int64_t jitter_us : {0, 500, 1000}) {
                uint64_t backend_deadline = 0;
                uint64_t pipeline_deadline = 0;
                // The pipeline cadence started with an earlier backend.
                ASSERT_TRUE(VideoPipeline::decimation_accepts(1'000'000, pipeline_deadline, 60));
                Jitter jitter;
                int delivered = 0;
                const int frames = static_cast<int>(source_fps * 10);
                for (int i = 0; i < frames; ++i) {
                    const uint64_t ts =
                        present_time_us(source_fps, i, offset_ms * 1000, jitter, jitter_us);
                    if (!frame_cadence_accepts(ts, backend_deadline, 60)) continue;
                    if (VideoPipeline::decimation_accepts(ts, pipeline_deadline, 60)) ++delivered;
                }
                EXPECT_NEAR(delivered / 10.0, 60.0, 1.0)
                    << "source " << source_fps << " fps, phase " << offset_ms << " ms, jitter "
                    << jitter_us << " us";
            }
        }
    }
}

// Lateness is kept as credit up to 3/4 of an interval and no more. After a
// stall the cadence gives one frame now and the next no sooner than 1/8 of an
// interval later: never a burst of frames to catch up.
TEST(FrameCadence, AStallGivesNoBurst) {
    uint64_t deadline = 0;
    ASSERT_TRUE(frame_cadence_accepts(0, deadline, 60));
    const uint64_t after_stall = 2'000'000;
    ASSERT_TRUE(frame_cadence_accepts(after_stall, deadline, 60));
    int burst = 0;
    for (uint64_t t = after_stall + 1'000; t < after_stall + 16'666; t += 1'000) {
        if (frame_cadence_accepts(t, deadline, 60)) ++burst;
    }
    EXPECT_LE(burst, 1) << "a stall must not be caught up";
}

// ─────────────────────────────────────────────────────────────────────────────
// Encode size. Before stage 2 each axis was clamped on its own and the
// preprocessor stretched the source to it: 3440x1440 on the 1080p preset was
// encoded at 1920x1080, a squashed picture.
// ─────────────────────────────────────────────────────────────────────────────

namespace {
double aspect(uint32_t w, uint32_t h) { return static_cast<double>(w) / h; }
} // namespace

TEST(EncodeSize, KeepsThePresetHeightAndTheSourceAspect) {
    auto s = VideoPipeline::encode_size_for(1920, 1200, 1080, 60);
    EXPECT_EQ(s.width, 1728u);
    EXPECT_EQ(s.height, 1080u);
    EXPECT_FALSE(s.level_capped);

    s = VideoPipeline::encode_size_for(1440, 1080, 720, 60);  // 4:3
    EXPECT_EQ(s.width, 960u);
    EXPECT_EQ(s.height, 720u);

    s = VideoPipeline::encode_size_for(1920, 1080, 720, 60);
    EXPECT_EQ(s.width, 1280u);
    EXPECT_EQ(s.height, 720u);

    s = VideoPipeline::encode_size_for(2560, 1440, 1080, 60);
    EXPECT_EQ(s.width, 1920u);
    EXPECT_EQ(s.height, 1080u);

    s = VideoPipeline::encode_size_for(3440, 1440, 720, 60);  // ultrawide, 720p
    EXPECT_EQ(s.width, 1728u);
    EXPECT_EQ(s.height, 720u);
}

// Every encoder backend configures H.264 level 5.1 (streaming-quality stage
// 4): 36864 macroblocks per frame, 983040 per second. An ultrawide source on
// the 1080p60 preset keeps the preset height: 2576x1080 is 10948 macroblocks
// (level 4.2 allowed 8704 and gave 2288x960).
TEST(EncodeSize, AnUltrawideSourceKeepsThePresetHeightAtLevel51) {
    const auto s = VideoPipeline::encode_size_for(3440, 1440, 1080, 60);
    EXPECT_FALSE(s.level_capped);
    EXPECT_EQ(s.width, 2576u);
    EXPECT_EQ(s.height, 1080u);
    EXPECT_NEAR(aspect(s.width, s.height), aspect(3440, 1440), 0.01);
}

// Over level 5.1 the height goes down, the aspect stays: the same source at
// 120 fps is 1313760 macroblocks per second at 1080 lines.
TEST(EncodeSize, AnUltrawideSourceIsCappedAtTheEncoderLevel) {
    const auto s = VideoPipeline::encode_size_for(3440, 1440, 1080, 120);
    EXPECT_TRUE(s.level_capped);
    EXPECT_LT(s.height, 1080u);
    EXPECT_NEAR(aspect(s.width, s.height), aspect(3440, 1440), 0.01);
    const uint32_t mbs = ((s.width + 15) / 16) * ((s.height + 15) / 16);
    EXPECT_LE(mbs, 36864u);
    EXPECT_LE(mbs * 120u, 983040u);
}

TEST(EncodeSize, NeverUpscales) {
    auto s = VideoPipeline::encode_size_for(1280, 720, 1080, 60);
    EXPECT_EQ(s.width, 1280u);
    EXPECT_EQ(s.height, 720u);

    s = VideoPipeline::encode_size_for(1921, 1081, 1080, 60);  // odd: NV12 needs even
    EXPECT_EQ(s.width, 1920u);
    EXPECT_EQ(s.height, 1080u);

    s = VideoPipeline::encode_size_for(1920, 1080, 0, 60);  // no preset height
    EXPECT_EQ(s.width, 1920u);
    EXPECT_EQ(s.height, 1080u);
}

TEST(EncodeSize, AWidthIsAMultipleOf16WhenScaled) {
    for (const uint32_t cap_w : {1366u, 1600u, 1680u, 2560u, 3440u, 3840u, 5120u}) {
        for (const uint32_t cap_h : {768u, 900u, 1050u, 1080u, 1440u, 1600u, 2160u}) {
            for (const uint32_t preset_h : {480u, 720u, 1080u}) {
                const auto s = VideoPipeline::encode_size_for(cap_w, cap_h, preset_h, 60);
                if (s.height < cap_h) {
                    EXPECT_EQ(s.width % 16, 0u) << cap_w << "x" << cap_h << " on " << preset_h;
                    EXPECT_NEAR(aspect(s.width, s.height), aspect(cap_w, cap_h), 0.02)
                        << cap_w << "x" << cap_h << " on " << preset_h;
                }
                EXPECT_LE(s.width, cap_w);
                EXPECT_LE(s.height, cap_h);
                EXPECT_EQ(s.height % 2, 0u);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Encoder control. The Rust threads set the bitrate and the framerate and ask
// for keyframes while the encode thread encodes and changes cost tiers. Before
// stage 2 nothing serialised them, and no encoder backend is thread safe.
// ─────────────────────────────────────────────────────────────────────────────

namespace {

// Counts any two calls that run at the same time, and any encode that sees a
// bitrate half written. Its two rate fields are written one after the other.
class TornStateEncoder : public Encoder {
public:
    std::atomic<int>      inside{0};
    std::atomic<int>      overlaps{0};
    std::atomic<int>      torn{0};
    std::atomic<int>      encodes{0};
    std::atomic<int>      keyframes{0};
    std::atomic<uint32_t> rate_a{1000};
    std::atomic<uint32_t> rate_b{1000};
    std::atomic<uint32_t> fps{60};
    // When set, encode() waits here until it is cleared.
    std::atomic<bool>     hold_encode{false};
    std::atomic<bool>     in_held_encode{false};

    bool initialize(const GraphicsDevice&, const EncoderConfig&) override { return true; }
    void shutdown() override {}
#ifdef _WIN32
    bool encode(ID3D11Texture2D*, EncodedPacket& out) override { return run_encode(out); }
#elif defined(__APPLE__)
    bool encode(void*, uint64_t, EncodedPacket& out) override { return run_encode(out); }
#endif
    void request_keyframe() override {
        enter();
        keyframes.fetch_add(1);
        leave();
    }
    void set_bitrate(uint32_t kbps) override {
        enter();
        rate_a.store(kbps);
        for (int i = 0; i < 4; ++i) std::this_thread::yield();
        rate_b.store(kbps);
        leave();
    }
    void set_framerate(uint32_t value) override {
        enter();
        fps.store(value);
        leave();
    }
    bool reduce_cost_tier() override {
        enter();
        std::this_thread::yield();
        leave();
        return false;
    }
    void get_stats(EncoderStats& out) const override { out = EncoderStats{}; }
    bool supports_codec(VideoCodec) const override { return true; }
    const char* name() const override { return "torn-state"; }

private:
    void enter() {
        if (inside.fetch_add(1) != 0) overlaps.fetch_add(1);
    }
    void leave() { inside.fetch_sub(1); }
    bool run_encode(EncodedPacket& out) {
        enter();
        in_held_encode.store(hold_encode.load());
        while (hold_encode.load()) std::this_thread::yield();
        in_held_encode.store(false);
        const uint32_t a = rate_a.load();
        std::this_thread::yield();
        const uint32_t b = rate_b.load();
        if (a != b) torn.fetch_add(1);
        encodes.fetch_add(1);
        leave();
        out.data.assign(4, 0);
        return true;
    }
};

// One encode through the control with the platform's arguments: a texture on
// Windows, a pixel buffer and its capture time on Apple (encoder_control.hpp).
bool encode_once(EncoderControl& control, EncodedPacket& packet) {
#ifdef _WIN32
    return control.encode(nullptr, packet);
#elif defined(__APPLE__)
    return control.encode(nullptr, 0, packet);
#else
    (void)control;
    (void)packet;
    return false;
#endif
}

} // namespace

TEST(EncoderControl, SettersNeverRunBesideAnEncode) {
    TornStateEncoder encoder;
    EncoderControl control;
    control.attach(&encoder);

    std::atomic<bool> setters_done{false};
    std::thread encode_thread([&] {
        EncodedPacket packet;
        int frames = 0;
        while (!setters_done.load() || frames < 1000) {
            encode_once(control, packet);
            if (frames % 50 == 0) control.reduce_cost_tier();
            ++frames;
        }
    });
    // Both threads run at the same time from here.
    while (encoder.encodes.load() == 0) std::this_thread::yield();
    for (uint32_t i = 0; i < 1000; ++i) {
        control.set_bitrate(1000 + i);
        if (i % 100 == 0) control.set_framerate(i % 200 == 0 ? 30 : 60);
        if (i % 10 == 0) control.request_keyframe();
    }
    setters_done.store(true);
    encode_thread.join();
    control.apply_pending();

    EXPECT_EQ(encoder.overlaps.load(), 0) << "a setter ran beside the encode path";
    EXPECT_EQ(encoder.torn.load(), 0) << "an encode saw a half-written bitrate";
    EXPECT_EQ(encoder.rate_a.load(), 1999u) << "the last bitrate wins";
    EXPECT_EQ(encoder.fps.load(), 60u);
    EXPECT_GE(encoder.keyframes.load(), 1);
    EXPECT_FALSE(control.has_pending());
}

// A setter must never wait for an encode: an encode can be stuck in the display
// driver for minutes (2026-09-16), and the caller is a Rust stream thread.
TEST(EncoderControl, ASetterDoesNotWaitForAnEncode) {
    TornStateEncoder encoder;
    EncoderControl control;
    control.attach(&encoder);

    encoder.hold_encode.store(true);
    std::thread encode_thread([&] {
        EncodedPacket packet;
        encode_once(control, packet);
    });
    while (!encoder.in_held_encode.load()) std::this_thread::yield();

    auto setter = std::async(std::launch::async, [&] {
        control.set_bitrate(4000);
        control.request_keyframe();
    });
    const bool returned = setter.wait_for(std::chrono::seconds(5)) == std::future_status::ready;
    EXPECT_TRUE(returned) << "set_bitrate waited for the encode";
    EXPECT_EQ(encoder.rate_a.load(), 1000u) << "applied beside a running encode";
    EXPECT_TRUE(control.has_pending());

    encoder.hold_encode.store(false);
    encode_thread.join();
    setter.wait();

    // The encode thread applies the value on its next wake.
    control.apply_pending();
    EXPECT_EQ(encoder.rate_a.load(), 4000u);
    EXPECT_EQ(encoder.keyframes.load(), 1);
    EXPECT_FALSE(control.has_pending());
    EXPECT_EQ(encoder.overlaps.load(), 0);
}

TEST(EncoderControl, AnIdleEncoderTakesTheValueAtOnce) {
    TornStateEncoder encoder;
    EncoderControl control;
    control.attach(&encoder);
    control.set_bitrate(2500);
    EXPECT_EQ(encoder.rate_a.load(), 2500u);
    EXPECT_FALSE(control.has_pending());

    control.attach(nullptr);
    control.set_bitrate(800);  // no encoder: nothing to call
    EXPECT_EQ(encoder.rate_a.load(), 2500u);
}

// ─────────────────────────────────────────────────────────────────────────────
// NVDEC host copy. Before stage 2 the NV12 fallback buffer was w*h*3/2 and the
// copy wrote w*coded_h*3/2 bytes: 1920 * 8 * 1.5 = 23040 bytes past the end at
// 1080p. The R8 buffer was sized at start, so a stream with a larger coded
// height also wrote past it.
// ─────────────────────────────────────────────────────────────────────────────

TEST(NvdecHostCopy, TheNv12FallbackStaysInsideItsBufferAt1080p) {
    const auto plan = plan_nvdec_host_copy(1920, 1080, 1088);
    EXPECT_EQ(plan.buffer_bytes, 1920u * 1080u * 3u / 2u);
    EXPECT_LE(plan.bytes_written(1920), plan.buffer_bytes);
    EXPECT_EQ(plan.luma_rows, 1080u);
    EXPECT_EQ(plan.chroma_src_row, 1088u) << "the CUDA surface has its chroma at the coded height";
    EXPECT_EQ(plan.chroma_dst_row, 1080u) << "an NV12 texture has its chroma after the last row";
    EXPECT_EQ(plan.chroma_rows, 540u);
}

TEST(NvdecHostCopy, TheR8LayoutKeepsItsChromaRow) {
    const auto plan = plan_nvdec_host_copy(1920, 1088, 1088);
    EXPECT_EQ(plan.buffer_bytes, 1920u * 1632u);
    EXPECT_EQ(plan.bytes_written(1920), plan.buffer_bytes);
    EXPECT_EQ(plan.chroma_dst_row, 1088u) << "the shader reads the chroma at the start uv offset";
}

TEST(NvdecHostCopy, ALargerStreamCannotWritePastTheBuffer) {
    for (const uint32_t coded_h : {720u, 1088u, 1440u, 2160u}) {
        for (const uint32_t layout : {720u, 1080u, 1088u}) {
            const auto plan = plan_nvdec_host_copy(1920, layout, coded_h);
            EXPECT_LE(plan.bytes_written(1920), plan.buffer_bytes)
                << "coded " << coded_h << " layout " << layout;
            EXPECT_EQ(plan.chroma_src_row, coded_h);
            EXPECT_LE(plan.luma_rows, layout);
        }
    }
}

// Idle keepalive: a quiet capture still needs video, or a viewer who joins a
// static stream waits for a keyframe that never comes.
TEST(IdleKeepalive, NothingToRepeatBeforeTheFirstFrame) {
    EXPECT_FALSE(VideoPipeline::idle_repeat_due(false, true, 10'000'000, 0, 0));
}

TEST(IdleKeepalive, NoRepeatWhileFramesFlow) {
    const uint64_t now = 10'000'000;
    EXPECT_FALSE(VideoPipeline::idle_repeat_due(true, false, now, now - 16'000, now - 16'000));
}

TEST(IdleKeepalive, RepeatsAfterTheIdleThreshold) {
    const uint64_t now = 10'000'000;
    const uint64_t last_new = now - VideoPipeline::kIdleAfterUs;
    EXPECT_TRUE(VideoPipeline::idle_repeat_due(true, false, now, last_new, last_new));
}

TEST(IdleKeepalive, RepeatsAtTheKeepaliveRateNotFaster) {
    const uint64_t now = 10'000'000;
    const uint64_t last_new = now - 5'000'000;
    EXPECT_FALSE(VideoPipeline::idle_repeat_due(true, false, now, last_new, now - 100'000));
    EXPECT_TRUE(VideoPipeline::idle_repeat_due(
        true, false, now, last_new, now - VideoPipeline::kIdleRepeatIntervalUs));
}

TEST(IdleKeepalive, KeyframeRequestOnAQuietStreamRepeatsAtOnce) {
    const uint64_t now = 10'000'000;
    // Quiet for 200 ms, last encode 10 ms ago: only the kick makes it due.
    EXPECT_FALSE(VideoPipeline::idle_repeat_due(true, false, now, now - 200'000, now - 10'000));
    EXPECT_TRUE(VideoPipeline::idle_repeat_due(true, true, now, now - 200'000, now - 10'000));
}

TEST(IdleKeepalive, KeyframeRequestWhileFramesFlowWaitsForTheNextFrame) {
    const uint64_t now = 10'000'000;
    EXPECT_FALSE(VideoPipeline::idle_repeat_due(true, true, now, now - 16'000, now - 16'000));
}

#ifdef _WIN32
#include "video/capture_process.hpp"

// Capture ladder: a method that does not deliver video fails; a method that
// went quiet after delivering is a static screen and stays.
TEST(CaptureLadder, SilentFromTheStartFailsAfterTheDeadline) {
    const uint64_t started = 1'000'000;
    for (bool continuous : {false, true}) {
        EXPECT_FALSE(ladder::startup_failed(continuous, 0, started, started + 1'999'999));
        EXPECT_TRUE(ladder::startup_failed(continuous, 0, started,
                                           started + ladder::kFirstFrameDeadlineUs));
    }
}

// Desktop duplication hands over one initial desktop image and then goes
// silent under a game. Measured against Unigine Heaven on 2026-09-16: the
// ladder accepted DXGI forever on the strength of that single frame.
TEST(CaptureLadder, OneFrameThenSilenceIsAFailureForDuplication) {
    const uint64_t started = 1'000'000;
    EXPECT_FALSE(ladder::startup_failed(true, 1, started, started + 2'999'999));
    EXPECT_TRUE(ladder::startup_failed(true, 1, started, started + ladder::kProbationUs));
}

// Window capture delivers a frame only when the captured content changes.
// Measured against Unigine Heaven on 2026-09-16: the game sat on a static
// screen, window capture delivered one frame, and the probation rule threw
// away a method that worked. Only desktop duplication gets that rule.
TEST(CaptureLadder, OneFrameThenSilenceIsNormalForWindowCapture) {
    const uint64_t started = 1'000'000;
    EXPECT_FALSE(ladder::startup_failed(false, 1, started, started + 1'800'000'000ULL));
    EXPECT_FALSE(ladder::expects_continuous_frames(LadderStep::WgcWindow));
    EXPECT_FALSE(ladder::expects_continuous_frames(LadderStep::WgcMonitor));
    EXPECT_TRUE(ladder::expects_continuous_frames(LadderStep::Dxgi));
}

// A person starts the stream and then goes to their game. Loading, switching
// window and taking fullscreen all take seconds, and the hook reports that the
// game has drawn nothing at all in that time. Moving the ladder on there cost a
// real user a stream of their desktop on 2026-09-16.
TEST(CaptureLadder, AMethodWaitsWhileTheGameHasDrawnNothing) {
    const uint64_t started = 1'000'000;

    // Without that knowledge, two seconds of silence is a failure.
    EXPECT_TRUE(ladder::startup_failed(false, 0, started,
                                       started + ladder::kFirstFrameDeadlineUs, false));

    // With it, the same silence is a game that has not drawn yet.
    EXPECT_FALSE(ladder::startup_failed(false, 0, started,
                                        started + ladder::kFirstFrameDeadlineUs, true));
    EXPECT_FALSE(ladder::startup_failed(false, 0, started,
                                        started + ladder::kWaitingForGameUs - 1, true));

    // The wait is bounded: a game that draws through an API the hook does not
    // cover is silent in exactly the same way, and stays silent.
    EXPECT_TRUE(ladder::startup_failed(false, 0, started,
                                       started + ladder::kWaitingForGameUs, true));
}

TEST(CaptureLadder, DeliveringMethodIsNeverFailed) {
    const uint64_t started = 1'000'000;
    // Delivered its quota, then 30 minutes of nothing: a paused game.
    EXPECT_FALSE(ladder::startup_failed(true, ladder::kProbationFrames, started,
                                        started + 1'800'000'000ULL));
}

TEST(CaptureLadder, ClockBeforeStepStartIsNotOverdue) {
    EXPECT_FALSE(ladder::startup_failed(true, 0, 5'000'000, 1'000'000));
}

TEST(CaptureLadder, EveryMethodIsInTheOrder) {
    auto order = ladder::initial_order(false);
    ASSERT_EQ(order.size(), 3u);
    EXPECT_NE(std::find(order.begin(), order.end(), LadderStep::Dxgi), order.end());
    EXPECT_NE(std::find(order.begin(), order.end(), LadderStep::WgcWindow), order.end());
    EXPECT_NE(std::find(order.begin(), order.end(), LadderStep::WgcMonitor), order.end());
}

TEST(CaptureLadder, WindowCaptureRunsFirstAndDuplicationLast) {
    // Measured on 2026-09-16 against a fullscreen game: desktop duplication
    // delivered one frame, then nothing, then wedged in the display driver and
    // left the device unusable. Window capture ran the same game at 48 fps.
    auto order = ladder::initial_order(false);
    EXPECT_EQ(order.front(), LadderStep::WgcWindow);
    EXPECT_EQ(order.back(), LadderStep::Dxgi);
}

// The hook is the only method that sees an exclusive-fullscreen game, so it
// goes first - but only when the caller allows it. A client with no capture
// policy from the backend never hooks anything (plan 3.6).
TEST(CaptureLadder, TheHookRunsFirstOnlyWhenTheCallerAllowsIt) {
    auto without = ladder::initial_order(false);
    EXPECT_EQ(std::find(without.begin(), without.end(), LadderStep::Hook), without.end());

    auto with = ladder::initial_order(true);
    ASSERT_EQ(with.size(), 4u);
    EXPECT_EQ(with.front(), LadderStep::Hook);
    EXPECT_EQ(with.back(), LadderStep::Dxgi);
}

// The hook delivers a frame for every present, like duplication, but it sees
// only the game. A game that renders nothing is silent on it, and that is not
// a failure.
TEST(CaptureLadder, TheHookIsNotJudgedOnFrameCount) {
    EXPECT_FALSE(ladder::expects_continuous_frames(LadderStep::Hook));
}

namespace {

// A capture backend with no device. It delivers a frame every few
// milliseconds on its own thread, after a first-frame delay, the way the hook
// delivers after its injection and the game's next present.
class FakeLadderBackend : public CaptureSource {
public:
    struct Shared {
        std::atomic<bool> running{false};
        std::atomic<int>  starts{0};
    };

    FakeLadderBackend(uintptr_t id, int first_frame_delay_ms, bool delivers, Shared* shared)
        : id_(id), delay_ms_(first_frame_delay_ms), delivers_(delivers), shared_(shared) {}
    ~FakeLadderBackend() override { stop(); }

    bool initialize(const GraphicsDevice&, const CaptureSourceDesc&) override { return true; }
    bool start(uint32_t, FrameCallback callback) override {
        running_ = true;
        shared_->running = true;
        shared_->starts.fetch_add(1);
        thread_ = std::thread([this, callback] {
            const auto first = std::chrono::steady_clock::now() +
                               std::chrono::milliseconds(delay_ms_);
            while (running_.load() && std::chrono::steady_clock::now() < first) {
                std::this_thread::sleep_for(std::chrono::milliseconds(1));
            }
            while (running_.load()) {
                if (delivers_) {
                    const uint64_t now = static_cast<uint64_t>(
                        std::chrono::duration_cast<std::chrono::microseconds>(
                            std::chrono::steady_clock::now().time_since_epoch())
                            .count());
                    callback(reinterpret_cast<ID3D11Texture2D*>(id_), now);
                }
                std::this_thread::sleep_for(std::chrono::milliseconds(4));
            }
        });
        return true;
    }
    void stop() override {
        running_ = false;
        if (thread_.joinable()) thread_.join();
        shared_->running = false;
    }
    uint32_t width() const override { return 640; }
    uint32_t height() const override { return 360; }
    const char* backend_name() const override { return id_ == 1 ? "Fake-old" : "Fake-next"; }

private:
    uintptr_t         id_;
    int               delay_ms_;
    bool              delivers_;
    Shared*           shared_;
    std::atomic<bool> running_{false};
    std::thread       thread_;
};

struct Delivery {
    uintptr_t id;
    std::chrono::steady_clock::time_point at;
};

// Records what reached the pipeline, and whether two backends ever reached it
// at the same time.
struct DeliveryLog {
    std::mutex            mutex;
    std::vector<Delivery> frames;
    std::atomic<int>      inside{0};
    std::atomic<bool>     overlapped{false};

    CaptureSource::FrameCallback callback() {
        return [this](ID3D11Texture2D* texture, uint64_t) {
            if (inside.fetch_add(1) != 0) overlapped = true;
            {
                std::lock_guard<std::mutex> lock(mutex);
                frames.push_back({reinterpret_cast<uintptr_t>(texture),
                                  std::chrono::steady_clock::now()});
            }
            inside.fetch_sub(1);
        };
    }

    // The longest time with no frame at all, between `from` and `to`.
    std::chrono::milliseconds longest_gap(std::chrono::steady_clock::time_point from,
                                          std::chrono::steady_clock::time_point to) {
        std::lock_guard<std::mutex> lock(mutex);
        auto last = from;
        std::chrono::steady_clock::duration longest{0};
        for (const Delivery& d : frames) {
            if (d.at < from || d.at > to) continue;
            longest = (std::max)(longest, d.at - last);
            last = d.at;
        }
        longest = (std::max)(longest, to - last);
        return std::chrono::duration_cast<std::chrono::milliseconds>(longest);
    }

    size_t count_from(uintptr_t id, std::chrono::steady_clock::time_point after) {
        std::lock_guard<std::mutex> lock(mutex);
        return static_cast<size_t>(std::count_if(frames.begin(), frames.end(), [&](const Delivery& d) {
            return d.id == id && d.at > after;
        }));
    }
};

}  // namespace

// A hook attempt takes seconds before its first frame: the injection, then the
// game's next present. The ladder stopped the running method first, so the
// viewer saw no frames for all that time, every 30 s while a game sat in
// exclusive fullscreen (scorecard 4.1). The next method now starts beside the
// old one, which keeps the stream until the next one delivers.
TEST(CaptureLadder, ASwapToASlowMethodLeavesNoGap) {
    FakeLadderBackend::Shared old_state, next_state;
    ProcessCapture capture;
    ASSERT_TRUE(capture.use_steps_for_test(
        {LadderStep::Hook, LadderStep::WgcWindow},
        [&](LadderStep step) -> std::unique_ptr<CaptureSource> {
            if (step == LadderStep::WgcWindow) {
                return std::make_unique<FakeLadderBackend>(1, 0, true, &old_state);
            }
            return std::make_unique<FakeLadderBackend>(2, 400, true, &next_state);
        },
        /*start_index=*/1));

    DeliveryLog log;
    ASSERT_TRUE(capture.start(60, log.callback()));
    std::this_thread::sleep_for(std::chrono::milliseconds(150));

    const auto moved_at = std::chrono::steady_clock::now();
    ASSERT_TRUE(capture.move_for_test(0, /*keep_old_until_first_frame=*/true));
    std::this_thread::sleep_for(std::chrono::milliseconds(200));
    const auto checked_at = std::chrono::steady_clock::now();

    // The fakes deliver every 4 ms (16 ms with a coarse timer). The next
    // method takes 400 ms for its first frame; stopping the old one first
    // shows that whole wait as a gap.
    EXPECT_LT(log.longest_gap(moved_at - std::chrono::milliseconds(100), checked_at).count(), 100)
        << "the stream had no frames while the next method started";
    EXPECT_STREQ(capture.backend_name(), "Fake-next");
    EXPECT_GT(log.count_from(2, moved_at), 0u) << "the next method never reached the pipeline";
    EXPECT_FALSE(old_state.running.load()) << "the old method kept running after the swap";
    EXPECT_FALSE(log.overlapped.load()) << "two methods reached the pipeline at once";
    EXPECT_TRUE(capture.consume_swap_event());

    capture.stop();
}

// A better method that never delivers costs nothing: the method that has the
// stream keeps delivering through the whole attempt and after it.
TEST(CaptureLadder, AFailedSwapKeepsTheOldMethodDelivering) {
    FakeLadderBackend::Shared old_state, next_state;
    ProcessCapture capture;
    ASSERT_TRUE(capture.use_steps_for_test(
        {LadderStep::Hook, LadderStep::WgcWindow},
        [&](LadderStep step) -> std::unique_ptr<CaptureSource> {
            if (step == LadderStep::WgcWindow) {
                return std::make_unique<FakeLadderBackend>(1, 0, true, &old_state);
            }
            return std::make_unique<FakeLadderBackend>(2, 0, /*delivers=*/false, &next_state);
        },
        /*start_index=*/1));

    DeliveryLog log;
    ASSERT_TRUE(capture.start(60, log.callback()));
    std::this_thread::sleep_for(std::chrono::milliseconds(100));

    const auto moved_at = std::chrono::steady_clock::now();
    EXPECT_FALSE(capture.move_for_test(0, /*keep_old_until_first_frame=*/true));
    std::this_thread::sleep_for(std::chrono::milliseconds(100));
    const auto checked_at = std::chrono::steady_clock::now();

    EXPECT_LT(log.longest_gap(moved_at, checked_at).count(), 100)
        << "the old method stopped delivering during the attempt";
    EXPECT_EQ(next_state.starts.load(), 1) << "the next method was not tried";
    EXPECT_FALSE(next_state.running.load()) << "the failed method was left running";
    EXPECT_TRUE(old_state.running.load());
    EXPECT_EQ(old_state.starts.load(), 1) << "the old method was stopped and started again";
    EXPECT_STREQ(capture.backend_name(), "Fake-old");
    EXPECT_FALSE(capture.consume_swap_event());

    capture.stop();
}

// What a still stream tells the person watching. A wrong answer here shows a
// pause card over a live game, or leaves a frozen picture unexplained.
TEST(CaptureState, TheHookKeepsStreamingAMinimizedGame) {
    using ladder::capture_state_for;
    // Minimized, on the hook: the frame is taken inside the game, so the
    // stream is live and there is nothing to tell the viewer.
    EXPECT_EQ(capture_state_for(false, false, false, true, false, false),
              CaptureState::Capturing);
    // Minimized, on screen capture: the viewer is looking at a still picture.
    EXPECT_EQ(capture_state_for(false, false, false, false, false, false),
              CaptureState::WaitingMinimized);
}

TEST(CaptureState, WaitingAndFailedAreDifferentSentences) {
    using ladder::capture_state_for;
    EXPECT_EQ(capture_state_for(false, true, false, true, true, false),
              CaptureState::WaitingMinimized)
        << "the stream began with the game minimized, so nothing started";
    EXPECT_EQ(capture_state_for(false, false, true, true, true, false),
              CaptureState::WaitingForGame);
    EXPECT_EQ(capture_state_for(true, false, false, true, true, false), CaptureState::Failed)
        << "a proven failure outranks every wait";
    EXPECT_EQ(capture_state_for(false, false, false, false, true, false),
              CaptureState::Capturing);
}

// The pause card is method-agnostic: an exclusive-fullscreen game on a
// screen-level method is blind with OS proof, hook or no hook.
TEST(CaptureState, ABlindMethodInExclusiveFullscreenIsFailedWithProof) {
    using ladder::capture_state_for;
    // WGC window serving an exclusive-fullscreen game: the compositor frames
    // are the desktop, not the game, and QUNS plus the foreground pid prove
    // the game is drawing.
    EXPECT_EQ(capture_state_for(false, false, false, false, true, true), CaptureState::Failed);
    // Same proof on DXGI: also blind, also failed.
    EXPECT_EQ(capture_state_for(false, false, false, false, true, true), CaptureState::Failed);
    // The hook sees exclusive fullscreen from inside the game: still live.
    EXPECT_EQ(capture_state_for(false, false, false, true, true, true), CaptureState::Capturing)
        << "captures_while_minimized short-circuits before the fullscreen proof";
    // Windowed and borderless never report fullscreen: a live game stays live.
    EXPECT_EQ(capture_state_for(false, false, false, false, true, false),
              CaptureState::Capturing);
    // A minimized game that is not fullscreen waits, it does not fail.
    EXPECT_EQ(capture_state_for(false, false, false, false, false, false),
              CaptureState::WaitingMinimized);
}

#include "video/hook_policy.hpp"
#include <windows.h>
#include <psapi.h>

// Choosing what to capture from what the user picked. A window picker lists
// every window a game has, including ones that can never carry a stream.
TEST(CaptureTarget, AProxyWindowCannotCarryAStream) {
    // Direct3D 9 leaves this behind when a game takes exclusive fullscreen.
    // Measured on 2026-09-16: picking Unigine Heaven in the window list gave a
    // 160x28 D3DProxyWindow and the stream refused to start.
    EXPECT_FALSE(window_is_capturable(160, 28, "D3DProxyWindow"));
    EXPECT_FALSE(window_is_capturable(1920, 1080, "D3DProxyWindow"))
        << "a proxy window is never the game, whatever size it claims";
    EXPECT_FALSE(window_is_capturable(1920, 1080, "d3dproxywindow"));
}

TEST(CaptureTarget, AWindowBelowTheEncoderMinimumCannotCarryAStream) {
    EXPECT_FALSE(window_is_capturable(kMinEncodeWidth - 1, 720, "UnigineWindowClass"));
    EXPECT_FALSE(window_is_capturable(1280, kMinEncodeHeight - 1, "UnigineWindowClass"));
    EXPECT_TRUE(window_is_capturable(kMinEncodeWidth, kMinEncodeHeight, "UnigineWindowClass"));
    EXPECT_TRUE(window_is_capturable(1280, 720, "UnigineWindowClass"));
}

// The run-time hook checks. Bob's rule is that the hook must not get anyone
// banned, so each of these is a refusal, and a refusal only costs a fallback to
// screen capture.
TEST(HookPolicy, KnownAntiCheatModulesAreRefused) {
    using namespace mello::video::hook;
    EXPECT_TRUE(is_anticheat_module("EasyAntiCheat_x64.dll"));
    EXPECT_TRUE(is_anticheat_module("C:\\Games\\x\\BEClient_x64.dll"));
    EXPECT_TRUE(is_anticheat_module("ACE-BASE.dll"));
    EXPECT_TRUE(is_anticheat_module("vgk.sys"));
    EXPECT_TRUE(is_anticheat_module("mhyprot3.sys"));
    EXPECT_TRUE(is_anticheat_module("xhunter1.sys"));

    EXPECT_FALSE(is_anticheat_module("d3d11.dll"));
    EXPECT_FALSE(is_anticheat_module("Heaven.exe"));
    EXPECT_FALSE(is_anticheat_module(""));

    // The names are matched from the start. A module that merely contains the
    // letters of a short pattern is not an anti-cheat, and refusing it would
    // cost every player of that game the hook.
    EXPECT_FALSE(is_anticheat_module("reach.dll"));
    EXPECT_FALSE(is_anticheat_module("svgc_helper.dll"));
    EXPECT_FALSE(is_anticheat_module("nvgameguardian.dll"));
}

TEST(HookPolicy, KnownAntiCheatProcessesAreRefused) {
    using namespace mello::video::hook;
    EXPECT_TRUE(is_anticheat_process("EasyAntiCheat.exe"));
    EXPECT_TRUE(is_anticheat_process("BEService.exe"));
    EXPECT_TRUE(is_anticheat_process("vgc.exe"));
    EXPECT_FALSE(is_anticheat_process("explorer.exe"));
}

TEST(HookPolicy, StorePackagedAndChromiumGamesAreRefused) {
    using namespace mello::video::hook;
    EXPECT_TRUE(is_store_packaged("C:\\Program Files\\WindowsApps\\Game_1.0\\game.exe"));
    EXPECT_FALSE(is_store_packaged("C:\\Games\\game.exe"));

    EXPECT_TRUE(is_chromium_window_class("Chrome_WidgetWin_1"));
    EXPECT_TRUE(is_chromium_window_class("Chrome_WidgetWin_0"));
    EXPECT_FALSE(is_chromium_window_class("UnigineWindowClass"));
}

// The developer override skips the backend hook policy for one executable,
// and a partial name must not widen it to other games.
TEST(HookPolicy, TheDeveloperOverrideNamesOneExecutable) {
    using namespace mello::video::hook;
    EXPECT_TRUE(developer_allows("Heaven.exe", R"(C:\Games\Heaven\bin\Heaven.exe)"));
    EXPECT_TRUE(developer_allows("heaven.exe", R"(C:\Games\Heaven.exe)"));
    EXPECT_TRUE(developer_allows(R"(C:\Games\Heaven.exe)", R"(D:\other\Heaven.exe)"));

    EXPECT_FALSE(developer_allows("Heaven", R"(C:\Games\Heaven.exe)")) << "whole name only";
    EXPECT_FALSE(developer_allows("Heav", R"(C:\Games\Heaven.exe)"));
    EXPECT_FALSE(developer_allows("Heaven.exe", R"(C:\Games\HeavenBenchmark.exe)"));
    EXPECT_FALSE(developer_allows("", R"(C:\Games\Heaven.exe)")) << "unset allows nothing";
    EXPECT_FALSE(developer_allows("Heaven.exe", ""));
}

// The catalogue and the backend decide first. With no decision there is no
// hook, whatever the process looks like.
TEST(HookPolicy, NothingIsHookedWithoutTheCallersPermission) {
    using namespace mello::video::hook;
    const PolicyResult result = check_process(GetCurrentProcessId(), false);
    EXPECT_FALSE(result.allowed());
    EXPECT_EQ(result.verdict, PolicyVerdict::NotAllowedByCaller);
}

// A module list that cannot be read is not proof that no anti-cheat module is
// loaded. The policy must refuse the hook, not allow it (it did until stage 2).
// A process created suspended has no loader data yet, so EnumProcessModulesEx
// fails on it with ERROR_PARTIAL_COPY. Every other check passes for it.
TEST(HookPolicy, AnUnreadableModuleListIsRefused) {
    using namespace mello::video::hook;
    wchar_t command[] = L"cmd.exe /c exit 0";
    STARTUPINFOW startup{};
    startup.cb = sizeof(startup);
    PROCESS_INFORMATION child{};
    ASSERT_TRUE(CreateProcessW(nullptr, command, nullptr, nullptr, FALSE,
                               CREATE_SUSPENDED | CREATE_NO_WINDOW, nullptr, nullptr, &startup,
                               &child))
        << "CreateProcessW failed: " << GetLastError();
    struct Reaper {
        PROCESS_INFORMATION& info;
        ~Reaper() {
            TerminateProcess(info.hProcess, 0);
            WaitForSingleObject(info.hProcess, 5000);
            CloseHandle(info.hThread);
            CloseHandle(info.hProcess);
        }
    } reaper{child};

    HMODULE first = nullptr;
    DWORD needed = 0;
    if (EnumProcessModulesEx(child.hProcess, &first, sizeof(first), &needed, LIST_MODULES_ALL)) {
        GTEST_SKIP() << "this Windows build reads the module list of a suspended process";
    }

    const PolicyResult result = check_process(child.dwProcessId, true);
    EXPECT_FALSE(result.allowed());
    EXPECT_EQ(result.verdict, PolicyVerdict::ProcessUnreadable) << verdict_name(result.verdict);
}
#endif

// ─────────────────────────────────────────────────────────────────────────────
// Native present for NV12 decoders. The Windows client registers no CPU frame
// callback, so a viewer sees only what reaches the native callback, through
// StagingTexture::shared_rgba_handle(). Before stage 2 that handle existed only
// for the R8 layout of NVDEC. AMF and OpenH264 emit NV12, so
// present_frame returned false on every frame: no picture on any viewer
// without an NVIDIA GPU. No hardware decoder here: an NV12 texture with a
// known colour stands in for the decoded frame.
// ─────────────────────────────────────────────────────────────────────────────
#ifdef _WIN32
#include "video/staging_texture.hpp"
#include <wrl/client.h>

namespace {

struct TestDevice {
    GraphicsDevice device = create_d3d11_device();
    ~TestDevice() {
        if (device.handle) device.d3d11()->Release();
    }
};

Microsoft::WRL::ComPtr<ID3D11Texture2D> make_nv12(ID3D11Device* device, uint32_t w, uint32_t h,
                                                  uint8_t y, uint8_t u, uint8_t v) {
    std::vector<uint8_t> data(static_cast<size_t>(w) * h * 3 / 2);
    std::fill(data.begin(), data.begin() + static_cast<size_t>(w) * h, y);
    for (size_t i = static_cast<size_t>(w) * h; i + 1 < data.size(); i += 2) {
        data[i] = u;
        data[i + 1] = v;
    }
    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = w;
    desc.Height = h;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_NV12;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;  // as every NV12 decoder creates it
    D3D11_SUBRESOURCE_DATA init{data.data(), w, 0};
    Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
    device->CreateTexture2D(&desc, &init, &texture);
    return texture;
}

// The integer BT.709 studio-swing conversion of the CPU path.
uint8_t expected_channel(int value) { return static_cast<uint8_t>(std::clamp(value, 0, 255)); }

bool nv12_shader_input_supported(ID3D11Device* device) {
    UINT support = 0;
    if (FAILED(device->CheckFormatSupport(DXGI_FORMAT_NV12, &support))) return false;
    return (support & D3D11_FORMAT_SUPPORT_TEXTURE2D) && (support & D3D11_FORMAT_SUPPORT_SHADER_LOAD);
}

} // namespace

TEST(NativePresent, AnNv12DecoderGetsASharedRgbaFrame) {
    TestDevice test;
    if (!test.device.handle) GTEST_SKIP() << "no D3D11 device";
    if (!nv12_shader_input_supported(test.device.d3d11())) {
        GTEST_SKIP() << "this adapter cannot read NV12 in a shader";
    }

    constexpr uint32_t kW = 64, kH = 32;
    const uint8_t y = 63, u = 102, v = 240;  // a red, studio swing
    auto texture = make_nv12(test.device.d3d11(), kW, kH, y, u, v);
    ASSERT_TRUE(texture);

    DecodedPicture picture;
    picture.texture = texture;
    picture.format = DXGI_FORMAT_NV12;
    picture.width = kW;
    picture.height = kH;

    // The Windows client: a native callback and no CPU readback.
    StagingTexture native_only;
    ASSERT_TRUE(native_only.initialize(test.device, false));
    ASSERT_TRUE(native_only.convert(picture, false));
    EXPECT_NE(native_only.shared_rgba_handle(), nullptr)
        << "an NV12 decoder has no native present path";
    native_only.shutdown();

    // The same conversion, read back to check the picture.
    StagingTexture staging;
    ASSERT_TRUE(staging.initialize(test.device, true));
    ASSERT_TRUE(staging.convert(picture, true));
    ASSERT_NE(staging.shared_rgba_handle(), nullptr);
    std::vector<uint8_t> rgba(static_cast<size_t>(kW) * kH * 4);
    ASSERT_TRUE(staging.read_rgba(rgba.data()));
    staging.shutdown();

    const int c = y - 16, d = u - 128, e = v - 128;
    const uint8_t r = expected_channel((298 * c + 459 * e + 128) >> 8);
    const uint8_t g = expected_channel((298 * c - 55 * d - 136 * e + 128) >> 8);
    const uint8_t b = expected_channel((298 * c + 541 * d + 128) >> 8);
    for (const auto [px, py] : {std::pair<uint32_t, uint32_t>{0, 0}, {kW / 2, kH / 2},
                                {kW - 1, kH - 1}, {kW - 1, 0}}) {
        const size_t at = (static_cast<size_t>(py) * kW + px) * 4;
        EXPECT_NEAR(rgba[at + 0], r, 2) << "red at " << px << "," << py;
        EXPECT_NEAR(rgba[at + 1], g, 2) << "green at " << px << "," << py;
        EXPECT_NEAR(rgba[at + 2], b, 2) << "blue at " << px << "," << py;
        EXPECT_EQ(rgba[at + 3], 255);
    }
}
#endif // _WIN32

// Present-to-capture delay histogram used by the DXGI vs WGC benchmark.
TEST(PresentDelayHistogram, BucketsAreOneMillisecondWide) {
    EXPECT_EQ(PresentDelayHistogram::bucket_for_ms(0.4), 0u);
    EXPECT_EQ(PresentDelayHistogram::bucket_for_ms(1.0), 1u);
    EXPECT_EQ(PresentDelayHistogram::bucket_for_ms(8.9), 8u);
}

TEST(PresentDelayHistogram, NegativeAndLargeDelaysAreClamped) {
    // A frame timestamp slightly in the future (clock rounding) is not a crash.
    EXPECT_EQ(PresentDelayHistogram::bucket_for_ms(-3.0), 0u);
    EXPECT_EQ(PresentDelayHistogram::bucket_for_ms(500.0), PresentDelayHistogram::kBuckets - 1);
}

TEST(PresentDelayHistogram, SnapshotIsCumulative) {
    PresentDelayHistogram h;
    h.record_ms(2.5);
    h.record_ms(2.1);
    h.record_ms(40.0);
    uint32_t out[PresentDelayHistogram::kBuckets]{};
    h.snapshot(out);
    EXPECT_EQ(out[2], 2u);
    EXPECT_EQ(out[PresentDelayHistogram::kBuckets - 1], 1u);
}

// The ladder is Windows-only (capture_process.hpp is _WIN32-guarded), so the
// tests below are too. They lived past the mid-file #endif unguarded and
// broke the macOS build on the first CI run that compiled this branch there.
#ifdef _WIN32
TEST(CaptureLadder, TheFirstFrameDeadlineIsTheSameForEveryMethod) {
    // A method that delivers nothing at all gets exactly one chance, whichever
    // method it is.
    const uint64_t started = 0;
    EXPECT_TRUE(ladder::startup_failed(true, 0, started, ladder::kFirstFrameDeadlineUs));
    EXPECT_FALSE(ladder::startup_failed(true, 2, started, ladder::kFirstFrameDeadlineUs));
    EXPECT_TRUE(ladder::startup_failed(true, 2, started, ladder::kProbationUs));
}

// A quiet stream is only an error when the game is provably rendering. A
// visible game that renders nothing looks exactly like a blind capture method,
// and telling that user to change a setting would be wrong.
TEST(CaptureLadder, QuietGameIsNotReportedAsAFailure) {
    EXPECT_FALSE(ladder::should_report_failure(false));
}

TEST(CaptureLadder, ExclusiveFullscreenIsReported) {
    EXPECT_TRUE(ladder::should_report_failure(true));
}
#endif // _WIN32 ladder tests

#ifdef _WIN32
#include "video/encoder_amf.hpp"
#include "video/encoder_nvenc.hpp"
#include "video/encoder_qsv.hpp"

// The cost tiers ran on NVENC only: an AMD or Intel host that could not hold
// the frame budget dropped frames with no quality feature given up. AMF and
// QSV now have tiers driven by the same overload signal, and the pipeline stops
// at each backend's own last tier.
TEST(EncoderCostTier, EveryHardwareBackendHasTwoTiers) {
    EXPECT_EQ(NvencEncoder().max_cost_tier(), 2);
    EXPECT_EQ(AmfEncoder().max_cost_tier(), 2);
    EXPECT_EQ(QsvEncoder().max_cost_tier(), 2);
}

TEST(EncoderCostTier, AmfTakesThePresetDownThenTheAdaptiveFeaturesOff) {
    const auto t0 = AmfEncoder::tier_settings(0);
    EXPECT_EQ(t0.quality_preset, -1) << "tier 0 is the init configuration";
    EXPECT_FALSE(t0.adaptive_off);

    const auto t1 = AmfEncoder::tier_settings(1);
    EXPECT_EQ(t1.quality_preset, AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED);
    EXPECT_FALSE(t1.adaptive_off) << "one feature at a time";

    const auto t2 = AmfEncoder::tier_settings(2);
    EXPECT_EQ(t2.quality_preset, AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED)
        << "a later tier keeps what an earlier one gave up";
    EXPECT_TRUE(t2.adaptive_off);
}

TEST(EncoderCostTier, QsvTargetUsageOnlyGoesTowardsSpeed) {
    EXPECT_EQ(QsvEncoder::target_usage_for_tier(0), MFX_TARGETUSAGE_BALANCED);
    EXPECT_EQ(QsvEncoder::target_usage_for_tier(2), MFX_TARGETUSAGE_BEST_SPEED);
    for (int tier = 0; tier < QsvEncoder::kMaxCostTier; ++tier) {
        EXPECT_GT(QsvEncoder::target_usage_for_tier(tier + 1),
                  QsvEncoder::target_usage_for_tier(tier));
    }
}

// Streaming-quality stage 4, backend parity: every backend's VBV is half a
// second of the peak rate (1.25 x the bitrate), at least four frames. AMF and
// QSV had one frame (8 Mbps at 60 fps: 133 kbit), which starved IDRs.
TEST(EncoderVbv, HalfASecondOfThePeakRateOnEveryBackend) {
    EXPECT_EQ(vbv_bits_for(8'000, 60), 5'000'000u);
    EXPECT_EQ(vbv_bits_for(5'000, 30), 3'125'000u);
    // Four frames when that is more: 300 kbps at 1 fps.
    EXPECT_EQ(vbv_bits_for(300, 1), 1'200'000u);
    EXPECT_EQ(QsvEncoder::vbv_kb_for(8'000, 60), 625u);
    EXPECT_EQ(QsvEncoder::vbv_kb_for(300, 60), 24u);
}

// QSV hands oneVPL a texture with no frame allocator and an unaligned height:
// it has never run on an Intel GPU, so the factory skips it unless
// MELLO_ENABLE_QSV=1 (stage 4, until an Intel box tests it).
TEST(EncoderFactory, QsvIsSkippedWithoutTheOptIn) {
    SetEnvironmentVariableA("MELLO_ENABLE_QSV", nullptr);
    EXPECT_FALSE(QsvEncoder::enabled());
    EXPECT_FALSE(QsvEncoder::is_available());
    SetEnvironmentVariableA("MELLO_ENABLE_QSV", "1");
    EXPECT_TRUE(QsvEncoder::enabled());
    SetEnvironmentVariableA("MELLO_ENABLE_QSV", nullptr);
}

// An encoder without a session refuses a tier and stays at 0: the pipeline
// may ask before the first encode.
TEST(EncoderCostTier, AnUninitialisedEncoderRefusesATier) {
    AmfEncoder amf;
    EXPECT_FALSE(amf.reduce_cost_tier());
    EXPECT_EQ(amf.cost_tier(), 0);
    QsvEncoder qsv;
    EXPECT_FALSE(qsv.reduce_cost_tier());
    EXPECT_EQ(qsv.cost_tier(), 0);
}
#endif // _WIN32
