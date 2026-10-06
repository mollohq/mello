// Voice capture path through a real AudioPipeline on the device-free backend
// (MELLO_AUDIO_BACKEND=test): capture inject, DSP, Opus encode, get_packet.

#include <gtest/gtest.h>

#include "audio/audio_pipeline.hpp"
#include "audio/opus_codec.hpp"

#include <cmath>
#include <cstdlib>
#include <vector>

#ifdef _WIN32
#include <windows.h>
#endif

namespace {

using namespace mello::audio;

void select_test_backend() {
#ifdef _WIN32
    SetEnvironmentVariableA("MELLO_AUDIO_BACKEND", "test");
#else
    setenv("MELLO_AUDIO_BACKEND", "test", 1);
#endif
}

struct SentPacket {
    uint32_t sequence;
    uint32_t timestamp;
};

class VoiceCaptureTest : public ::testing::Test {
protected:
    AudioPipeline pipeline;
    uint64_t frames_injected = 0;

    void SetUp() override {
        select_test_backend();
        ASSERT_TRUE(pipeline.initialize());
        // Push-to-talk: every unmuted frame is encoded, so the test controls
        // exactly which frames become packets.
        pipeline.set_push_to_talk(true);
        ASSERT_TRUE(pipeline.start_capture_inject());
    }

    void TearDown() override { pipeline.shutdown(); }

    // Inject `frames` 20 ms frames of a 300 Hz tone, in 10 ms chunks like a
    // device callback.
    void inject(int frames) {
        std::vector<int16_t> chunk(FRAME_SIZE / 2);
        for (int f = 0; f < frames * 2; ++f) {
            for (size_t i = 0; i < chunk.size(); ++i) {
                const double t =
                    static_cast<double>(frames_injected * FRAME_SIZE + f * chunk.size() + i) /
                    SAMPLE_RATE;
                chunk[i] = static_cast<int16_t>(6000.0 * std::sin(2.0 * 3.14159265358979 * 300.0 * t));
            }
            pipeline.inject_capture(chunk.data(), static_cast<int>(chunk.size()));
        }
        frames_injected += static_cast<uint64_t>(frames);
    }

    std::vector<SentPacket> drain() {
        std::vector<SentPacket> out;
        uint8_t buf[MAX_PACKET_SIZE + 4];
        for (;;) {
            uint32_t ts = 0;
            const int n = pipeline.get_packet(buf, sizeof(buf), &ts);
            if (n <= 0) break;
            const uint32_t seq = static_cast<uint32_t>(buf[0]) |
                                 (static_cast<uint32_t>(buf[1]) << 8) |
                                 (static_cast<uint32_t>(buf[2]) << 16) |
                                 (static_cast<uint32_t>(buf[3]) << 24);
            out.push_back({seq, ts});
        }
        return out;
    }
};

// Two consecutive packets 20 ms apart differ by 960 (48 kHz samples).
TEST_F(VoiceCaptureTest, ConsecutivePacketsAdvanceBy960) {
    inject(5);
    const auto sent = drain();
    ASSERT_EQ(sent.size(), 5u);
    for (size_t i = 1; i < sent.size(); ++i) {
        EXPECT_EQ(sent[i].sequence, sent[i - 1].sequence + 1);
        EXPECT_EQ(sent[i].timestamp - sent[i - 1].timestamp, 960u) << "packet " << i;
    }
}

// The timestamp counts every captured frame, encoded or not. A 1 s gap in
// which nothing is encoded (here: muted) advances it by 48000, like DTX,
// while the sequence advances by one.
TEST_F(VoiceCaptureTest, OneSecondGateGapAdvancesBy48000) {
    inject(2);
    pipeline.set_mute(true);
    inject(50);  // 1 s, not encoded
    pipeline.set_mute(false);
    inject(1);

    const auto sent = drain();
    ASSERT_EQ(sent.size(), 3u);
    EXPECT_EQ(sent[1].timestamp - sent[0].timestamp, 960u);
    EXPECT_EQ(sent[2].sequence, sent[1].sequence + 1);
    EXPECT_EQ(sent[2].timestamp - sent[1].timestamp, 48000u + 960u);
}

}  // namespace

namespace {

// Input sensitivity (spec 10 section 8: every control reaches libmello).
// The RMS gate decides which frames are speech candidates for Silero.
class InputSensitivityTest : public VoiceCaptureTest {
protected:
    void SetUp() override {
        VoiceCaptureTest::SetUp();
        pipeline.set_push_to_talk(false);  // the gate only runs in VAD mode
    }

    // Inject `frames` 20 ms frames of a tone at `dbfs` RMS.
    void inject_level(int frames, float dbfs) {
        const double amplitude = 32768.0 * std::pow(10.0, dbfs / 20.0) * std::sqrt(2.0);
        std::vector<int16_t> chunk(FRAME_SIZE / 2);
        for (int f = 0; f < frames * 2; ++f) {
            for (size_t i = 0; i < chunk.size(); ++i) {
                const double t = static_cast<double>(f * chunk.size() + i) / SAMPLE_RATE;
                chunk[i] = static_cast<int16_t>(amplitude * std::sin(2.0 * 3.14159265358979 * 400.0 * t));
            }
            pipeline.inject_capture(chunk.data(), static_cast<int>(chunk.size()));
        }
    }
};

// Manual: the dB value is the gate threshold. A -40 dBFS voice stays below a
// -30 dBFS gate and passes a -50 dBFS gate.
TEST_F(InputSensitivityTest, ManualLevelIsTheGateThreshold) {
    pipeline.set_input_sensitivity_auto(false);
    pipeline.set_input_sensitivity(-30.0f);
    inject_level(5, -40.0f);
    EXPECT_NEAR(pipeline.input_gate_dbfs(), -30.0f, 0.01f);
    EXPECT_EQ(pipeline.gate_candidate_frames(), 0u);

    pipeline.set_input_sensitivity(-50.0f);
    inject_level(5, -40.0f);
    EXPECT_NEAR(pipeline.input_gate_dbfs(), -50.0f, 0.01f);
    EXPECT_EQ(pipeline.gate_candidate_frames(), 5u);
}

// Auto (the default) keeps the floor-tracking gate: at least -54 dBFS, here
// 2.5 x the initial -60 dBFS floor. Switching back from manual restores it.
TEST_F(InputSensitivityTest, AutoTracksTheNoiseFloor) {
    EXPECT_TRUE(pipeline.input_sensitivity_auto());
    inject_level(1, -40.0f);
    EXPECT_NEAR(pipeline.input_gate_dbfs(), 20.0f * std::log10(0.0025f), 0.1f);
    EXPECT_EQ(pipeline.gate_candidate_frames(), 1u);

    pipeline.set_input_sensitivity_auto(false);
    pipeline.set_input_sensitivity(0.0f);
    inject_level(1, -40.0f);
    EXPECT_EQ(pipeline.gate_candidate_frames(), 1u) << "a 0 dBFS gate admits nothing";

    pipeline.set_input_sensitivity_auto(true);
    inject_level(1, -40.0f);
    EXPECT_EQ(pipeline.gate_candidate_frames(), 2u);
}

}  // namespace
