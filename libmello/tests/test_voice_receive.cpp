// Voice receive path through a real AudioPipeline: NetEQ per peer, the mixer,
// the output ring and the render path (spec 10 section 5). Device-free:
// MELLO_AUDIO_BACKEND=test selects the test backend, and
// util::set_test_clock_ms() drives the playout clock in 1 ms steps, the same
// method as the voice quality gate (tools/voice-gate). The test pulls 10 ms
// of output every 10 ms, as the playback device thread does. No sleeps, so
// every run is the same.

#include <gtest/gtest.h>

#include "audio/audio_pipeline.hpp"
#include "audio/opus_codec.hpp"
#include "util/test_clock.hpp"

#include <algorithm>
#include <cmath>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <functional>
#include <iterator>
#include <set>
#include <string>
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

// 16-bit mono 48 kHz WAV from the voice gate corpus. Empty on any other
// format.
std::vector<int16_t> load_corpus_clip(const char* name) {
    std::ifstream f(std::string(MELLO_VOICE_FIXTURE_DIR) + "/" + name, std::ios::binary);
    std::vector<char> b((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
    auto u32 = [&](size_t o) {
        return static_cast<uint32_t>(static_cast<uint8_t>(b[o])) |
               (static_cast<uint32_t>(static_cast<uint8_t>(b[o + 1])) << 8) |
               (static_cast<uint32_t>(static_cast<uint8_t>(b[o + 2])) << 16) |
               (static_cast<uint32_t>(static_cast<uint8_t>(b[o + 3])) << 24);
    };
    if (b.size() < 12 || std::memcmp(b.data(), "RIFF", 4) != 0) return {};
    size_t pos = 12;
    while (pos + 8 <= b.size()) {
        const uint32_t len = u32(pos + 4);
        if (std::memcmp(b.data() + pos, "data", 4) == 0) {
            const size_t n = std::min<size_t>(len, b.size() - pos - 8) / 2;
            std::vector<int16_t> out(n);
            std::memcpy(out.data(), b.data() + pos + 8, n * 2);
            return out;
        }
        pos += 8 + len + (len & 1);
    }
    return {};
}

double rms(const std::vector<int16_t>& x) {
    double sum = 0.0;
    for (int16_t v : x) sum += static_cast<double>(v) * v;
    return x.empty() ? 0.0 : std::sqrt(sum / static_cast<double>(x.size()));
}

class VoiceReceiveTest : public ::testing::Test {
protected:
    AudioPipeline pipeline;
    OpusEnc encoder;

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

    // One 20 ms Opus frame of a continuous 300 Hz tone.
    std::vector<uint8_t> opus_frame(uint32_t index) {
        int16_t pcm[FRAME_SIZE];
        for (int i = 0; i < FRAME_SIZE; ++i) {
            const double t = (static_cast<double>(index) * FRAME_SIZE + i) / SAMPLE_RATE;
            pcm[i] = static_cast<int16_t>(8000.0 * std::sin(2.0 * 3.14159265358979 * 300.0 * t));
        }
        uint8_t payload[MAX_PACKET_SIZE];
        const int n = encoder.encode(pcm, FRAME_SIZE, payload, MAX_PACKET_SIZE);
        EXPECT_GT(n, 0);
        return std::vector<uint8_t>(payload, payload + std::max(n, 0));
    }

    // The loopback voice frame (spec 10 section 4.2): 32-bit sequence and 32-bit
    // RTP timestamp, little-endian, then Opus.
    std::vector<uint8_t> voice_frame(uint32_t seq, uint32_t ts) {
        const auto opus = opus_frame(seq);
        std::vector<uint8_t> f(VOICE_FRAME_HEADER_SIZE);
        for (int i = 0; i < 4; ++i) {
            f[i] = static_cast<uint8_t>(seq >> (8 * i));
            f[4 + i] = static_cast<uint8_t>(ts >> (8 * i));
        }
        f.insert(f.end(), opus.begin(), opus.end());
        return f;
    }

    // A complete RTP packet as the SFU forwards it.
    std::vector<uint8_t> rtp_packet(uint32_t index, uint16_t seq, uint32_t ts, uint32_t ssrc) {
        const auto opus = opus_frame(index);
        std::vector<uint8_t> p{0x80, 111, static_cast<uint8_t>(seq >> 8), static_cast<uint8_t>(seq),
                               static_cast<uint8_t>(ts >> 24), static_cast<uint8_t>(ts >> 16),
                               static_cast<uint8_t>(ts >> 8), static_cast<uint8_t>(ts),
                               static_cast<uint8_t>(ssrc >> 24), static_cast<uint8_t>(ssrc >> 16),
                               static_cast<uint8_t>(ssrc >> 8), static_cast<uint8_t>(ssrc)};
        p.insert(p.end(), opus.begin(), opus.end());
        return p;
    }

    struct Arrival {
        int64_t arrival_ms;
        bool rtp;
        std::vector<uint8_t> bytes;
    };

    // `count` loopback frames sent every 20 ms with a 20 ms one-way delay,
    // except the `lost` ones.
    std::vector<Arrival> frame_stream(uint32_t count, const std::set<uint32_t>& lost = {}) {
        std::vector<Arrival> out;
        for (uint32_t seq = 0; seq < count; ++seq) {
            auto f = voice_frame(seq, 960 * seq);
            if (lost.count(seq) == 0) {
                out.push_back({static_cast<int64_t>(seq) * 20 + 20, false, std::move(f)});
            }
        }
        return out;
    }

    // Run the clock from 0 to end_ms in 1 ms steps: deliver what is due and
    // pull 10 ms every 10 ms. on_pull sees each pulled chunk.
    void play(std::vector<Arrival> arrivals, int64_t end_ms,
              const std::function<void(int64_t, const std::vector<int16_t>&)>& on_pull = {}) {
        std::stable_sort(arrivals.begin(), arrivals.end(),
                         [](const Arrival& a, const Arrival& b) { return a.arrival_ms < b.arrival_ms; });
        size_t next = 0;
        std::vector<int16_t> pull(480);
        for (int64_t now = 0; now <= end_ms; ++now) {
            mello::util::set_test_clock_ms(kClockOrigin + now);
            while (next < arrivals.size() && arrivals[next].arrival_ms <= now) {
                const auto& a = arrivals[next++];
                if (a.rtp) {
                    pipeline.feed_rtp(kPeer, a.bytes.data(), static_cast<int>(a.bytes.size()), -1);
                } else {
                    pipeline.feed_packet(kPeer, a.bytes.data(), static_cast<int>(a.bytes.size()));
                }
            }
            if (now % 10 == 0) {
                ASSERT_GE(pipeline.render_test_output(pull.data(), pull.size()), 0);
                if (on_pull) on_pull(now, pull);
            }
        }
    }

    PlayoutStats stats() const { return pipeline.receive_stats().playout; }
};

// The loopback framing (8-byte header) decodes through NetEQ.
TEST_F(VoiceReceiveTest, LoopbackFrameDecodesThroughNetEq) {
    int audible = 0;
    play(frame_stream(150), 150 * 20 + 200, [&](int64_t t, const std::vector<int16_t>& pcm) {
        if (t > 300 && t < 2900 && rms(pcm) > 2000.0) ++audible;
    });
    const auto s = stats();
    EXPECT_GT(audible, 240) << "the tone must play";
    EXPECT_EQ(s.packets_inserted, 150u);
    EXPECT_EQ(s.insert_errors, 0u);
    EXPECT_EQ(s.get_audio_errors, 0u);
    EXPECT_EQ(s.packets_rejected, 0u);
    EXPECT_EQ(pipeline.active_streams(), 1);
}

// A frame with no payload behind the 8-byte header is not a frame.
TEST_F(VoiceReceiveTest, ShortLoopbackFrameIsIgnored) {
    const auto f = voice_frame(0, 0);
    pipeline.feed_packet(kPeer, f.data(), VOICE_FRAME_HEADER_SIZE);
    EXPECT_EQ(stats().packets_inserted, 0u);
    EXPECT_EQ(pipeline.active_streams(), 0);
}

// The SFU path: complete RTP packets with the real sequence and timestamp.
TEST_F(VoiceReceiveTest, SfuRtpPacketsDecodeThroughNetEq) {
    std::vector<Arrival> arrivals;
    for (uint32_t i = 0; i < 150; ++i) {
        arrivals.push_back({static_cast<int64_t>(i) * 20 + 20, true,
                            rtp_packet(i, static_cast<uint16_t>(65500 + i), 0xFFFF0000u + 960 * i,
                                       0xCAFE)});
    }
    int audible = 0;
    play(std::move(arrivals), 150 * 20 + 200, [&](int64_t t, const std::vector<int16_t>& pcm) {
        if (t > 300 && t < 2900 && rms(pcm) > 2000.0) ++audible;
    });
    const auto s = stats();
    EXPECT_GT(audible, 240) << "the tone must play across the sequence and timestamp wrap";
    EXPECT_EQ(s.insert_errors, 0u);
    EXPECT_EQ(s.stream_resets, 0u);
    EXPECT_EQ(s.packets_discarded, 0u);
}

// One concealment per lost frame (plans/voice-quality.md section 3): losses
// do not add delay. NetEQ conceals on its timeline, so the buffer delay after
// single, double and triple losses is the delay before them.
TEST_F(VoiceReceiveTest, LossesAddNoDelay) {
    const std::set<uint32_t> lost{60, 80, 81, 100, 101, 102};
    float before = -1.0f;
    float after = -1.0f;
    uint64_t concealed_before = 0;
    uint64_t concealed_after = 0;
    play(frame_stream(200, lost), 200 * 20 + 300, [&](int64_t t, const std::vector<int16_t>&) {
        if (t == 1150) {
            before = stats().filtered_delay_ms;
            concealed_before = stats().concealed_samples;
        }
        if (t == 3900) {
            after = stats().filtered_delay_ms;
            concealed_after = stats().concealed_samples;
        }
    });
    ASSERT_GE(before, 0.0f);
    ASSERT_GE(after, 0.0f);
    EXPECT_LE(after, before + 20.0f) << "before=" << before << " after=" << after;
    // Concealment covers the lost time once: 6 frames of 20 ms, plus at most
    // one 10 ms frame of overlap per loss run.
    const uint64_t concealed = concealed_after - concealed_before;
    EXPECT_LE(concealed, 6u * 960u + 3u * 480u) << "concealed=" << concealed;
    EXPECT_GE(concealed, 6u * 960u / 2u) << "concealed=" << concealed;
    EXPECT_EQ(stats().get_audio_errors, 0u);
}

// Single losses in speech whose successor has arrived are repaired from the
// successor's in-band FEC, not concealed.
TEST_F(VoiceReceiveTest, SingleLossesUseFecFromTheNextPacket) {
    const auto speech = load_corpus_clip("m1_davison.wav");
    ASSERT_GT(speech.size(), 48000u * 4) << "corpus clip missing";
    OpusEnc speech_encoder;
    ASSERT_TRUE(speech_encoder.initialize());
    std::vector<Arrival> arrivals;
    uint32_t sent = 0;
    for (uint32_t i = 0; i < 200; ++i) {
        uint8_t opus[MAX_PACKET_SIZE];
        const int n = speech_encoder.encode(speech.data() + i * FRAME_SIZE, FRAME_SIZE, opus,
                                            MAX_PACKET_SIZE);
        ASSERT_GT(n, 0);
        if (i % 20 == 10) continue;  // 10 single losses
        std::vector<uint8_t> f(VOICE_FRAME_HEADER_SIZE);
        for (int k = 0; k < 4; ++k) {
            f[k] = static_cast<uint8_t>(i >> (8 * k));
            f[4 + k] = static_cast<uint8_t>((960 * i) >> (8 * k));
        }
        f.insert(f.end(), opus, opus + n);
        arrivals.push_back({static_cast<int64_t>(i) * 20 + 20, false, std::move(f)});
        ++sent;
    }
    play(std::move(arrivals), 200 * 20 + 100);
    const auto s = stats();
    EXPECT_EQ(s.packets_inserted, sent);
    EXPECT_GT(s.fec_packets_received, 100u) << "the speech packets carry no FEC";
    const uint64_t fec_used = s.fec_packets_received - s.fec_packets_discarded;
    EXPECT_GE(fec_used, 5u) << "FEC repaired fewer than half of the 10 losses";
    EXPECT_LE(fec_used, 10u) << "FEC used for frames that were not lost";
}

// A 5 s outage: every packet in it is lost. When packets return, the delay
// is back at its value before the outage within 2 s.
TEST_F(VoiceReceiveTest, OutageRecoversToThePreOutageDelay) {
    std::set<uint32_t> lost;
    for (uint32_t seq = 100; seq < 350; ++seq) lost.insert(seq);
    float before = -1.0f;
    float after = -1.0f;
    const int64_t outage_end_ms = 350 * 20 + 20;
    play(frame_stream(500, lost), 500 * 20 + 300, [&](int64_t t, const std::vector<int16_t>&) {
        if (t == 1900) before = stats().filtered_delay_ms;
        if (t == outage_end_ms + 2000 - (outage_end_ms + 2000) % 10) after = stats().filtered_delay_ms;
    });
    ASSERT_GE(before, 0.0f);
    ASSERT_GE(after, 0.0f);
    EXPECT_LE(after, before + 20.0f) << "before=" << before << " after=" << after;
}

// Late audio reaches the underrun health. A 200 ms stall delays packets
// 50..59 and one of them is lost, so NetEQ expands while packets wait.
TEST_F(VoiceReceiveTest, StallWithPacketsWaitingCountsAsLateUnderrun) {
    auto arrivals = frame_stream(100, {53});
    for (auto& a : arrivals) {
        if (a.arrival_ms >= 1020 && a.arrival_ms <= 1200) a.arrival_ms = 1220;
    }
    play(std::move(arrivals), 100 * 20 + 500);
    EXPECT_GT(stats().late_frames, 0u);
}

// A talk pause is not an underrun: nothing waits in the buffer.
TEST_F(VoiceReceiveTest, TalkPauseIsNotALateUnderrun) {
    play(frame_stream(50), 50 * 20 + 2000);
    const auto s = stats();
    EXPECT_GT(s.concealed_samples, 0u) << "NetEQ must have expanded into the pause";
    EXPECT_EQ(s.late_frames, 0u);
}

// Deafen silences the output but keeps each peer's NetEQ running, so the
// voice is back at once on undeafen.
TEST_F(VoiceReceiveTest, DeafenSilencesOutputAndKeepsTheStream) {
    double deaf_rms = 0.0;
    int audible_after = 0;
    play(frame_stream(200), 200 * 20 + 200, [&](int64_t t, const std::vector<int16_t>& pcm) {
        if (t == 1000) pipeline.set_deafen(true);
        if (t > 1000 && t < 2000) deaf_rms = std::max(deaf_rms, rms(pcm));
        if (t == 2000) pipeline.set_deafen(false);
        if (t > 2050 && t < 2500 && rms(pcm) > 2000.0) ++audible_after;
    });
    EXPECT_EQ(deaf_rms, 0.0);
    EXPECT_GT(audible_after, 40);
    EXPECT_EQ(stats().stream_resets, 0u);
}

// Output volume and per-peer volume apply before the mixer.
TEST_F(VoiceReceiveTest, PeerVolumeAndOutputVolumeScaleThePeer) {
    std::vector<double> levels;
    pipeline.set_peer_volume(kPeer, 0.5f);
    play(frame_stream(100), 100 * 20, [&](int64_t t, const std::vector<int16_t>& pcm) {
        if (t == 1000) pipeline.set_output_volume(0.5f);
        if ((t > 400 && t < 900) || (t > 1100 && t < 1900)) levels.push_back(rms(pcm));
    });
    EXPECT_FLOAT_EQ(pipeline.peer_volume(kPeer), 0.5f);
    ASSERT_GT(levels.size(), 100u);
    const double first = levels.front();
    const double last = levels.back();
    EXPECT_NEAR(first, 8000.0 / std::sqrt(2.0) * 0.5, 400.0);
    EXPECT_NEAR(last, first * 0.5, 200.0);
}

// Leaving voice destroys every NetEQ; removing a peer destroys its own.
TEST_F(VoiceReceiveTest, LeaveAndPeerRemovalDestroyStreams) {
    play(frame_stream(20), 500);
    EXPECT_EQ(pipeline.active_streams(), 1);
    pipeline.remove_peer(kPeer);
    EXPECT_EQ(pipeline.active_streams(), 0);
    const auto f = voice_frame(30, 30 * 960);
    pipeline.feed_packet(kPeer, f.data(), static_cast<int>(f.size()));
    EXPECT_EQ(pipeline.active_streams(), 1);
    pipeline.stop_capture();
    EXPECT_EQ(pipeline.active_streams(), 0);
    EXPECT_EQ(pipeline.pipeline_delay_ms(), 0.0f);
}

}  // namespace
