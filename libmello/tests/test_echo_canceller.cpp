#include <gtest/gtest.h>
#include "audio/echo_canceller.hpp"
#include <cmath>
#include <cstring>
#include <vector>

using namespace mello::audio;

static constexpr int SAMPLE_RATE = 48000;
static constexpr int CHANNELS = 1;
static constexpr int FRAME_SIZE = 960; // 20ms at 48kHz

class EchoCancellerTest : public ::testing::Test {
protected:
    EchoCanceller ec;

    void SetUp() override {
        ASSERT_TRUE(ec.initialize(SAMPLE_RATE, CHANNELS));
    }

    void fill_silence(int16_t* buf, int n) {
        std::memset(buf, 0, n * sizeof(int16_t));
    }

    void fill_tone(int16_t* buf, int n, float freq_hz = 440.0f, int16_t amplitude = 8000) {
        for (int i = 0; i < n; ++i) {
            float t = static_cast<float>(i) / SAMPLE_RATE;
            buf[i] = static_cast<int16_t>(amplitude * std::sin(2.0f * 3.14159265f * freq_hz * t));
        }
    }
};

TEST_F(EchoCancellerTest, InitShutdown) {
    ec.shutdown();
    ASSERT_TRUE(ec.initialize(SAMPLE_RATE, CHANNELS))
        << "re-init after shutdown should succeed";
}

TEST_F(EchoCancellerTest, DefaultsEnabled) {
    EXPECT_TRUE(ec.aec_enabled());
    EXPECT_TRUE(ec.agc_enabled());
}

TEST_F(EchoCancellerTest, ToggleAec) {
    ec.set_aec_enabled(false);
    EXPECT_FALSE(ec.aec_enabled());
    ec.set_aec_enabled(true);
    EXPECT_TRUE(ec.aec_enabled());
}

TEST_F(EchoCancellerTest, ToggleAgc) {
    ec.set_agc_enabled(false);
    EXPECT_FALSE(ec.agc_enabled());
    ec.set_agc_enabled(true);
    EXPECT_TRUE(ec.agc_enabled());
}

TEST_F(EchoCancellerTest, SilencePassthrough) {
    int16_t buf[FRAME_SIZE];
    fill_silence(buf, FRAME_SIZE);

    ec.process_capture(buf, FRAME_SIZE);

    double energy = 0;
    for (int i = 0; i < FRAME_SIZE; ++i)
        energy += static_cast<double>(buf[i]) * buf[i];
    energy = std::sqrt(energy / FRAME_SIZE);

    EXPECT_LT(energy, 100.0) << "silence should remain near-zero after processing";
}

TEST_F(EchoCancellerTest, ProcessRenderDoesNotCrash) {
    int16_t render[FRAME_SIZE];
    fill_tone(render, FRAME_SIZE);

    // Should not crash or error on valid data
    ec.process_render(render, FRAME_SIZE);
}

TEST_F(EchoCancellerTest, ProcessCaptureAfterRender) {
    int16_t render[FRAME_SIZE];
    int16_t capture[FRAME_SIZE];
    fill_tone(render, FRAME_SIZE, 440.0f, 8000);
    fill_tone(capture, FRAME_SIZE, 1000.0f, 4000);

    ec.process_render(render, FRAME_SIZE);
    ec.process_capture(capture, FRAME_SIZE);

    // Just verify it doesn't crash; AEC convergence needs many frames
}

TEST_F(EchoCancellerTest, DisabledPassthrough) {
    int16_t original[FRAME_SIZE];
    int16_t buf[FRAME_SIZE];
    fill_tone(original, FRAME_SIZE);
    std::memcpy(buf, original, sizeof(buf));

    ec.set_aec_enabled(false);
    ec.set_agc_enabled(false);
    ec.process_capture(buf, FRAME_SIZE);

    EXPECT_EQ(std::memcmp(buf, original, sizeof(buf)), 0)
        << "fully disabled should not modify audio";
}

TEST_F(EchoCancellerTest, MultipleFrames) {
    int16_t render[FRAME_SIZE];
    int16_t capture[FRAME_SIZE];
    fill_tone(render, FRAME_SIZE, 440.0f, 8000);

    for (int i = 0; i < 50; ++i) {
        fill_tone(capture, FRAME_SIZE, 1000.0f, 4000);
        ec.process_render(render, FRAME_SIZE);
        ec.process_capture(capture, FRAME_SIZE);
    }
}

// --- ERLE regression harness ---
// Broadband synthetic loopback: far-end white noise, mic = 20 ms-delayed
// attenuated copy. AGC2 off for isolation. Measures steady-state ERLE.
// Tonal signals are NOT used: AEC3 collapses to ~1 dB on tones.

static uint32_t prng_next(uint32_t& s) {
    s = s * 1664525u + 1013904223u;
    return s;
}

static void fill_broadband(int16_t* buf, int n, uint32_t& seed, int16_t amplitude = 8000) {
    for (int i = 0; i < n; ++i) {
        // White noise in [-amplitude, amplitude], deterministic.
        uint32_t r = prng_next(seed);
        int32_t v = static_cast<int32_t>(r % (2u * static_cast<uint32_t>(amplitude))) - amplitude;
        buf[i] = static_cast<int16_t>(v);
    }
}

static double rms_of(const int16_t* buf, int n) {
    double sum = 0.0;
    for (int i = 0; i < n; ++i) {
        double s = buf[i] / 32768.0;
        sum += s * s;
    }
    return std::sqrt(sum / n);
}

TEST_F(EchoCancellerTest, BroadbandLoopbackCancelsEcho) {
    ec.set_agc_enabled(false);
    ec.set_aec_enabled(true);

    constexpr int kWarmupFrames = 150;   // ~3 s convergence
    constexpr int kMeasureFrames = 150;  // ~3 s steady state
    constexpr int kTotal = kWarmupFrames + kMeasureFrames;
    constexpr float kEchoAtten = 0.5f;  // -6 dB echo path

    uint32_t seed = 0x12345678u;
    std::vector<int16_t> far(FRAME_SIZE);
    std::vector<int16_t> prev_far(FRAME_SIZE, 0);
    std::vector<int16_t> mic(FRAME_SIZE);

    double pre_sum_sq = 0.0;
    double post_sum_sq = 0.0;
    int64_t measure_samples = 0;

    for (int f = 0; f < kTotal; ++f) {
        fill_broadband(far.data(), FRAME_SIZE, seed);
        // 20 ms-delayed attenuated echo: mic = previous far-end frame * atten.
        for (int i = 0; i < FRAME_SIZE; ++i) {
            int32_t e = static_cast<int32_t>(prev_far[i] * kEchoAtten);
            mic[i] = static_cast<int16_t>(e);
        }
        ec.process_render(far.data(), FRAME_SIZE);

        if (f >= kWarmupFrames) {
            double pre_rms = rms_of(mic.data(), FRAME_SIZE);
            pre_sum_sq += pre_rms * pre_rms * FRAME_SIZE;
        }
        ec.process_capture(mic.data(), FRAME_SIZE);
        if (f >= kWarmupFrames) {
            double post_rms = rms_of(mic.data(), FRAME_SIZE);
            post_sum_sq += post_rms * post_rms * FRAME_SIZE;
            measure_samples += FRAME_SIZE;
        }
        prev_far = far;
    }

    double pre_rms = std::sqrt(pre_sum_sq / measure_samples);
    double post_rms = std::sqrt(post_sum_sq / measure_samples);
    double erle_db = 20.0 * std::log10(pre_rms / (post_rms + 1e-12));
    printf("[ERLE] broadband loopback (frame-aligned 20 ms): pre=%.6f post=%.6f ERLE=%.2f dB\n",
           pre_rms, post_rms, erle_db);

    // Baseline on vendored v1.3 (M88), ideal 1-frame sync loopback, AGC2 off:
    // ~24.3 dB (macOS arm64, measured 2026-09-04). Field reports near ~13 dB
    // reflect realistic impairments (latency search, reverb, AGC pumping),
    // not this ideal harness. Threshold stays at 10 dB to catch render-feed
    // regressions; re-baseline toward 25 dB once the engine upgrade lands.
    // The MisalignedDelayLoopback test below covers the delay-estimator path
    // where engine generations actually differ.
    EXPECT_GT(erle_db, 10.0) << "pre_rms=" << pre_rms << " post_rms=" << post_rms;
}

TEST_F(EchoCancellerTest, MisalignedDelayLoopbackCancelsEcho) {
    // Same broadband loopback, but the echo path delay (24.4 ms = 1173
    // samples) is NOT a multiple of the 10 ms APM chunk: the delay
    // estimator must find it instead of starting converged. This is the
    // path where Bluetooth latency and missing delay hints hurt, and where
    // newer delay estimation earns its keep. A delay-line models the lag.
    ec.set_agc_enabled(false);
    ec.set_aec_enabled(true);

    constexpr int kDelaySamples = 1173;
    constexpr int kWarmupFrames = 250;   // longer convergence for search
    constexpr int kMeasureFrames = 150;
    constexpr int kTotal = kWarmupFrames + kMeasureFrames;
    constexpr float kEchoAtten = 0.5f;

    uint32_t seed = 0x9E3779B9u;
    std::vector<int16_t> far(FRAME_SIZE);
    std::vector<int16_t> mic(FRAME_SIZE);
    std::vector<int16_t> history(kDelaySamples + FRAME_SIZE, 0);

    double pre_sum_sq = 0.0;
    double post_sum_sq = 0.0;
    int64_t measure_samples = 0;

    for (int f = 0; f < kTotal; ++f) {
        fill_broadband(far.data(), FRAME_SIZE, seed);
        // Push first, then tap: history holds the last
        // (kDelaySamples + FRAME_SIZE) far-end samples, so history[i]
        // lags the current frame by exactly kDelaySamples.
        history.erase(history.begin(), history.begin() + FRAME_SIZE);
        history.insert(history.end(), far.begin(), far.end());
        for (int i = 0; i < FRAME_SIZE; ++i) {
            int32_t e = static_cast<int32_t>(
                history[i] * kEchoAtten);
            mic[i] = static_cast<int16_t>(e);
        }

        ec.process_render(far.data(), FRAME_SIZE);
        if (f >= kWarmupFrames) {
            double pre_rms = rms_of(mic.data(), FRAME_SIZE);
            pre_sum_sq += pre_rms * pre_rms * FRAME_SIZE;
        }
        ec.process_capture(mic.data(), FRAME_SIZE);
        if (f >= kWarmupFrames) {
            double post_rms = rms_of(mic.data(), FRAME_SIZE);
            post_sum_sq += post_rms * post_rms * FRAME_SIZE;
            measure_samples += FRAME_SIZE;
        }
    }

    double pre_rms = std::sqrt(pre_sum_sq / measure_samples);
    double post_rms = std::sqrt(post_sum_sq / measure_samples);
    double erle_db = 20.0 * std::log10(pre_rms / (post_rms + 1e-12));
    printf("[ERLE] broadband loopback (misaligned 24.4 ms): pre=%.6f post=%.6f ERLE=%.2f dB\n",
           pre_rms, post_rms, erle_db);

    // Deliberately weaker than the aligned case: guards the estimator path
    // without overfitting to one engine generation. Measured v2.1 (M131):
    // 22.7 dB vs 23.7 dB aligned. Investigate (don't just lower) if it
    // drops more than ~6 dB under the aligned result on the same build.
    EXPECT_GT(erle_db, 6.0) << "pre_rms=" << pre_rms << " post_rms=" << post_rms;
}

TEST_F(EchoCancellerTest, BlindRunStaysPassthrough) {
    // No process_render calls: AEC has no reference, so capture must pass
    // through (within +/-3 dB). Catches vacuous harness + render-feed bugs
    // from the other side: if THIS fails, AEC attenuates without reference.
    ec.set_agc_enabled(false);
    ec.set_aec_enabled(true);

    uint32_t seed = 0xABCDEF01u;
    std::vector<int16_t> mic(FRAME_SIZE);
    double pre_sum_sq = 0.0, post_sum_sq = 0.0;
    constexpr int kFrames = 100;
    for (int f = 0; f < kFrames; ++f) {
        fill_broadband(mic.data(), FRAME_SIZE, seed);
        double pre = rms_of(mic.data(), FRAME_SIZE);
        pre_sum_sq += pre * pre * FRAME_SIZE;
        ec.process_capture(mic.data(), FRAME_SIZE);
        double post = rms_of(mic.data(), FRAME_SIZE);
        post_sum_sq += post * post * FRAME_SIZE;
    }
    double pre_rms = std::sqrt(pre_sum_sq / (kFrames * FRAME_SIZE));
    double post_rms = std::sqrt(post_sum_sq / (kFrames * FRAME_SIZE));
    double ratio_db = 20.0 * std::log10((post_rms + 1e-12) / (pre_rms + 1e-12));
    EXPECT_GT(ratio_db, -3.0) << "AEC attenuated without reference";
    EXPECT_LT(ratio_db, 3.0) << "AEC amplified without reference";
}

TEST_F(EchoCancellerTest, StreamDelayHintClamped) {
    ec.set_stream_delay_ms(30);
    EXPECT_EQ(ec.stream_delay_ms(), 30);
    ec.set_stream_delay_ms(-5);
    EXPECT_EQ(ec.stream_delay_ms(), 0);
    ec.set_stream_delay_ms(9999);
    EXPECT_EQ(ec.stream_delay_ms(), 500);
}

TEST_F(EchoCancellerTest, RenderAccumulatesSubFrameChunks) {
    // 48 x 100-sample render callbacks = 4800 samples = exactly ten 10 ms
    // APM chunks. Old code dropped every sub-480 tail (0 frames processed);
    // the accumulator must produce 10.
    ec.set_aec_enabled(true);
    uint32_t before = ec.render_frames();
    int16_t chunk[100];
    uint32_t seed = 0x77777777u;
    fill_broadband(chunk, 100, seed);
    for (int i = 0; i < 48; ++i) {
        ec.process_render(chunk, 100);
    }
    EXPECT_EQ(ec.render_frames() - before, 10u);
}

