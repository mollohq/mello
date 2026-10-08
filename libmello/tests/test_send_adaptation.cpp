// Voice encoder adaptation to the send leg (spec 10 section 4.6).
//
// The controller gets a trace of receiver reports, one per second as the SFU
// sends them, and the tests assert the rung sequence it takes. Loss alone
// moves only the loss perc; the ladder needs loss with a rising round trip,
// a rising jitter, or no fresh round trip for two reports.

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
    float jitter_ms = 5.0f;
};

// One report into the controller. The only call site of on_report in the
// controller tests.
void feed(SendAdaptation& c, int64_t t, const Step& s) {
    c.on_report(t, s.expected, s.lost, s.rtt_ms, s.jitter_ms);
}

// Feed one report per second from t = 1 s and return every rung change as
// (time in ms, new bitrate in kbps).
std::vector<std::pair<int64_t, int>> run_trace(SendAdaptation& c, const std::vector<Step>& trace,
                                               int64_t start_ms = 1000) {
    std::vector<std::pair<int64_t, int>> changes;
    int64_t t = start_ms;
    for (const auto& s : trace) {
        const int before = c.bitrate_kbps();
        feed(c, t, s);
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

using Changes = std::vector<std::pair<int64_t, int>>;

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

TEST(SendAdaptation, StepsDownOnCongestionAndUpOneRungAfterTenCleanSeconds) {
    SendAdaptation c;  // channel bitrate 64
    std::vector<Step> trace;
    repeat(trace, 8, {50, 1, 40.0f});      // t 1..8: 2 % loss, 40 ms round trip
    trace.push_back({50, 15, 40.0f});      // t 9: 30 % loss, flat round trip
    repeat(trace, 4, {50, 15, 300.0f});    // t 10..13: 30 % loss, the queue fills
    repeat(trace, 20, {50, 0, 40.0f});     // t 14..33: the queue drains, clean
    repeat(trace, 12, {50, 7, 40.0f});     // t 34..45: 14 % random loss
    repeat(trace, 20, {0, 0, 40.0f});      // t 46..65: the speech gate is closed
    repeat(trace, 2, {50, 25, 40.0f, 30.0f});  // t 66..67: 50 % loss, jitter 6 x
    repeat(trace, 16, {50, 0, 40.0f});     // t 68..83: clean
    repeat(trace, 2, {50, 7, 40.0f});      // t 84..85: 14 % loss
    trace.push_back({50, 7, -1.0f});       // t 86: one report without a round trip
    repeat(trace, 2, {50, 7, 40.0f});      // t 87..88
    repeat(trace, 2, {50, 7, -1.0f});      // t 89..90: two in a row: the queue
                                           // drops our sender reports
    repeat(trace, 4, {50, 0, 40.0f});      // t 91..94: clean

    const auto changes = run_trace(c, trace);
    const Changes want = {
        {11000, 48},  // loss with the 3 s round-trip median above 2 x 30 s
        {13000, 32},  // two reports after the step: still congested
        {23000, 48},  // 10 s without congestion since the step down
        {33000, 64},  // and again; 64 is the top
                      // 14 % random loss with a flat round trip: no step
        {67000, 48},  // loss with the jitter median above 2 x its 30 s median
        {77000, 64},  // the step lowered the loss; 10 s clean since the step
                      // one report without a round trip: no step
        {90000, 48},  // loss and no fresh round trip for two reports
    };
    EXPECT_EQ(changes, want);
    EXPECT_EQ(c.rung_changes(), want.size());
}

TEST(SendAdaptation, RandomLossWithAFlatRoundTripAndFlatJitterDoesNotStepDown) {
    SendAdaptation c;
    std::vector<Step> trace;
    // 13 % loss (the voice gate's bad profile) for 90 s: a 160 ms round trip
    // with a 450 ms sample every 5 s (a stalled sender report), and a
    // jitter of 22 to 26 ms.
    for (int i = 0; i < 90; ++i) {
        const uint32_t lost = (i % 2 == 0) ? 6 : 7;
        const float rtt = (i % 5 == 4) ? 450.0f : 160.0f + static_cast<float>(i % 3) * 10.0f;
        const float jitter = 22.0f + static_cast<float>(i % 5);
        trace.push_back({50, lost, rtt, jitter});
    }
    EXPECT_TRUE(run_trace(c, trace).empty());
    EXPECT_EQ(c.bitrate_kbps(), 64);
    // The loss goes to the in-band FEC instead.
    EXPECT_EQ(c.loss_perc(), 13);
}

// The voice gate's congestion profile, stage 3b: the tail-drop queue that
// drops 11 % of the packets also drops our sender reports, so the receiver
// reports carry no fresh round trip, and the jitter stays flat. The
// round-trip test cannot see the queue; two reports in a row without a
// round trip and the loss are the evidence.
TEST(SendAdaptation, LossWithoutAFreshRoundTripForTwoReportsStepsDown) {
    SendAdaptation c;
    std::vector<Step> trace;
    repeat(trace, 10, {50, 0, 42.0f, 3.0f});   // t 1..10: clean
    trace.push_back({48, 7, -1.0f, 6.0f});     // t 11: 15 % loss, report dropped
    trace.push_back({50, 4, -1.0f, 6.5f});     // t 12: 8 %, dropped again
    repeat(trace, 6, {50, 0, 45.0f, 3.0f});    // t 13..18: clean
    const auto changes = run_trace(c, trace);
    // 11 of 98 over the two reports: 11.2 %. The jitter is 2 x 3 ms, under
    // the 10 ms floor; no round trip is inflated.
    EXPECT_EQ(changes, (Changes{{12000, 48}}));

    // At the start of a session, before any round trip: the same.
    SendAdaptation start;
    std::vector<Step> first;
    repeat(first, 2, {50, 8, -1.0f, 3.0f});
    EXPECT_EQ(run_trace(start, first), (Changes{{2000, 48}}));
}

TEST(SendAdaptation, AStepThatDoesNotLowerTheLossGoesBackAndHoldsTheLadder) {
    SendAdaptation c;
    std::vector<Step> trace;
    repeat(trace, 8, {50, 0, 40.0f, 5.0f});    // t 1..8: clean
    repeat(trace, 11, {50, 10, 40.0f, 30.0f});  // t 9..19: 20 % loss, jitter 6 x
    repeat(trace, 3, {50, 10, 400.0f, 30.0f});  // t 20..22: and the round trip 10 x
    repeat(trace, 38, {50, 10, 40.0f, 30.0f});  // t 23..60: 20 % loss, flat
    const auto changes = run_trace(c, trace);
    const Changes want = {
        {10000, 48},  // loss with jitter growth
        {12000, 32},
        {14000, 24},
        {18000, 32},  // 4 s after the last step the loss is still 20 %: back
                      // one rung and hold 30 s. The congestion at 21 and 22 s
                      // does not step down in the hold.
        {48000, 48},  // the hold ends; 10 s without congestion since 22 s
        {58000, 64},
    };
    EXPECT_EQ(changes, want);
    // The hold set at 18 s ends at 48 s.
    EXPECT_TRUE(c.holding(47000));
    EXPECT_FALSE(c.holding(48000));
}

TEST(SendAdaptation, ReportArrivalJitterDoesNotWidenTheWindow) {
    // The SFU's ticker is 1 s, but the reports arrive a few ms early or
    // late. The 2 s window holds the last two reports, not three.
    SendAdaptation c;
    for (int k = 1; k <= 11; ++k) {
        feed(c, 1000 * k + 30, {50, 0, 40.0f});
    }
    feed(c, 12029, {48, 7, 333.0f});
    feed(c, 13028, {49, 5, 351.0f});  // 12 of 97 over the last two: 12.4 %
    EXPECT_EQ(c.bitrate_kbps(), 48);
}

TEST(SendAdaptation, OneLossySecondStepsDownOnce) {
    SendAdaptation c;
    std::vector<Step> trace;
    repeat(trace, 5, {50, 0, 40.0f});
    trace.push_back({50, 25, 300.0f});  // t 6: 50 % in one second
    trace.push_back({50, 0, 300.0f});   // t 7: the round trip stays up
    repeat(trace, 6, {50, 0, 40.0f});   // t 8..13: clean
    const auto changes = run_trace(c, trace);
    EXPECT_EQ(changes, (Changes{{7000, 48}}));
}

TEST(SendAdaptation, ClimbsBackAfterTenSecondsWithoutCongestion) {
    SendAdaptation c;
    c.set_max_kbps(96);
    std::vector<Step> trace;
    repeat(trace, 5, {50, 0, 30.0f});
    repeat(trace, 2, {50, 15, 300.0f});  // t 6..7: congested, down to 64
    repeat(trace, 60, {50, 4, 30.0f});   // a minute at 8 % random loss
    const auto changes = run_trace(c, trace);
    // Fast down, 10 s up: no flapping, and the random loss does not hold
    // the rung down.
    EXPECT_EQ(changes, (Changes{{7000, 64}, {17000, 96}}));
}

TEST(SendAdaptation, TooFewPacketsGiveNoDecision) {
    SendAdaptation c;
    std::vector<Step> trace;
    repeat(trace, 5, {10, 0, 40.0f});
    // 20 % loss and a 10 x round trip, but 10 packets a second: the 2 s
    // window holds 20 packets, under 25. No step.
    repeat(trace, 10, {10, 2, 400.0f});
    EXPECT_TRUE(run_trace(c, trace).empty());
    EXPECT_EQ(c.window_loss(), -1.0);
}

TEST(SendAdaptation, BottomRungStaysAndLanRoundTripsDoNotCount) {
    SendAdaptation c;
    c.set_max_kbps(24);
    std::vector<Step> trace;
    repeat(trace, 5, {50, 25, 40.0f});
    repeat(trace, 5, {50, 25, 300.0f});
    EXPECT_TRUE(run_trace(c, trace).empty());
    EXPECT_EQ(c.bitrate_kbps(), 24);

    // 2 ms to 5 ms is "double" but stays under 2 x 50 ms; 2 ms jitter to
    // 6 ms stays under 2 x 10 ms.
    SendAdaptation lan;
    std::vector<Step> lan_trace;
    repeat(lan_trace, 5, {50, 10, 2.0f, 2.0f});
    repeat(lan_trace, 5, {50, 10, 5.0f, 6.0f});
    EXPECT_TRUE(run_trace(lan, lan_trace).empty());
}

TEST(SendAdaptation, LossPercFollowsFourSecondsOfLossBetweenFloorAndCap) {
    SendAdaptation c;
    EXPECT_EQ(c.loss_perc(), 5);
    int64_t t = 1000;
    auto report = [&](uint32_t expected, uint32_t lost) {
        feed(c, t, {expected, lost, 40.0f});
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
    repeat(trace, 5, {50, 0, 40.0f});
    repeat(trace, 3, {50, 20, 300.0f});  // down to 48 at t 7
    run_trace(c, trace);
    EXPECT_EQ(c.bitrate_kbps(), 48);
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
    std::vector<Step> d_trace;
    repeat(d_trace, 5, {50, 0, 40.0f});
    repeat(d_trace, 2, {50, 20, 300.0f});
    run_trace(d, d_trace);
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

    // Eight clean reports with a 60 ms round trip, then six of 40 % loss
    // with a 400 ms round trip (a full queue), one second apart on the
    // playout clock: a step down at the 2nd, 4th and 6th, to 24 kbps; loss
    // perc at the cap.
    MelloSendFeedback fb{};
    fb.interval_expected = 50;
    fb.jitter_ms = 5.0f;
    fb.rtt_fresh = true;
    for (int i = 0; i < 14; ++i) {
        const bool congested = i >= 8;
        fb.interval_lost = congested ? 20 : 0;
        fb.fraction_lost = congested ? 0.4f : 0.0f;
        fb.rtt_ms = congested ? 400.0f : 60.0f;
        mello_test_set_clock_ms(5000000 + 1000 * (i + 1));
        fb.reports = static_cast<uint32_t>(i + 1);
        pipeline.report_send_feedback(fb);
    }
    mello_test_set_clock_ms(-1);
    stats = pipeline.send_stats();
    EXPECT_EQ(stats.bitrate_kbps, 24);
    EXPECT_EQ(stats.loss_perc, 30);
    EXPECT_EQ(stats.reports, 14u);
    EXPECT_EQ(stats.rung_changes, 3u);
    EXPECT_NEAR(stats.feedback.rtt_ms, 400.0f, 1e-6f);

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
