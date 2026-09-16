#include <gtest/gtest.h>
#include "video/video_pipeline.hpp"
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

#include "video/hook_policy.hpp"

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

// The developer override stands in for the backend safe list. It names one
// executable, and a partial name must not widen it to other games.
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
#endif

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
