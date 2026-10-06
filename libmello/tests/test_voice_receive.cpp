// Voice receive path through a real AudioPipeline: jitter buffer, decode,
// concealment and mix. Device-free: MELLO_AUDIO_BACKEND=test selects the test
// backend, and util::set_test_clock_ms() drives the playout clock in 1 ms
// steps, the same method as the voice quality gate (tools/voice-gate). No
// sleeps, so every run is the same.

#include <gtest/gtest.h>

#include "audio/audio_pipeline.hpp"
#include "audio/opus_codec.hpp"
#include "util/test_clock.hpp"

#include <algorithm>
#include <cmath>
#include <cstdlib>
#include <cstring>
#include <set>
#include <utility>
#include <vector>

#ifdef _WIN32
#include <windows.h>
#endif

namespace {

using namespace mello::audio;

constexpr int64_t kClockOrigin = 1'000'000;
constexpr char kPeer[] = "peer-1";

void select_test_backend() {
#ifdef _WIN32
    SetEnvironmentVariableA("MELLO_AUDIO_BACKEND", "test");
#else
    setenv("MELLO_AUDIO_BACKEND", "test", 1);
#endif
}

class VoiceReceiveTest : public ::testing::Test {
protected:
    AudioPipeline pipeline;
    OpusEnc encoder;
    std::vector<int16_t> output;

    void SetUp() override {
        select_test_backend();
        mello::util::set_test_clock_ms(kClockOrigin);
        ASSERT_TRUE(pipeline.initialize());
        ASSERT_TRUE(encoder.initialize());
    }

    void TearDown() override {
        pipeline.shutdown();
        mello::util::set_test_clock_ms(-1);
    }

    // One 20 ms packet as the sender produces it: a 4-byte little-endian
    // sequence header and an Opus frame of a continuous 300 Hz tone.
    std::vector<uint8_t> encode_packet(uint32_t seq) {
        int16_t pcm[FRAME_SIZE];
        for (int i = 0; i < FRAME_SIZE; ++i) {
            const double t = (static_cast<double>(seq) * FRAME_SIZE + i) / SAMPLE_RATE;
            pcm[i] = static_cast<int16_t>(8000.0 * std::sin(2.0 * 3.14159265358979 * 300.0 * t));
        }
        uint8_t payload[MAX_PACKET_SIZE];
        const int n = encoder.encode(pcm, FRAME_SIZE, payload, MAX_PACKET_SIZE);
        EXPECT_GT(n, 0);
        std::vector<uint8_t> pkt(4 + static_cast<size_t>(n));
        pkt[0] = static_cast<uint8_t>(seq);
        pkt[1] = static_cast<uint8_t>(seq >> 8);
        pkt[2] = static_cast<uint8_t>(seq >> 16);
        pkt[3] = static_cast<uint8_t>(seq >> 24);
        std::memcpy(pkt.data() + 4, payload, static_cast<size_t>(n));
        return pkt;
    }

    // A packet that reaches the receiver at `arrival_ms` (receiver clock).
    struct Arrival {
        int64_t arrival_ms;
        std::vector<uint8_t> bytes;
    };

    // Encode `count` packets in order (the sender encodes every frame) and
    // deliver all but `lost`, sent every 20 ms with a 20 ms one-way delay.
    std::vector<Arrival> steady_stream(uint32_t count, const std::set<uint32_t>& lost) {
        std::vector<Arrival> arrivals;
        for (uint32_t seq = 0; seq < count; ++seq) {
            auto pkt = encode_packet(seq);
            if (lost.count(seq) == 0) {
                arrivals.push_back({static_cast<int64_t>(seq) * 20 + 20, std::move(pkt)});
            }
        }
        return arrivals;
    }

    // Run the receiver clock from 0 to `end_ms` in 1 ms steps: deliver the
    // packets that are due, and pull 10 ms of mixed output every 10 ms, as
    // the playback device thread does.
    void play(std::vector<Arrival> arrivals, int64_t end_ms) {
        std::stable_sort(arrivals.begin(), arrivals.end(),
                         [](const Arrival& a, const Arrival& b) {
                             return a.arrival_ms < b.arrival_ms;
                         });
        size_t next = 0;
        std::vector<int16_t> pull(480);
        for (int64_t now = 0; now <= end_ms; ++now) {
            mello::util::set_test_clock_ms(kClockOrigin + now);
            while (next < arrivals.size() && arrivals[next].arrival_ms <= now) {
                const auto& a = arrivals[next++];
                pipeline.feed_packet(kPeer, a.bytes.data(), static_cast<int>(a.bytes.size()));
            }
            if (now % 10 == 0) {
                ASSERT_GE(pipeline.render_test_output(pull.data(), pull.size()), 0);
                output.insert(output.end(), pull.begin(), pull.end());
            }
        }
    }

    // Concealment frames produced for lost packets (not for an empty
    // playout buffer).
    static uint32_t loss_concealment(const ReceiveStats& s) {
        return s.conceal_missing_plc + s.conceal_gap_fec + s.conceal_gap_plc;
    }
};

// One concealment per lost frame (plans/voice-quality.md section 3). A lost
// 20 ms frame must produce 20 ms of concealed audio, never 40. Single, double
// and triple losses.
TEST_F(VoiceReceiveTest, EachLostFrameIsConcealedOnce) {
    const std::set<uint32_t> lost{20, 40, 41, 60, 61, 62};
    play(steady_stream(100, lost), 100 * 20 + 500);

    const auto s = pipeline.receive_stats();
    EXPECT_EQ(s.jitter_missing, lost.size());
    EXPECT_EQ(loss_concealment(s), lost.size())
        << "missing_plc=" << s.conceal_missing_plc << " gap_fec=" << s.conceal_gap_fec
        << " gap_plc=" << s.conceal_gap_plc;
    EXPECT_EQ(s.frames_decoded, 100u - lost.size());
    EXPECT_EQ(s.decode_errors, 0u);
}

// A single loss whose successor is already buffered is concealed from the
// successor's in-band FEC, once, and not with PLC as well.
TEST_F(VoiceReceiveTest, SingleLossUsesFecFromTheNextPacket) {
    play(steady_stream(60, {30}), 60 * 20 + 500);

    const auto s = pipeline.receive_stats();
    EXPECT_EQ(s.conceal_gap_fec, 1u);
    EXPECT_EQ(s.conceal_missing_plc, 0u);
    EXPECT_EQ(s.conceal_gap_plc, 0u);
}

}  // namespace
