#include <gtest/gtest.h>
#include "audio/noise_suppressor.hpp"
#include <cmath>
#include <vector>
#include <cstring>

using namespace mello::audio;

class NoiseSuppressorTest : public ::testing::Test {
protected:
    NoiseSuppressor ns;

    void SetUp() override {
        ASSERT_TRUE(ns.initialize());
    }

    void fill_silence(int16_t* buf, int n) {
        std::memset(buf, 0, n * sizeof(int16_t));
    }

    void fill_noise(int16_t* buf, int n, int16_t amplitude = 3000) {
        for (int i = 0; i < n; ++i) {
            buf[i] = static_cast<int16_t>((i * 7919 + 104729) % (2 * amplitude) - amplitude);
        }
    }
};

TEST_F(NoiseSuppressorTest, InitShutdown) {
    ns.shutdown();
    ASSERT_TRUE(ns.initialize()) << "re-init after shutdown should succeed";
}

TEST_F(NoiseSuppressorTest, SilencePassthrough) {
    int16_t buf[RNNOISE_FRAME_SIZE * 2];
    fill_silence(buf, RNNOISE_FRAME_SIZE * 2);

    ns.process(buf, RNNOISE_FRAME_SIZE * 2);

    double energy = 0;
    for (int i = 0; i < RNNOISE_FRAME_SIZE * 2; ++i)
        energy += static_cast<double>(buf[i]) * buf[i];
    energy = std::sqrt(energy / (RNNOISE_FRAME_SIZE * 2));

    EXPECT_LT(energy, 100.0) << "silence output should remain near-zero";
    EXPECT_LT(ns.speech_probability(), 0.3f) << "silence should have low speech probability";
}

TEST_F(NoiseSuppressorTest, SpeechProbabilityRange) {
    int16_t buf[RNNOISE_FRAME_SIZE];
    fill_noise(buf, RNNOISE_FRAME_SIZE);

    for (int i = 0; i < 10; ++i) {
        ns.process(buf, RNNOISE_FRAME_SIZE);
        float prob = ns.speech_probability();
        EXPECT_GE(prob, 0.0f) << "prob below 0 at frame " << i;
        EXPECT_LE(prob, 1.0f) << "prob above 1 at frame " << i;
    }
}

TEST_F(NoiseSuppressorTest, DisabledPassthrough) {
    int16_t original[RNNOISE_FRAME_SIZE];
    int16_t buf[RNNOISE_FRAME_SIZE];
    fill_noise(original, RNNOISE_FRAME_SIZE, 5000);
    std::memcpy(buf, original, sizeof(buf));

    ns.set_enabled(false);
    ns.process(buf, RNNOISE_FRAME_SIZE);

    EXPECT_EQ(std::memcmp(buf, original, sizeof(buf)), 0)
        << "disabled suppressor should not modify audio";
}
