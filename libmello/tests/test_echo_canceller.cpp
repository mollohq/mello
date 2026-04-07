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
