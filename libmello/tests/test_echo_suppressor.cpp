// Neural echo suppressor tests. The model file ships via CMake download;
// locate it relative to this source file so dev and CI layouts both work.
// Skips (never fails) when the model is absent.
#include <gtest/gtest.h>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>
#include "audio/echo_suppressor.hpp"

using namespace mello::audio;

namespace {

std::string model_path() {
    std::string self(__FILE__);
    auto pos = self.find_last_of("/\\");
    std::string dir = (pos == std::string::npos) ? "." : self.substr(0, pos);
    return dir + "/../models/echo_suppressor.onnx";
}

bool have_model() {
    FILE* f = std::fopen(model_path().c_str(), "rb");
    if (f) std::fclose(f);
    return f != nullptr;
}

static constexpr int FRAME48 = 960;

void fill_noise48(int16_t* buf, int n, uint32_t& seed, int16_t amp = 8000) {
    for (int i = 0; i < n; ++i) {
        seed = seed * 1664525u + 1013904223u;
        buf[i] = static_cast<int16_t>(
            static_cast<int32_t>(seed % (2u * static_cast<uint32_t>(amp))) - amp);
    }
}

void fill_tone48(int16_t* buf, int n, float freq = 220.0f, int16_t amp = 6000) {
    for (int i = 0; i < n; ++i) {
        const float t = static_cast<float>(i) / 48000.0f;
        buf[i] = static_cast<int16_t>(
            amp * std::sin(2.0f * 3.14159265f * freq * t) *
            (0.5f + 0.5f * std::sin(2.0f * 3.14159265f * 3.0f * t)));
    }
}

// Speech-like double-talk signal: pitch contour + harmonics + syllabic
// gating. A steady pure tone is pathological for mask models (they read
// it as stationary interference); this tracks real-speech behavior.
void fill_speechlike48(int16_t* buf, int n, int frame_index) {
    for (int i = 0; i < n; ++i) {
        const float t = static_cast<float>(frame_index * 960 + i) / 48000.0f;
        const float ph = 2.0f * 3.14159265f * 130.0f * t +
                         (30.0f / 0.7f) *
                             (1.0f - std::cos(2.0f * 3.14159265f * 0.7f * t));
        const float gate =
            (0.5f + 0.5f * std::sin(2.0f * 3.14159265f * 2.2f * t)) > 0.35f ? 1.0f : 0.0f;
        const float s = 0.20f *
                        (std::sin(ph) + 0.4f * std::sin(2.0f * ph) +
                         0.2f * std::sin(3.0f * ph)) *
                        gate;
        int32_t v = static_cast<int32_t>(s * 32767.0f);
        if (v > 32767) v = 32767;
        if (v < -32768) v = -32768;
        buf[i] = static_cast<int16_t>(v);
    }
}

double rms48(const int16_t* buf, int n) {
    double sum = 0.0;
    for (int i = 0; i < n; ++i) {
        const double s = buf[i] / 32768.0;
        sum += s * s;
    }
    return std::sqrt(sum / n);
}

class EchoSuppressorTest : public ::testing::Test {
protected:
    EchoSuppressor sup;

    void SetUp() override {
        if (!have_model()) {
            GTEST_SKIP() << "echo_suppressor.onnx absent (CMake downloads it)";
        }
        ASSERT_TRUE(sup.initialize(model_path()));
        sup.set_enabled(true);
    }

    void TearDown() override { sup.shutdown(); }
};

}  // namespace

TEST_F(EchoSuppressorTest, DisabledIsBitExactPassthrough) {
    sup.set_enabled(false);
    int16_t frame[FRAME48], copy[FRAME48];
    uint32_t seed = 1;
    fill_noise48(frame, FRAME48, seed);
    std::memcpy(copy, frame, sizeof(frame));
    sup.process(frame);
    EXPECT_EQ(std::memcmp(frame, copy, sizeof(frame)), 0);
}

TEST_F(EchoSuppressorTest, SilentFarEndBypasses) {
    // No feed_far_end calls: reference ring stays empty, frame untouched.
    int16_t frame[FRAME48], copy[FRAME48];
    fill_tone48(frame, FRAME48);
    std::memcpy(copy, frame, sizeof(frame));
    for (int i = 0; i < 40; ++i) sup.process(frame);
    EXPECT_EQ(std::memcmp(frame, copy, sizeof(frame)), 0);
}

TEST_F(EchoSuppressorTest, BroadbandEchoSuppressed) {
    // Far-end noise; mic = 20 ms-delayed attenuated copy. Feed the
    // reference in 512-sample chunks like a real playback callback.
    uint32_t seed = 0x1234u;
    int16_t far[FRAME48], mic[FRAME48], prev[FRAME48] = {};
    double pre_sq = 0.0, post_sq = 0.0;
    constexpr int kWarm = 60, kMeasure = 120;
    for (int f = 0; f < kWarm + kMeasure; ++f) {
        fill_noise48(far, FRAME48, seed);
        for (int i = 0; i < FRAME48; ++i) {
            const int32_t e = static_cast<int32_t>(prev[i]) / 2;
            mic[i] = static_cast<int16_t>(e);
        }
        for (int off = 0; off < FRAME48; off += 512) {
            const int n = std::min(512, FRAME48 - off);
            sup.feed_far_end(far + off, n);
        }
        double pre = rms48(mic, FRAME48);
        sup.process(mic);
        if (f >= kWarm) {
            pre_sq += pre * pre;
            const double post = rms48(mic, FRAME48);
            post_sq += post * post;
        }
        std::memcpy(prev, far, sizeof(far));
    }
    const double erle = 20.0 * std::log10(std::sqrt(pre_sq / kMeasure) /
                                          (std::sqrt(post_sq / kMeasure) + 1e-12));
    printf("[ERLE] neural suppressor broadband: %.2f dB\n", erle);
    // AEC3 alone reaches ~24 dB here; the stage must clearly beat it.
    EXPECT_GT(erle, 25.0);
}

TEST_F(EchoSuppressorTest, DoubleTalkKeepsNearEnd) {
    // Speech-like near-end + echo: output must retain most near-end energy.
    // (A steady pure tone is pathological for mask models; speech-like
    // pitch/gating tracks real behavior. Python reference: -4.6 dB.)
    uint32_t seed = 0x999u;
    int16_t far[FRAME48], mic[FRAME48], near[FRAME48], prev[FRAME48] = {};
    double near_sq = 0.0, post_sq = 0.0;
    constexpr int kWarm = 60, kMeasure = 120;
    for (int f = 0; f < kWarm + kMeasure; ++f) {
        fill_noise48(far, FRAME48, seed);
        fill_speechlike48(near, FRAME48, f);
        for (int i = 0; i < FRAME48; ++i) {
            mic[i] = static_cast<int16_t>(near[i] + prev[i] / 2);
        }
        sup.feed_far_end(far, FRAME48);
        sup.process(mic);
        if (f >= kWarm) {
            near_sq += rms48(near, FRAME48) * rms48(near, FRAME48);
            const double post = rms48(mic, FRAME48);
            post_sq += post * post;
        }
        std::memcpy(prev, far, sizeof(far));
    }
    const double keep = 20.0 * std::log10(std::sqrt(post_sq / kMeasure) /
                                          (std::sqrt(near_sq / kMeasure) + 1e-12));
    printf("[DT] neural suppressor near-end keep: %.2f dB\n", keep);
    // Guards catastrophic suppression (an early revision managed -43 dB on
    // a steady tone). Python parity on this signal is -4.6 dB; the gap to
    // the resampled C++ chain is an open investigation for the default-on
    // decision (field MOS decides), not a rollout blocker while off.
    EXPECT_GT(keep, -14.0);
}
