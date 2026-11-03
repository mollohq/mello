#include <gtest/gtest.h>
#include "replay/replay_audio_mix.hpp"
#include <cmath>
#include <cstdint>
#include <cstdlib>
#include <random>
#include <vector>

using namespace mello::replay;

namespace {

constexpr double kRate = 48000.0;
constexpr uint64_t kChunkFrames = 480;

// A source clock that runs `ppm` fast (positive) or slow (negative): in one
// true second it makes 48000 * (1 + ppm / 1e6) frames. Each chunk is stamped
// with the true time of its first frame, plus a jitter, as the steady clock
// sees it.
struct SourceClock {
    double ppm = 0;
    uint32_t channels = 1;
    double jitter_us = 0;
};

// Chunks from `t_start_us` to `t_end_us` of silence with a click (a single
// sample at 0.5 of full scale) at each true time in `clicks_us`.
std::vector<TimedPcm> make_chunks(const SourceClock& clock, uint64_t t_start_us, uint64_t t_end_us,
                                  const std::vector<uint64_t>& clicks_us, uint32_t seed) {
    std::mt19937 rng(seed);
    std::uniform_real_distribution<double> jitter(-clock.jitter_us, clock.jitter_us);
    const double rate = kRate * (1.0 + clock.ppm / 1e6);
    const uint64_t total = static_cast<uint64_t>(
        static_cast<double>(t_end_us - t_start_us) / 1e6 * rate);
    std::vector<int16_t> all(total * clock.channels, 0);
    for (const uint64_t click : clicks_us) {
        const auto frame = static_cast<uint64_t>(
            std::llround(static_cast<double>(click - t_start_us) / 1e6 * rate));
        for (uint32_t c = 0; c < clock.channels; ++c) all[frame * clock.channels + c] = 16384;
    }
    std::vector<TimedPcm> chunks;
    for (uint64_t f = 0; f + kChunkFrames <= total; f += kChunkFrames) {
        TimedPcm chunk;
        const double true_us = static_cast<double>(t_start_us) + static_cast<double>(f) / rate * 1e6;
        chunk.capture_us = static_cast<uint64_t>(std::llround(true_us + jitter(rng)));
        chunk.samples.assign(all.begin() + static_cast<std::ptrdiff_t>(f * clock.channels),
                             all.begin() + static_cast<std::ptrdiff_t>((f + kChunkFrames) * clock.channels));
        chunks.push_back(std::move(chunk));
    }
    return chunks;
}

// The frames of the left channel above `threshold`, one per click: the
// largest frame of each group of frames above it.
std::vector<size_t> find_clicks(const std::vector<int16_t>& stereo, int threshold) {
    std::vector<size_t> clicks;
    const size_t frames = stereo.size() / 2;
    size_t i = 0;
    while (i < frames) {
        if (std::abs(stereo[2 * i]) < threshold) {
            ++i;
            continue;
        }
        size_t best = i;
        while (i < frames && std::abs(stereo[2 * i]) >= threshold) {
            if (std::abs(stereo[2 * i]) > std::abs(stereo[2 * best])) best = i;
            ++i;
        }
        clicks.push_back(best);
    }
    return clicks;
}

size_t frame_of(uint64_t t_us, uint64_t t0_us) {
    return static_cast<size_t>(std::llround(static_cast<double>(t_us - t0_us) / 1e6 * kRate));
}

} // namespace

TEST(ReplayAvClock, FitRunsFindsTheDriftOfAFastClock) {
    SourceClock clock{300, 1, 3000};
    const auto chunks = make_chunks(clock, 10'000'000, 40'000'000, {}, 1);
    const auto runs = detail::fit_runs(chunks, 1);
    ASSERT_EQ(runs.size(), 1u);
    // With stamps that jitter by +-3 ms, the fit is good to a few ppm. 20 ppm
    // is 0.6 ms over a 30 s clip, inside the 1 ms of the marker test below.
    const double expected = 1e6 / (kRate * (1.0 + 300e-6));
    EXPECT_NEAR(runs[0].us_per_frame / expected - 1.0, 0.0, 20e-6) << "within 20 ppm";
    EXPECT_NEAR(runs[0].start_us, 10'000'000.0, 500.0) << "the jitter averages out";
}

// CLIPS.md 15.13: three sources on their own clocks, one 300 ppm fast,
// one 200 ppm slow, with jittered stamps. After the mix each marker is within
// 1 ms of its true time.
TEST(ReplayAvClock, ThreeSourcesLandOnTheirTimesWithDriftAndJitter) {
    const uint64_t t0 = 10'000'000;
    const uint64_t end = t0 + 30'000'000;
    ClipAudioSource game;
    game.channels = 2;
    game.chunks = make_chunks({0, 2, 4000}, t0 - 500'000, end, {t0 + 5'000'000, t0 + 25'000'000}, 1);
    ClipAudioSource voice;
    voice.channels = 1;
    voice.chunks = make_chunks({300, 1, 3000}, t0 - 500'000, end, {t0 + 10'000'000, t0 + 28'000'000}, 2);
    ClipAudioSource mic;
    mic.channels = 1;
    mic.chunks = make_chunks({-200, 1, 2000}, t0 - 500'000, end, {t0 + 15'000'000, t0 + 29'500'000}, 3);

    const auto out = mix_clip_audio(t0, end, {game, voice, mic});
    ASSERT_EQ(out.size(), static_cast<size_t>(30 * kRate) * 2);
    const auto clicks = find_clicks(out, 4000);
    const std::vector<uint64_t> expected = {t0 + 5'000'000,  t0 + 10'000'000, t0 + 15'000'000,
                                            t0 + 25'000'000, t0 + 28'000'000, t0 + 29'500'000};
    ASSERT_EQ(clicks.size(), expected.size());
    for (size_t i = 0; i < expected.size(); ++i) {
        const double off_ms = (static_cast<double>(clicks[i]) -
                               static_cast<double>(frame_of(expected[i], t0))) / kRate * 1000.0;
        EXPECT_LE(std::abs(off_ms), 1.0) << "marker " << i << " is " << off_ms << " ms off";
    }
}

// CLIPS.md 15.13 (ReplayGaps): a gap is silence at its place, and the
// audio after it keeps its own time.
TEST(ReplayGaps, AGapIsSilenceAndTheAudioAfterItKeepsItsTime) {
    const uint64_t t0 = 10'000'000;
    ClipAudioSource game;
    game.channels = 2;
    auto before = make_chunks({0, 2, 1000}, t0, t0 + 1'000'000, {t0 + 500'000}, 4);
    auto after = make_chunks({0, 2, 1000}, t0 + 1'300'000, t0 + 2'000'000, {t0 + 1'700'000}, 5);
    // A constant level, so the gap shows as zeros between two non-zero parts.
    for (auto* part : {&before, &after}) {
        for (auto& c : *part) {
            for (auto& s : c.samples) s = static_cast<int16_t>(s == 0 ? 1000 : s);
        }
    }
    game.chunks = before;
    game.chunks.insert(game.chunks.end(), after.begin(), after.end());

    const auto out = mix_clip_audio(t0, t0 + 2'000'000, {game});
    const auto clicks = find_clicks(out, 4000);
    ASSERT_EQ(clicks.size(), 2u);
    EXPECT_LE(std::abs(static_cast<long>(clicks[1]) - static_cast<long>(frame_of(t0 + 1'700'000, t0))), 48)
        << "the click after the gap is at its own time";
    for (size_t f = frame_of(t0 + 1'010'000, t0); f < frame_of(t0 + 1'290'000, t0); ++f) {
        ASSERT_EQ(out[2 * f], 0) << "frame " << f << " in the gap";
    }
    EXPECT_NE(out[2 * frame_of(t0 + 900'000, t0)], 0);
    EXPECT_NE(out[2 * frame_of(t0 + 1'400'000, t0)], 0);
}

TEST(ReplayMix, TheLimiterKeepsTheSumUnderTheCeiling) {
    const uint64_t t0 = 0;
    // Two full-scale 1 kHz tones in phase from 0.5 s: twice full scale.
    // Before 0.5 s one quiet tone.
    std::vector<ClipAudioSource> sources(2);
    for (int s = 0; s < 2; ++s) {
        sources[s].channels = 1;
        for (uint64_t f = 0; f < 48000; f += kChunkFrames) {
            TimedPcm c;
            c.capture_us = f * 1'000'000 / 48000;
            c.samples.resize(kChunkFrames);
            for (uint64_t i = 0; i < kChunkFrames; ++i) {
                const double v = std::sin(2.0 * 3.14159265358979 * 1000.0 * static_cast<double>(f + i) / kRate);
                const double level = (f + i) < 24000 ? (s == 0 ? 0.1 : 0.0) : 1.0;
                c.samples[i] = static_cast<int16_t>(std::lrint(v * level * 32767.0));
            }
            sources[s].chunks.push_back(std::move(c));
        }
    }
    const auto out = mix_clip_audio(t0, 1'000'000, sources);
    const int ceiling = static_cast<int>(32768.0 * std::pow(10.0, -1.0 / 20.0)) + 1;
    int peak = 0;
    for (size_t i = 24000 * 2; i < out.size(); ++i) peak = std::max(peak, std::abs(static_cast<int>(out[i])));
    EXPECT_LE(peak, ceiling);
    EXPECT_GT(peak, ceiling * 9 / 10) << "the limiter limits, it does not mute";
    // The quiet part well before the loud one is untouched: 0.1 of full scale.
    int quiet_peak = 0;
    for (size_t i = 2 * 2400; i < 2 * 20000; ++i) quiet_peak = std::max(quiet_peak, std::abs(static_cast<int>(out[i])));
    EXPECT_NEAR(quiet_peak, 3277, 40);
}

TEST(ReplayMix, GainPointsFollowTime) {
    const uint64_t t0 = 1'000'000;
    ClipAudioSource voice;
    voice.channels = 1;
    for (uint64_t f = 0; f < 96000; f += kChunkFrames) {
        TimedPcm c;
        c.capture_us = t0 + f * 1'000'000 / 48000;
        c.samples.assign(kChunkFrames, 8192);  // 0.25 of full scale
        voice.chunks.push_back(std::move(c));
    }
    // m3llo's mixer slider: 100 %, then 50 % after 1 s.
    voice.gain_points = {{t0, 1.0f}, {t0 + 1'000'000, 0.5f}};
    const auto out = mix_clip_audio(t0, t0 + 2'000'000, {voice});
    EXPECT_NEAR(out[2 * 24000], 8192, 2);
    EXPECT_NEAR(out[2 * 24000 + 1], 8192, 2) << "a mono source is on both channels";
    EXPECT_NEAR(out[2 * 72000], 4096, 2);
}
