// Voice capture path through a real AudioPipeline on the device-free backend
// (MELLO_AUDIO_BACKEND=test): capture inject, DSP, Opus encode, get_packet.

#include <gtest/gtest.h>

#include "audio/audio_pipeline.hpp"
#include "audio/opus_codec.hpp"

#include <atomic>
#include <chrono>
#include <cmath>
#include <condition_variable>
#include <cstdlib>
#include <mutex>
#include <thread>
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

// Packet sink (spec 10 section 4.5): frames leave on the capture thread.
class PacketSinkTest : public VoiceCaptureTest {
protected:
    std::mutex mutex;
    std::vector<SentPacket> sunk;
    std::vector<int> sizes;

    void TearDown() override {
        pipeline.set_packet_sink(nullptr);
        VoiceCaptureTest::TearDown();
    }

    void set_collecting_sink() {
        pipeline.set_packet_sink([this](const uint8_t* data, int size, uint32_t ts, uint32_t seq) {
            ASSERT_NE(data, nullptr);
            std::lock_guard<std::mutex> lock(mutex);
            sunk.push_back({seq, ts});
            sizes.push_back(size);
        });
    }

    std::vector<SentPacket> taken() {
        std::lock_guard<std::mutex> lock(mutex);
        return sunk;
    }
};

// Each encoded frame reaches the sink once, with its media time and
// sequence, and not the get_packet queue.
TEST_F(PacketSinkTest, EachFrameGoesToTheSinkAndNotToTheQueue) {
    set_collecting_sink();
    inject(5);

    const auto got = taken();
    ASSERT_EQ(got.size(), 5u);
    for (size_t i = 1; i < got.size(); ++i) {
        EXPECT_EQ(got[i].sequence, got[i - 1].sequence + 1);
        EXPECT_EQ(got[i].timestamp - got[i - 1].timestamp, 960u) << "frame " << i;
    }
    for (int size : sizes) {
        EXPECT_GT(size, 0);
        EXPECT_LE(size, MAX_PACKET_SIZE);
    }
    EXPECT_TRUE(drain().empty()) << "a sunk frame was also queued";
}

// A cleared sink sends frames to the queue again. The sequence and the
// media time continue across the switch.
TEST_F(PacketSinkTest, ClearingTheSinkReturnsFramesToTheQueue) {
    set_collecting_sink();
    inject(2);
    pipeline.set_packet_sink(nullptr);
    inject(3);

    const auto got = taken();
    const auto queued = drain();
    ASSERT_EQ(got.size(), 2u);
    ASSERT_EQ(queued.size(), 3u);
    EXPECT_EQ(queued[0].sequence, got[1].sequence + 1);
    EXPECT_EQ(queued[0].timestamp - got[1].timestamp, 960u);
}

// Frames that wait in the queue when a sink is set would leave late or reach
// a later get_packet caller as stale audio. Setting the sink drops them.
TEST_F(PacketSinkTest, SettingASinkDropsQueuedFrames) {
    inject(3);
    set_collecting_sink();
    EXPECT_TRUE(drain().empty());
    inject(1);
    EXPECT_EQ(taken().size(), 1u);
}

// Mute gates frames before encode, also with a sink.
TEST_F(PacketSinkTest, MutedFramesDoNotReachTheSink) {
    set_collecting_sink();
    pipeline.set_mute(true);
    inject(5);
    EXPECT_TRUE(taken().empty());
    pipeline.set_mute(false);
    inject(1);
    const auto got = taken();
    ASSERT_EQ(got.size(), 1u);
    EXPECT_EQ(got[0].timestamp, 5u * 960u) << "muted frames still advance the media time";
}

// A clear waits for a sink call that runs now, so the caller may free the
// sink's state when the clear returns.
TEST_F(PacketSinkTest, ClearWaitsForARunningSinkCall) {
    std::mutex gate_mutex;
    std::condition_variable gate_cv;
    bool entered = false;
    bool release = false;
    std::atomic<bool> sink_returned{false};

    pipeline.set_packet_sink([&](const uint8_t*, int, uint32_t, uint32_t) {
        std::unique_lock<std::mutex> lock(gate_mutex);
        entered = true;
        gate_cv.notify_all();
        gate_cv.wait(lock, [&] { return release; });
        sink_returned.store(true);
    });

    std::thread capture([this] { inject(1); });
    {
        std::unique_lock<std::mutex> lock(gate_mutex);
        ASSERT_TRUE(gate_cv.wait_for(lock, std::chrono::seconds(5), [&] { return entered; }));
    }

    std::atomic<bool> cleared{false};
    std::atomic<bool> sink_returned_before_clear{false};
    std::thread clearer([&] {
        pipeline.set_packet_sink(nullptr);
        sink_returned_before_clear.store(sink_returned.load());
        cleared.store(true);
    });

    // The sink still runs: the clear must not return.
    std::this_thread::sleep_for(std::chrono::milliseconds(100));
    EXPECT_FALSE(cleared.load()) << "the clear returned while the sink ran";

    {
        std::lock_guard<std::mutex> lock(gate_mutex);
        release = true;
    }
    gate_cv.notify_all();
    capture.join();
    clearer.join();
    EXPECT_TRUE(cleared.load());
    EXPECT_TRUE(sink_returned_before_clear.load());
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
