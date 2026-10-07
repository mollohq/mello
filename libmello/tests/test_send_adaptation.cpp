// Voice encoder adaptation to the send leg (spec 10 section 4.6).
//
// The controller gets a trace of receiver reports, one per second as the SFU
// sends them, and the tests assert the rung sequence it takes.

#include <gtest/gtest.h>

#include "audio/audio_pipeline.hpp"
#include "audio/send_adaptation.hpp"
#include "mello.h"

#include <algorithm>
#include <cstdint>
#include <mutex>
#include <random>
#include <utility>
#include <vector>

#ifdef _WIN32
#include <windows.h>
#endif

using mello::audio::SendAdaptation;

namespace {

struct Step {
    uint32_t expected;
    uint32_t lost;
    float rtt_ms;
};

// Feed one report per second from t = 1 s and return every rung change as
// (time in ms, new bitrate in kbps).
std::vector<std::pair<int64_t, int>> run_trace(SendAdaptation& c, const std::vector<Step>& trace,
                                               int64_t start_ms = 1000) {
    std::vector<std::pair<int64_t, int>> changes;
    int64_t t = start_ms;
    for (const auto& s : trace) {
        const int before = c.bitrate_kbps();
        c.on_report(t, s.expected, s.lost, s.rtt_ms);
        if (c.bitrate_kbps() != before) {
            changes.emplace_back(t, c.bitrate_kbps());
        }
        t += 1000;
    }
    return changes;
}

void repeat(std::vector<Step>& trace, int n, Step s) {
    for (int i = 0; i < n; ++i) trace.push_back(s);
}

TEST(SendAdaptation, LadderTopIsTheChannelBitrate) {
    SendAdaptation c;
    EXPECT_EQ(c.rungs(), (std::vector<int>{64, 48, 32, 24}));
    EXPECT_EQ(c.bitrate_kbps(), 64);
    c.set_max_kbps(96);
    EXPECT_EQ(c.rungs(), (std::vector<int>{96, 64, 48, 32, 24}));
    c.set_max_kbps(40);
    EXPECT_EQ(c.rungs(), (std::vector<int>{40, 32, 24}));
    c.set_max_kbps(200);
    EXPECT_EQ(c.max_kbps(), 96);
    c.set_max_kbps(8);
    EXPECT_EQ(c.rungs(), (std::vector<int>{24}));
}

TEST(SendAdaptation, StepsDownFastAndUpOneRungAfterTenCleanSeconds) {
    SendAdaptation c;  // channel bitrate 64
    std::vector<Step> trace;
    repeat(trace, 5, {50, 1, 40.0f});    // t 1..5 s: 2 % loss
    trace.push_back({50, 12, 40.0f});    // t 6: 13 % over the last 2 s
    trace.push_back({50, 10, 40.0f});    // t 7: 20 % since the step
    trace.push_back({50, 2, 40.0f});     // t 8: 4 %
    repeat(trace, 9, {50, 1, 40.0f});    // t 9..17: clean
    repeat(trace, 10, {50, 1, 40.0f});   // t 18..27: clean
    repeat(trace, 13, {50, 4, 40.0f});   // t 28..40: 8 %, between the thresholds
    trace.push_back({50, 1, 120.0f});    // t 41: the round trip triples
    repeat(trace, 20, {50, 4, 40.0f});   // t 42..61: 8 % again
    repeat(trace, 20, {0, 0, 40.0f});    // t 62..81: the speech gate is closed
    repeat(trace, 12, {50, 0, 40.0f});   // t 82..93: clean speech

    const auto changes = run_trace(c, trace);
    const std::vector<std::pair<int64_t, int>> want = {
        {6000, 48},   // loss over 2 s above 10 %: one rung down at once
        {7000, 32},   // still above 10 % in the second after the step
        {17000, 48},  // 10 s clean since the last step
        {27000, 64},  // 10 s clean since the step up; 64 is the top
        {41000, 48},  // round trip above 2 x the floor
        {86000, 64},  // silence is no evidence: the clean time (from 61 s)
                      // completes with 5 s of clean speech
    };
    EXPECT_EQ(changes, want);
    EXPECT_EQ(c.rung_changes(), want.size());
}

TEST(SendAdaptation, HoldsTheRungWhenLossSitsBetweenTheThresholds) {
    SendAdaptation c;
    c.set_max_kbps(96);
    std::vector<Step> trace;
    trace.push_back({50, 15, 30.0f});   // t 1: 30 %, down to 64
    repeat(trace, 60, {50, 4, 30.0f});  // a minute at 8 %
    const auto changes = run_trace(c, trace);
    EXPECT_EQ(changes, (std::vector<std::pair<int64_t, int>>{{1000, 64}}));
}

TEST(SendAdaptation, TooFewPacketsGiveNoDecision) {
    SendAdaptation c;
    std::vector<Step> trace;
    // 20 % loss, but 10 packets a second: under 25 per 2 s window only at
    // the first report; then the window holds 20 packets. No step.
    repeat(trace, 10, {10, 2, 40.0f});
    EXPECT_TRUE(run_trace(c, trace).empty());
    EXPECT_EQ(c.window_loss(), -1.0);
}

TEST(SendAdaptation, BottomRungStaysAndLanRoundTripsDoNotCount) {
    SendAdaptation c;
    c.set_max_kbps(24);
    std::vector<Step> trace;
    repeat(trace, 5, {50, 25, 2.0f});
    EXPECT_TRUE(run_trace(c, trace).empty());
    EXPECT_EQ(c.bitrate_kbps(), 24);

    // 2 ms to 5 ms is "double" but stays under 2 x 50 ms.
    SendAdaptation lan;
    std::vector<Step> lan_trace;
    repeat(lan_trace, 5, {50, 0, 2.0f});
    repeat(lan_trace, 5, {50, 0, 5.0f});
    EXPECT_TRUE(run_trace(lan, lan_trace).empty());
}

TEST(SendAdaptation, LossPercFollowsFourSecondsOfLossBetweenFloorAndCap) {
    SendAdaptation c;
    EXPECT_EQ(c.loss_perc(), 5);
    int64_t t = 1000;
    auto report = [&](uint32_t expected, uint32_t lost) {
        c.on_report(t, expected, lost, 40.0f);
        t += 1000;
    };
    for (int i = 0; i < 4; ++i) report(50, 1);  // 2 %: the floor
    EXPECT_EQ(c.loss_perc(), 5);
    report(50, 12);  // t 5: 15 of 200 over 4 s
    EXPECT_EQ(c.loss_perc(), 8);
    report(50, 10);  // t 6: 24 of 200
    EXPECT_EQ(c.loss_perc(), 12);
    for (int i = 0; i < 4; ++i) report(50, 40);  // 80 %: the cap
    EXPECT_EQ(c.loss_perc(), 30);
    for (int i = 0; i < 4; ++i) report(0, 0);  // no packets: no change
    EXPECT_EQ(c.loss_perc(), 30);
    for (int i = 0; i < 4; ++i) report(50, 0);
    EXPECT_EQ(c.loss_perc(), 5);
}

TEST(SendAdaptation, ResetGoesBackToTheTopAndTheFloor) {
    SendAdaptation c;
    std::vector<Step> trace;
    repeat(trace, 3, {50, 20, 40.0f});
    run_trace(c, trace);
    EXPECT_LT(c.bitrate_kbps(), 64);
    EXPECT_GT(c.loss_perc(), 5);
    c.reset();
    EXPECT_EQ(c.bitrate_kbps(), 64);
    EXPECT_EQ(c.loss_perc(), 5);

    // A lower channel bitrate moves the rung down to the new top.
    c.set_max_kbps(32);
    EXPECT_EQ(c.bitrate_kbps(), 32);
    // At the top, a higher channel bitrate is the new top.
    c.set_max_kbps(96);
    EXPECT_EQ(c.bitrate_kbps(), 96);

    // Stepped down, a new channel bitrate keeps the rung: the clean steps
    // climb from there.
    SendAdaptation d;
    d.on_report(1000, 50, 20, 40.0f);
    ASSERT_EQ(d.bitrate_kbps(), 48);
    d.set_max_kbps(96);
    EXPECT_EQ(d.bitrate_kbps(), 48);
    EXPECT_EQ(d.rungs(), (std::vector<int>{96, 64, 48, 32, 24}));
}

// The pipeline: a report given on another thread reaches the encoder at the
// next frame, and the frames shrink with the bitrate.

void select_test_backend() {
#ifdef _WIN32
    SetEnvironmentVariableA("MELLO_AUDIO_BACKEND", "test");
#else
    setenv("MELLO_AUDIO_BACKEND", "test", 1);
#endif
}

struct FrameSizes {
    std::mutex mutex;
    std::vector<int> sizes;
};

double mean_size(FrameSizes& f, size_t from) {
    std::lock_guard<std::mutex> lock(f.mutex);
    double sum = 0.0;
    for (size_t i = from; i < f.sizes.size(); ++i) sum += f.sizes[i];
    return f.sizes.size() > from ? sum / static_cast<double>(f.sizes.size() - from) : 0.0;
}

TEST(SendAdaptationPipeline, ReportsSetTheEncoderBitrateAndLossPerc) {
    select_test_backend();
    mello::audio::AudioPipeline pipeline;
    ASSERT_TRUE(pipeline.initialize());
    pipeline.set_push_to_talk(true);  // every unmuted frame is encoded
    pipeline.set_noise_suppression(false);
    ASSERT_TRUE(pipeline.start_capture_inject());
    FrameSizes frames;
    pipeline.set_packet_sink([&frames](const uint8_t*, int size, uint32_t, uint32_t) {
        std::lock_guard<std::mutex> lock(frames.mutex);
        frames.sizes.push_back(size);
    });

    // Loud white noise: Opus spends the whole bitrate on it.
    std::mt19937 rng(7);
    std::normal_distribution<double> noise(0.0, 6000.0);
    std::vector<int16_t> chunk(480);
    auto inject_seconds = [&](int seconds) {
        for (int i = 0; i < seconds * 100; ++i) {
            for (auto& s : chunk) s = static_cast<int16_t>(std::clamp(noise(rng), -32768.0, 32767.0));
            pipeline.inject_capture(chunk.data(), static_cast<int>(chunk.size()));
        }
    };

    inject_seconds(2);
    const double at_64 = mean_size(frames, 10);
    auto stats = pipeline.send_stats();
    EXPECT_EQ(stats.bitrate_kbps, 64);
    EXPECT_EQ(stats.loss_perc, 5);
    EXPECT_EQ(stats.reports, 0u);

    // Three reports of 40 % loss, one second apart on the playout clock:
    // three rungs down to 24 kbps, loss perc at the cap.
    mello_test_set_clock_ms(5000000);
    MelloSendFeedback fb{};
    fb.interval_expected = 50;
    fb.interval_lost = 20;
    fb.fraction_lost = 0.4f;
    fb.rtt_ms = 60.0f;
    for (int i = 0; i < 3; ++i) {
        mello_test_set_clock_ms(5000000 + 1000 * (i + 1));
        fb.reports = static_cast<uint32_t>(i + 1);
        pipeline.report_send_feedback(fb);
    }
    mello_test_set_clock_ms(-1);
    stats = pipeline.send_stats();
    EXPECT_EQ(stats.bitrate_kbps, 24);
    EXPECT_EQ(stats.loss_perc, 30);
    EXPECT_EQ(stats.reports, 3u);
    EXPECT_EQ(stats.rung_changes, 3u);
    EXPECT_NEAR(stats.feedback.rtt_ms, 60.0f, 1e-6f);

    const size_t before = [&] {
        std::lock_guard<std::mutex> lock(frames.mutex);
        return frames.sizes.size();
    }();
    inject_seconds(2);
    const double at_24 = mean_size(frames, before + 10);
    // 64 kbps is 160 bytes per 20 ms frame, 24 kbps is 60.
    EXPECT_GT(at_64, 120.0);
    EXPECT_LT(at_24, 75.0);

    // A new session starts at the top again.
    pipeline.set_packet_sink(nullptr);
    pipeline.stop_capture_inject();
    ASSERT_TRUE(pipeline.start_capture_inject());
    stats = pipeline.send_stats();
    EXPECT_EQ(stats.bitrate_kbps, 64);
    EXPECT_EQ(stats.loss_perc, 5);
    EXPECT_EQ(stats.reports, 0u);
    // The channel bitrate is the top rung.
    pipeline.set_max_bitrate_kbps(96);
    EXPECT_EQ(pipeline.send_stats().bitrate_kbps, 96);
    EXPECT_EQ(pipeline.send_stats().max_bitrate_kbps, 96);
    pipeline.stop_capture_inject();
    pipeline.shutdown();
}

}  // namespace
