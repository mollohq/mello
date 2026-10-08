// Speech gate on the capture path (spec 10 section 4.1): a real AudioPipeline
// on the device-free backend (MELLO_AUDIO_BACKEND=test) with Silero VAD, fed
// with the speech corpus of the voice quality gate after a quiet floor.
//
// The echo canceller, AGC2 and noise suppression are off, so the frame that
// reaches the encoder is the injected frame times the expander gain. The
// encode tap shows each frame and its media time.

#include <gtest/gtest.h>

#include "audio/audio_pipeline.hpp"

#include <cmath>
#include <cstring>
#include <fstream>
#include <iterator>
#include <string>
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

double rms(const int16_t* x, size_t n) {
    double sum = 0.0;
    for (size_t i = 0; i < n; ++i) sum += static_cast<double>(x[i]) * x[i];
    return n == 0 ? 0.0 : std::sqrt(sum / static_cast<double>(n));
}

double db(double ratio) { return 20.0 * std::log10(ratio); }

// One frame at the encoder input.
struct TapFrame {
    uint32_t timestamp;  // 48 kHz capture sample index of its first sample
    double rms;
    size_t call;         // which inject call encoded it
};

constexpr size_t kChunk = FRAME_SIZE / 2;  // 10 ms, like a device callback
constexpr int kSampleRate = SAMPLE_RATE;

class SpeechGateTest : public ::testing::Test {
protected:
    AudioPipeline pipeline;
    std::vector<int16_t> input;     // everything injected, in order
    std::vector<TapFrame> encoded;  // in encode order
    size_t calls = 0;

    void SetUp() override {
        select_test_backend();
        ASSERT_TRUE(pipeline.initialize());
        pipeline.set_echo_cancellation(false);
        pipeline.set_agc(false);
        pipeline.set_ns_mode(NsMode::Off);
        pipeline.set_push_to_talk(false);
        ASSERT_TRUE(pipeline.start_capture_inject());
        pipeline.set_encode_tap([this](const int16_t* pcm, int n, uint32_t ts) {
            encoded.push_back({ts, rms(pcm, static_cast<size_t>(n)), calls});
        });
    }

    void TearDown() override {
        pipeline.set_encode_tap(nullptr);
        pipeline.shutdown();
    }

    // A quiet floor like the voice gate corpus: seeded noise at -70 dBFS.
    void append_floor(double seconds) {
        uint32_t state = 20261008u;
        const double level = 32768.0 * std::pow(10.0, -70.0 / 20.0) * std::sqrt(3.0);
        const size_t n = static_cast<size_t>(seconds * kSampleRate);
        for (size_t i = 0; i < n; ++i) {
            state = state * 1664525u + 1013904223u;
            const double u = static_cast<double>(state >> 8) / static_cast<double>(1u << 24);
            input.push_back(static_cast<int16_t>(std::lround((2.0 * u - 1.0) * level)));
        }
    }

    // Returns the input position of the clip's speech onset: its first
    // 10 ms at -45 dBFS or more (the voice gate's talk-spurt start).
    size_t append_clip(const std::vector<int16_t>& clip, double seconds) {
        const size_t start = input.size();
        const size_t n = std::min(clip.size(), static_cast<size_t>(seconds * kSampleRate));
        input.insert(input.end(), clip.begin(), clip.begin() + static_cast<std::ptrdiff_t>(n));
        const double active = 32768.0 * std::pow(10.0, -45.0 / 20.0);
        for (size_t k = start; k + kChunk <= input.size(); k += kChunk) {
            if (rms(&input[k], kChunk) >= active) return k;
        }
        return input.size();
    }

    void inject_all() {
        for (size_t k = 0; k + kChunk <= input.size(); k += kChunk) {
            pipeline.inject_capture(&input[k], static_cast<int>(kChunk));
            ++calls;
        }
    }

    double input_rms(uint32_t timestamp) const {
        return rms(&input[timestamp], FRAME_SIZE);
    }
};

// Pre-roll exempt from the expander (plans/voice-quality.md stage 3b). The
// frames kept before the gate opens were captured while Silero said "no
// speech", when the expander attenuates toward its floor (-20 dB). They are
// the start of the word: they must leave at the level they were captured.
TEST_F(SpeechGateTest, PreRollLeavesAtTheCapturedLevel) {
    const auto clip = load_corpus_clip("m1_davison.wav");
    ASSERT_GT(clip.size(), static_cast<size_t>(3 * kSampleRate)) << "corpus clip missing";
    constexpr int kPreRoll = 5;
    pipeline.set_speech_pre_roll_frames(kPreRoll);
    append_floor(1.5);
    append_clip(clip, 3.0);
    inject_all();

    ASSERT_FALSE(encoded.empty()) << "the gate never opened (Silero model not found?)";
    // The first gate open: the pre-roll and the live frame leave in one call.
    std::vector<TapFrame> first;
    for (const auto& f : encoded) {
        if (f.call == encoded.front().call) first.push_back(f);
    }
    ASSERT_EQ(first.size(), static_cast<size_t>(kPreRoll + 1));
    for (int i = 0; i < kPreRoll; ++i) {
        const double in = input_rms(first[i].timestamp);
        ASSERT_GT(in, 0.0);
        EXPECT_NEAR(db(first[i].rms / in), 0.0, 0.5)
            << "pre-roll frame " << i << " at " << first[i].timestamp / 48 << " ms";
    }
    // The frame that opens the gate follows at unity too, not at the start
    // of an expander attack from its floor.
    EXPECT_NEAR(db(first.back().rms / input_rms(first.back().timestamp)), 0.0, 0.5)
        << "the first live frame";
}

}  // namespace
