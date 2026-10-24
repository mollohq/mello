// Covers StreamAudioHostPipeline::feed_float_pcm, the macOS game-audio path.
//
// macOS has no loopback device, so ScreenCaptureKit pushes float PCM straight
// into the pipeline with whatever channel count and rate the OS chose. The
// channel mapping is the risky part: get it wrong and the result is not a
// crash but silence in one ear, or a tail of dead samples in every 20 ms frame.
//
// These tests decode the emitted Opus packets rather than counting them. Packet
// count alone is driven by the buffer size and stays correct even when the
// conversion loop fills only part of it — an earlier version of this file
// asserted counts and passed against a deliberately broken mapping.
//
// Gated to Apple because it is the only platform that calls feed_float_pcm, and
// because start() opens a real WASAPI device on Windows.
#ifdef __APPLE__

#include "audio/opus_codec.hpp"
#include "audio/stream_audio_pipeline.hpp"

#include <gtest/gtest.h>
#include <cmath>
#include <vector>

using mello::audio::OpusDec;
using mello::audio::StreamAudioHostPipeline;
using mello::audio::STREAM_AUDIO_CHANNELS;
using mello::audio::STREAM_AUDIO_FRAME_SAMPLES;
using mello::audio::STREAM_AUDIO_SAMPLE_RATE;

namespace {

constexpr uint32_t kFrames = static_cast<uint32_t>(STREAM_AUDIO_FRAME_SAMPLES); // 20 ms

/// A tone, not silence: Opus DTX legitimately encodes silence to zero bytes,
/// so a silent fixture would make every assertion below vacuously pass.
///
/// 400 Hz is exactly 8 cycles per 20 ms frame at 48 kHz, so repeating the same
/// buffer is phase-continuous and does not click at the frame boundary.
std::vector<float> tone(uint32_t frames, uint32_t channels, float amplitude = 0.5f) {
    std::vector<float> out(static_cast<size_t>(frames) * channels);
    for (uint32_t i = 0; i < frames; ++i) {
        const float v = amplitude * std::sin(2.0f * 3.14159265f * 400.0f *
                                             static_cast<float>(i) / 48000.0f);
        for (uint32_t c = 0; c < channels; ++c) {
            out[static_cast<size_t>(i) * channels + c] = v;
        }
    }
    return out;
}

/// Runs the pipeline and decodes what it emits back to PCM.
struct Harness {
    StreamAudioHostPipeline pipeline;
    std::vector<std::vector<uint8_t>> packets;
    OpusDec decoder;

    Harness() {
        EXPECT_TRUE(pipeline.start([this](const uint8_t* d, int size, uint64_t) {
            if (size > 0) packets.emplace_back(d, d + size);
        }));
        EXPECT_TRUE(decoder.initialize(STREAM_AUDIO_SAMPLE_RATE, STREAM_AUDIO_CHANNELS));
    }

    void feed(const std::vector<float>& pcm, uint32_t frames, uint32_t channels,
              uint32_t rate = STREAM_AUDIO_SAMPLE_RATE) {
        pipeline.feed_float_pcm(pcm.data(), frames, channels, rate);
    }

    /// Decode every packet in order and return the last frame.
    ///
    /// The decoder is stateful: handing it one packet in isolation produces a
    /// fade-in, which reads as near-silence and would fail every assertion
    /// below for the wrong reason. Feeding the whole sequence also matches what
    /// the viewer actually does.
    std::vector<int16_t> decode_last() {
        EXPECT_FALSE(packets.empty());
        std::vector<int16_t> out;
        for (const auto& packet : packets) {
            out.assign(static_cast<size_t>(STREAM_AUDIO_FRAME_SAMPLES) * STREAM_AUDIO_CHANNELS, 0);
            const int decoded = decoder.decode(packet.data(), static_cast<int>(packet.size()),
                                               out.data(), STREAM_AUDIO_FRAME_SAMPLES);
            EXPECT_GT(decoded, 0);
            out.resize(static_cast<size_t>(decoded) * STREAM_AUDIO_CHANNELS);
        }
        return out;
    }
};

/// RMS of one channel over a half-open frame range, normalised to 0..1.
double rms(const std::vector<int16_t>& pcm, uint32_t channel,
           size_t first_frame, size_t last_frame) {
    double sum = 0.0;
    size_t n = 0;
    for (size_t i = first_frame; i < last_frame; ++i) {
        const size_t idx = i * STREAM_AUDIO_CHANNELS + channel;
        if (idx >= pcm.size()) break;
        const double v = pcm[idx] / 32768.0;
        sum += v * v;
        ++n;
    }
    return n ? std::sqrt(sum / static_cast<double>(n)) : 0.0;
}

/// Feed several frames so assertions can skip Opus's encoder lookahead, which
/// makes the first packet partly silent no matter what the input was.
constexpr int kWarmupFrames = 4;

/// RMS of the 0.5-amplitude sine the fixtures use: 0.5 / sqrt(2).
///
/// Assertions compare against this rather than against "louder than silence".
/// A conversion that fills only part of each frame still leaves the tail
/// non-silent — stale buffer contents and Opus's encoder delay smear signal
/// into it — so a mere non-zero check passes against a broken mapping. Level
/// is the property that actually separates them: correct conversion measures
/// 0.354, dropping half the frame measures 0.21-0.28.
constexpr double kToneRms = 0.354;
constexpr double kToneRmsFloor = 0.30;

} // namespace

// start() must not need a capture device on macOS: audio arrives from the
// ScreenCaptureKit stream that is already running for video.
TEST(StreamAudioHostPipeline, StartsWithoutACaptureDevice) {
    StreamAudioHostPipeline pipeline;
    EXPECT_TRUE(pipeline.start([](const uint8_t*, int, uint64_t) {}));
    pipeline.stop();
}

// The whole 20 ms frame must carry signal. Converting only part of the buffer
// leaves a silent tail that no packet count would reveal.
TEST(StreamAudioHostPipeline, StereoFillsTheWholeFrame) {
    Harness h;
    const auto pcm = tone(kFrames, 2);
    for (int i = 0; i < kWarmupFrames; ++i) h.feed(pcm, kFrames, 2);
    ASSERT_GE(h.packets.size(), 2u);

    const auto out = h.decode_last();
    const size_t frames = out.size() / STREAM_AUDIO_CHANNELS;
    for (uint32_t ch = 0; ch < STREAM_AUDIO_CHANNELS; ++ch) {
        EXPECT_GT(rms(out, ch, 0, frames / 2), kToneRmsFloor) << "channel " << ch << " head";
        EXPECT_GT(rms(out, ch, frames / 2, frames), kToneRmsFloor) << "channel " << ch << " tail";
    }
}

// Mono must be duplicated across both sides. Reading channel 1 of a mono buffer
// gives either silence or a neighbouring sample; either way the right ear is
// wrong, and on some inputs it reads past the end.
TEST(StreamAudioHostPipeline, MonoIsUpmixedToBothChannels) {
    Harness h;
    const auto pcm = tone(kFrames, 1);
    for (int i = 0; i < kWarmupFrames; ++i) h.feed(pcm, kFrames, 1);
    ASSERT_GE(h.packets.size(), 2u);

    const auto out = h.decode_last();
    const size_t frames = out.size() / STREAM_AUDIO_CHANNELS;
    const double left = rms(out, 0, 0, frames);
    const double right = rms(out, 1, 0, frames);
    EXPECT_NEAR(left, kToneRms, 0.06);
    EXPECT_NEAR(right, kToneRms, 0.06) << "mono was not duplicated to the right channel";
    EXPECT_NEAR(left, right, 0.03);
    EXPECT_GT(rms(out, 1, frames / 2, frames), kToneRmsFloor) << "right channel tail";
}

// Surround sources keep the front pair, and the extra channels must not shorten
// the converted frame.
TEST(StreamAudioHostPipeline, SurroundKeepsFrontPairAndFullFrame) {
    Harness h;
    const auto pcm = tone(kFrames, 6);
    for (int i = 0; i < kWarmupFrames; ++i) h.feed(pcm, kFrames, 6);
    ASSERT_GE(h.packets.size(), 2u);

    const auto out = h.decode_last();
    const size_t frames = out.size() / STREAM_AUDIO_CHANNELS;
    for (uint32_t ch = 0; ch < STREAM_AUDIO_CHANNELS; ++ch) {
        EXPECT_GT(rms(out, ch, frames / 2, frames), kToneRmsFloor)
            << "channel " << ch << " tail — frame count scaled by channels";
    }
}

// There is no resampler in libmello. Encoding 44.1 kHz as if it were 48 kHz
// would ship audibly pitch-shifted audio, so the pipeline drops instead.
TEST(StreamAudioHostPipeline, RejectsUnexpectedSampleRate) {
    Harness h;
    const auto pcm = tone(kFrames, 2);
    for (int i = 0; i < kWarmupFrames; ++i) h.feed(pcm, kFrames, 2, 44100);
    EXPECT_TRUE(h.packets.empty());
}

// Partial buffers accumulate: SCK delivers whatever chunk size it likes, which
// is not a multiple of the 20 ms Opus frame.
TEST(StreamAudioHostPipeline, AccumulatesPartialBuffers) {
    Harness h;
    const uint32_t chunk = kFrames / 4;
    const auto pcm = tone(chunk, 2);
    for (int i = 0; i < 3; ++i) {
        h.feed(pcm, chunk, 2);
        EXPECT_TRUE(h.packets.empty()) << "emitted before a full 20 ms frame arrived";
    }
    h.feed(pcm, chunk, 2);
    EXPECT_EQ(h.packets.size(), 1u);
}

// Out-of-range floats must clamp. Without it the int16 cast wraps and a loud
// passage turns into full-scale noise.
TEST(StreamAudioHostPipeline, ClampsOutOfRangeSamples) {
    Harness h;
    const auto pcm = tone(kFrames, 2, 4.0f); // 8x over full scale
    for (int i = 0; i < kWarmupFrames; ++i) h.feed(pcm, kFrames, 2);
    ASSERT_GE(h.packets.size(), 2u);

    const auto out = h.decode_last();
    // Clamped, the 8x-overdriven sine becomes a near-square wave close to full
    // scale, so RMS lands well above the plain tone's 0.354.
    for (uint32_t ch = 0; ch < STREAM_AUDIO_CHANNELS; ++ch) {
        EXPECT_GT(rms(out, ch, 0, out.size() / STREAM_AUDIO_CHANNELS), 0.6)
            << "channel " << ch;
    }
}

// After stop() the ScreenCaptureKit queue can still be in flight; late samples
// must be dropped rather than reach a cleared callback.
TEST(StreamAudioHostPipeline, IgnoresSamplesAfterStop) {
    Harness h;
    h.pipeline.stop();
    const auto pcm = tone(kFrames, 2);
    h.feed(pcm, kFrames, 2);
    EXPECT_TRUE(h.packets.empty());
}

TEST(StreamAudioHostPipeline, IgnoresEmptyAndNullInput) {
    Harness h;
    const auto pcm = tone(kFrames, 2);
    h.pipeline.feed_float_pcm(nullptr, kFrames, 2, STREAM_AUDIO_SAMPLE_RATE);
    h.pipeline.feed_float_pcm(pcm.data(), 0, 2, STREAM_AUDIO_SAMPLE_RATE);
    h.pipeline.feed_float_pcm(pcm.data(), kFrames, 0, STREAM_AUDIO_SAMPLE_RATE);
    EXPECT_TRUE(h.packets.empty());
}

#endif // __APPLE__

// ─────────────────────────────────────────────────────────────────────────────
// Viewer playout (streaming-quality stage 7): the game audio is one stereo
// NetEQ in the voice mix, fed with each RTP packet. Device-free: a real
// AudioPipeline on the test backend, pulled 10 ms at a time through the
// render path as the device thread does, on the test clock.
//
// Before stage 7 the playout decoded each packet on arrival into a device
// of its own: nothing reached the voice mix, a lost packet was a click, a
// reordered one played out of order, and there was no clock to sync with.
// ─────────────────────────────────────────────────────────────────────────────

#include "audio/audio_pipeline.hpp"
#include "audio/opus_codec.hpp"
#include "audio/stream_audio_link.hpp"
#include "audio/stream_audio_pipeline.hpp"
#include "audio/voice_playout.hpp"
#include "transport/rtp_av_sync_extension.hpp"
#include "util/test_clock.hpp"

#include <opus.h>

#include <gtest/gtest.h>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iterator>
#include <memory>
#include <string>
#include <vector>

#ifdef _WIN32
#include <windows.h>
#endif

namespace {

using mello::audio::AudioPipeline;
using mello::audio::GameAudioStats;
using mello::audio::OpusApplication;
using mello::audio::OpusEnc;
using mello::audio::StreamAudioPlayout;
using mello::audio::VoicePlayout;

constexpr int64_t kClockOrigin = 2'000'000;
constexpr int kFrame = mello::audio::STREAM_AUDIO_FRAME_SAMPLES;  // 960, 20 ms
constexpr uint32_t kSsrc = 0x5EA11AB1;

void select_test_backend() {
#ifdef _WIN32
    SetEnvironmentVariableA("MELLO_AUDIO_BACKEND", "test");
#else
    setenv("MELLO_AUDIO_BACKEND", "test", 1);
#endif
}

/// 16-bit mono 48 kHz WAV from the voice gate corpus.
std::vector<int16_t> load_speech(const char* name) {
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

/// The host's game-audio encoder settings (StreamAudioHostPipeline).
struct GameEncoder {
    OpusEnc enc;
    explicit GameEncoder(int loss_perc = 5) {
        EXPECT_TRUE(enc.initialize(48000, 2, mello::audio::STREAM_AUDIO_BITRATE,
                                   OpusApplication::Audio));
        enc.set_inband_fec(1);
        enc.set_packet_loss_perc(loss_perc);
    }
    std::vector<uint8_t> encode(const std::vector<int16_t>& stereo_frame) {
        uint8_t buf[mello::audio::MAX_PACKET_SIZE];
        const int n = enc.encode(stereo_frame.data(), kFrame, buf, sizeof(buf));
        EXPECT_GT(n, 0);
        return std::vector<uint8_t>(buf, buf + std::max(n, 0));
    }
};

/// Frame `k` (20 ms) of a 400 Hz stereo tone.
std::vector<int16_t> tone_frame(int k, double amplitude = 12000.0) {
    std::vector<int16_t> pcm(static_cast<size_t>(kFrame) * 2);
    for (int i = 0; i < kFrame; ++i) {
        const double t = (static_cast<double>(k) * kFrame + i) / 48000.0;
        const auto v = static_cast<int16_t>(amplitude * std::sin(2.0 * 3.14159265358979 * 400.0 * t));
        pcm[static_cast<size_t>(i) * 2] = v;
        pcm[static_cast<size_t>(i) * 2 + 1] = v;
    }
    return pcm;
}

/// Frame `k` of a speech clip, the same on both channels.
std::vector<int16_t> speech_frame(const std::vector<int16_t>& mono, int k) {
    std::vector<int16_t> pcm(static_cast<size_t>(kFrame) * 2, 0);
    for (int i = 0; i < kFrame; ++i) {
        const size_t s = (static_cast<size_t>(k) * kFrame + i) % std::max<size_t>(mono.size(), 1);
        const int16_t v = mono.empty() ? 0 : mono[s];
        pcm[static_cast<size_t>(i) * 2] = v;
        pcm[static_cast<size_t>(i) * 2 + 1] = v;
    }
    return pcm;
}

/// An RTP packet as the SFU forwards the host's game audio.
std::vector<uint8_t> rtp_packet(uint16_t seq, uint32_t ts, const std::vector<uint8_t>& payload,
                                std::optional<uint32_t> video_ts = std::nullopt) {
    std::vector<std::byte> p(12);
    p[0] = std::byte{0x80};
    p[1] = std::byte{111};
    p[2] = std::byte{static_cast<uint8_t>(seq >> 8)};
    p[3] = std::byte{static_cast<uint8_t>(seq)};
    for (int i = 0; i < 4; ++i) {
        p[4 + i] = std::byte{static_cast<uint8_t>(ts >> (24 - 8 * i))};
        p[8 + i] = std::byte{static_cast<uint8_t>(kSsrc >> (24 - 8 * i))};
    }
    for (uint8_t b : payload) p.push_back(std::byte{b});
    if (video_ts) mello::transport::add_av_sync_extension(p, *video_ts);
    std::vector<uint8_t> out(p.size());
    std::memcpy(out.data(), p.data(), p.size());
    return out;
}

double rms(const int16_t* x, size_t n) {
    double sum = 0.0;
    for (size_t i = 0; i < n; ++i) sum += static_cast<double>(x[i]) * x[i];
    return n ? std::sqrt(sum / static_cast<double>(n)) : 0.0;
}

/// A real AudioPipeline on the test backend and one stream view's playout
/// in its voice mix. step() advances the test clock by 20 ms: one packet in
/// (when given), two 10 ms pulls out, as the device thread pulls.
class StreamPlayoutTest : public ::testing::Test {
protected:
    std::unique_ptr<AudioPipeline> pipeline;
    std::unique_ptr<StreamAudioPlayout> playout;
    int64_t now_ms = kClockOrigin;
    std::vector<int16_t> out;  // everything pulled, mono downmix

    void SetUp() override {
        select_test_backend();
        mello::util::set_test_clock_ms(now_ms);
        pipeline = std::make_unique<AudioPipeline>();
        ASSERT_TRUE(pipeline->initialize());
        AudioPipeline* p = pipeline.get();
        playout = std::make_unique<StreamAudioPlayout>(
            [p] { return p->playout().add_game_audio(); },
            [p] { return p->output_latency_ms(); });
    }

    void TearDown() override {
        playout.reset();
        pipeline->shutdown();
        mello::util::set_test_clock_ms(-1);
    }

    void pull10() {
        int16_t block[480];
        ASSERT_GE(pipeline->render_test_output(block, 480), 0);
        out.insert(out.end(), block, block + 480);
    }

    void step(const std::vector<uint8_t>* packet = nullptr) {
        if (packet) playout->feed_rtp(packet->data(), packet->size(), now_ms * 1000);
        for (int i = 0; i < 2; ++i) {
            now_ms += 10;
            mello::util::set_test_clock_ms(now_ms);
            pull10();
        }
    }

    /// RMS of the 10 ms blocks of `out` from block `from` on, each.
    std::vector<double> block_rms(size_t from) const {
        std::vector<double> r;
        for (size_t b = from; (b + 1) * 480 <= out.size(); ++b) r.push_back(rms(out.data() + b * 480, 480));
        return r;
    }
};

} // namespace

// The game audio reaches the voice output: the render path that the device
// thread pulls. The voice statistics do not count it.
TEST_F(StreamPlayoutTest, GameAudioPlaysInTheVoiceMix) {
    GameEncoder enc;
    for (int k = 0; k < 60; ++k) {
        const auto pkt = rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame),
                                    enc.encode(tone_frame(k)));
        step(&pkt);
    }
    const auto r = block_rms(40);  // after NetEQ's start
    ASSERT_FALSE(r.empty());
    for (size_t i = 0; i < r.size(); ++i) EXPECT_GT(r[i], 4000.0) << "10 ms block " << i;
    EXPECT_EQ(pipeline->playout().peer_count(), 0) << "the game audio is not a voice peer";
    const auto s = playout->stats();
    EXPECT_EQ(s.packets_fed, 60u);
    EXPECT_TRUE(s.source.playing);
}

// A paused stream goes silent at the next 10 ms; packets that arrive while
// muted are dropped; the first new packet after unmute plays.
TEST_F(StreamPlayoutTest, MuteIsSilentAndUnmuteResumes) {
    GameEncoder enc;
    int k = 0;
    for (; k < 40; ++k) {
        const auto pkt = rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame),
                                    enc.encode(tone_frame(k)));
        step(&pkt);
    }
    ASSERT_GT(rms(out.data() + out.size() - 480, 480), 4000.0) << "the tone plays before the mute";

    playout->set_muted(true);
    EXPECT_TRUE(playout->muted());
    const size_t muted_from = out.size();
    for (int i = 0; i < 20; ++i, ++k) {
        const auto pkt = rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame),
                                    enc.encode(tone_frame(k)));
        step(&pkt);
    }
    EXPECT_EQ(rms(out.data() + muted_from, out.size() - muted_from), 0.0)
        << "audio played while muted (the queued audio was not flushed)";
    EXPECT_EQ(playout->packets_dropped_muted(), 20u);

    playout->set_muted(false);
    const size_t unmuted_from = out.size();
    for (int i = 0; i < 30; ++i, ++k) {
        const auto pkt = rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame),
                                    enc.encode(tone_frame(k)));
        step(&pkt);
    }
    EXPECT_GT(rms(out.data() + out.size() - 480, 480), 4000.0) << "unmuted audio is silent";
    EXPECT_GT(rms(out.data() + unmuted_from, out.size() - unmuted_from), 2000.0);
}

// The game audio plays on the voice output device and follows a device
// change at once (stage 2a applied a change only at the next unmute).
TEST_F(StreamPlayoutTest, AnOutputDeviceChangeKeepsTheGameAudio) {
    GameEncoder enc;
    int k = 0;
    for (; k < 30; ++k) {
        const auto pkt = rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame),
                                    enc.encode(tone_frame(k)));
        step(&pkt);
    }
    ASSERT_GT(pipeline->set_playback_device("{0.0.0.00000000}.{headset}"), 0);
    EXPECT_EQ(pipeline->playback_device_id(), "{0.0.0.00000000}.{headset}");
    for (int i = 0; i < 30; ++i, ++k) {
        const auto pkt = rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame),
                                    enc.encode(tone_frame(k)));
        step(&pkt);
    }
    EXPECT_GT(rms(out.data() + out.size() - 4800, 4800), 4000.0)
        << "no game audio on the new output device";
}

// Volume scales the game audio alone; a voice peer keeps its level.
TEST_F(StreamPlayoutTest, TheVolumeScalesTheGameAudio) {
    GameEncoder enc;
    playout->set_volume(0.25f);
    EXPECT_FLOAT_EQ(playout->volume(), 0.25f);
    for (int k = 0; k < 60; ++k) {
        const auto pkt = rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame),
                                    enc.encode(tone_frame(k)));
        step(&pkt);
    }
    // The tone is about 8485 RMS at unity: a quarter is about 2120.
    const double level = rms(out.data() + out.size() - 4800, 4800);
    EXPECT_GT(level, 1500.0);
    EXPECT_LT(level, 2800.0);
    playout->set_muted(true);
    playout->set_muted(false);
    EXPECT_FLOAT_EQ(playout->volume(), 0.25f) << "the volume is kept across mute";
}

// Reordered packets play in order: NetEQ conceals nothing for them. Before
// stage 7 the playout decoded each packet as it arrived.
TEST_F(StreamPlayoutTest, ReorderedPacketsAreNotConcealed) {
    GameEncoder enc;
    std::vector<std::vector<uint8_t>> packets;
    for (int k = 0; k < 120; ++k) {
        packets.push_back(rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame),
                                     enc.encode(tone_frame(k))));
    }
    // Pairs swap: 1, 0, 3, 2, ... after the first 20.
    for (int k = 0; k < 120; ++k) {
        int i = k;
        if (k >= 20) i = (k % 2 == 0) ? k + 1 : k - 1;
        step(&packets[static_cast<size_t>(std::min(i, 119))]);
    }
    const auto s = playout->stats();
    EXPECT_LT(s.expand_ms, 60.0) << "reordered packets were concealed";
    const auto r = block_rms(60);
    for (size_t i = 0; i < r.size(); ++i) EXPECT_GT(r[i], 4000.0) << "gap at block " << i;
}

// A lost packet with in-band FEC in the next one is repaired from it.
TEST_F(StreamPlayoutTest, ALostPacketIsRepairedFromInBandFec) {
    // The loss the host's encoder expects after its receiver reports said
    // 15 %: over Opus's switch to the FEC-carrying mode.
    GameEncoder enc(15);
    const auto speech = load_speech("f1_farris.wav");
    ASSERT_FALSE(speech.empty());
    int with_lbrr = 0;
    for (int k = 0; k < 150; ++k) {
        const auto payload = enc.encode(speech_frame(speech, k));
        with_lbrr += opus_packet_has_lbrr(payload.data(), static_cast<opus_int32>(payload.size())) > 0;
        const auto pkt = rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame), payload);
        // Every 10th packet after the start is lost.
        if (k > 30 && k % 10 == 0) {
            step(nullptr);
        } else {
            step(&pkt);
        }
    }
    // Speech carries FEC; its pauses do not need it.
    ASSERT_GT(with_lbrr, 50) << "the encoder sent no in-band FEC";
    const auto s = playout->stats();
    std::printf("in-band FEC: %d of 150 packets carried it, %llu losses repaired from it\n",
                with_lbrr, static_cast<unsigned long long>(s.fec_used));
    EXPECT_GE(s.fec_used, 5u) << "NetEQ did not repair the losses from FEC";
}

// The sync sample maps the audio heard now onto the video clock through
// the extension of the newest packet, and says when it is heard.
TEST_F(StreamPlayoutTest, TheSyncSampleMapsTheHeardAudioToTheVideoClock) {
    GameEncoder enc;
    const uint32_t audio_base = 123'456;
    const uint32_t video_base = 900'000;
    for (int k = 0; k < 50; ++k) {
        const uint32_t ts = audio_base + static_cast<uint32_t>(k * kFrame);
        // The video clock at the audio's capture: 20 ms = 1800 ticks.
        const auto pkt = rtp_packet(static_cast<uint16_t>(k), ts, enc.encode(tone_frame(k)),
                                    video_base + static_cast<uint32_t>(k * 1800));
        step(&pkt);
    }
    const auto sample = playout->sync_sample();
    ASSERT_TRUE(sample.has_value());
    // The heard audio's video timestamp, back to the audio clock.
    const int64_t video_delta = static_cast<int32_t>(sample->video_rtp_timestamp - video_base);
    const int64_t audio_delta = video_delta * 48 / 90;
    EXPECT_GE(audio_delta, 0);
    EXPECT_LE(audio_delta, 50 * kFrame);
    // The last pull ran at now_ms; its 10 ms are heard until 10 ms later
    // (no device latency on the test backend).
    EXPECT_EQ(sample->heard_at_us, now_ms * 1000 + 10'000);
    EXPECT_EQ(playout->stats().sync_extensions, 50u);
}

// The pending delay is what NetEQ still adds to the audio. A buffer over the
// target is heard as it is: NetEQ keeps a delay from its target up to the
// target plus the largest arrival delay plus 20 ms, so no pending delay. A
// raised minimum is still to come (NetEQ stretches the audio up to it).
TEST_F(StreamPlayoutTest, ThePendingDelayIsOnlyWhatNetEqStillAdds) {
    GameEncoder enc;
    const uint32_t audio_base = 123'456;
    const uint32_t video_base = 900'000;
    int k = 0;
    auto next = [&] {
        const uint32_t ts = audio_base + static_cast<uint32_t>(k * kFrame);
        auto pkt = rtp_packet(static_cast<uint16_t>(k), ts, enc.encode(tone_frame(k)),
                              video_base + static_cast<uint32_t>(k * 1800));
        ++k;
        return pkt;
    };
    for (int i = 0; i < 100; ++i) {
        const auto pkt = next();
        step(&pkt);
    }
    // Two packets early: the buffer holds 40 ms more than the target.
    for (int i = 0; i < 2; ++i) {
        const auto pkt = next();
        playout->feed_rtp(pkt.data(), pkt.size(), now_ms * 1000);
    }
    for (int i = 0; i < 25; ++i) {
        const auto pkt = next();
        step(&pkt);
    }
    const GameAudioStats over = playout->stats().source;
    ASSERT_GT(over.filtered_delay_ms, over.target_delay_ms) << "the buffer is not over the target";
    auto sample = playout->sync_sample();
    ASSERT_TRUE(sample.has_value());
    EXPECT_EQ(sample->pending_ms, 0.0) << "a buffer over the target is no pending delay";

    // A raised minimum: the distance to it is still to come.
    playout->set_sync_delay_ms(200);
    {
        const auto pkt = next();
        step(&pkt);
    }
    const GameAudioStats raised = playout->stats().source;
    sample = playout->sync_sample();
    ASSERT_TRUE(sample.has_value());
    ASSERT_GT(raised.target_delay_ms, raised.filtered_delay_ms);
    EXPECT_NEAR(sample->pending_ms, raised.target_delay_ms - raised.filtered_delay_ms, 1.0);
    EXPECT_GT(sample->pending_ms, 100.0);
}

// Ten minutes of clock drift: the host's audio device runs 300 ppm fast
// against its capture clock, and the viewer's device pulls on its own
// clock. NetEQ keeps its delay (time stretch), and the sync mapping, which
// takes the capture time from each packet, does not drift: after 10
// simulated minutes the A/V offset it measures moved by under 10 ms.
TEST(StreamAudioSyncDrift, TenMinutesOfDriftMoveTheOffsetByUnder10Ms) {
    mello::util::set_test_clock_ms(kClockOrigin);
    VoicePlayout::Config cfg;
    cfg.delay_manager_config = VoicePlayout::default_delay_manager_config();
    VoicePlayout mix(cfg);
    StreamAudioPlayout playout([&mix] { return mix.add_game_audio(); });
    GameEncoder enc;
    // Encode one second of tone once and loop it: the content does not
    // matter for the clocks, and 30000 encodes would dominate the run.
    std::vector<std::vector<uint8_t>> payloads;
    for (int k = 0; k < 50; ++k) payloads.push_back(enc.encode(tone_frame(k)));

    static constexpr double kHostPpm = 300.0;
    constexpr int64_t kRunMs = 10 * 60 * 1000;
    // Frame k leaves the host when its 960 samples are captured: at
    // k * 20 ms * (1 - ppm) on the host clock (a fast device). Its capture
    // time on the video clock is the same instant, at 90 kHz.
    auto send_ms = [](int64_t k) {
        return static_cast<double>(k) * 20.0 * (1.0 - kHostPpm * 1e-6);
    };
    const int64_t network_ms = 30;
    std::vector<double> offsets;  // heard time minus capture time, ms
    int64_t k = 0;
    int16_t block[VoicePlayout::kFrameSamples * VoicePlayout::kChannels];
    for (int64_t t = 0; t <= kRunMs; ++t) {
        mello::util::set_test_clock_ms(kClockOrigin + t);
        while (send_ms(k) + network_ms <= static_cast<double>(t)) {
            const uint32_t video_ts =
                static_cast<uint32_t>(static_cast<int64_t>(send_ms(k) * 90.0 + 0.5));
            const auto pkt = rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame),
                                        payloads[static_cast<size_t>(k % 50)], video_ts);
            playout.feed_rtp(pkt.data(), pkt.size(), (kClockOrigin + t) * 1000);
            ++k;
        }
        if (t % 10 == 0) {
            mix.mix(block);
            if (t % 1000 == 0) {
                if (const auto s = playout.sync_sample()) {
                    // The capture time of the heard audio on the host clock,
                    // ms, against when it is heard (both from 0).
                    const double captured_ms = static_cast<double>(s->video_rtp_timestamp) / 90.0;
                    const double heard_ms =
                        static_cast<double>(s->heard_at_us) / 1000.0 - static_cast<double>(kClockOrigin);
                    offsets.push_back(heard_ms - captured_ms);
                }
            }
        }
    }
    mello::util::set_test_clock_ms(-1);
    ASSERT_GT(offsets.size(), 500u);
    // After NetEQ settles (5 s), the end-to-end delay of the audio stays
    // within 10 ms of where it was: no drift accumulates over 10 minutes.
    const double first = offsets[5];
    double worst = 0.0;
    for (size_t i = 5; i < offsets.size(); ++i) worst = std::max(worst, std::abs(offsets[i] - first));
    std::printf("10 min drift: end-to-end %.1f ms at 5 s, %.1f ms at 600 s, worst change %.1f ms\n",
                first, offsets.back(), worst);
    EXPECT_LT(worst, 10.0) << "first " << first << " ms, last " << offsets.back() << " ms";
    // Without NetEQ's drift handling the buffer would grow 300 ppm x 600 s
    // = 180 ms; the stretch went somewhere.
    const auto st = playout.stats();
    EXPECT_GT(st.source.removed_samples_for_acceleration, 48u * 100u)
        << "NetEQ did not accelerate against the fast host clock";
}

// The host encoder carries in-band FEC once the receiver reports say the
// link loses packets (game_loss_perc: the loss over 4 s, floor 10 %). Before
// stage 7 FEC was off on the game audio.
TEST(StreamAudioHostFec, ReportedLossTurnsOnInBandFec) {
    auto link = std::make_shared<mello::audio::StreamAudioLink>();
    mello::audio::StreamAudioHostPipeline host;
    host.set_link(link);
    std::vector<std::vector<uint8_t>> packets;
    ASSERT_TRUE(host.start(
        [&packets](const uint8_t* d, int size, uint64_t) {
            if (size > 0) packets.emplace_back(d, d + size);
        },
        0, /*external_source=*/true));
    // 15 % loss for 5 s of reports, one per second.
    for (int i = 0; i < 5; ++i) link->on_send_feedback(1000 * (i + 1), 50, 8, 40.0f, 5.0f);
    EXPECT_GE(link->loss_perc(), 15);

    const auto speech = load_speech("m1_davison.wav");
    ASSERT_FALSE(speech.empty());
    std::vector<float> pcm(static_cast<size_t>(kFrame) * 2);
    for (int k = 0; k < 100; ++k) {
        for (int i = 0; i < kFrame; ++i) {
            const float v = static_cast<float>(speech[(static_cast<size_t>(k) * kFrame + i) % speech.size()]) / 32768.0f;
            pcm[static_cast<size_t>(i) * 2] = v;
            pcm[static_cast<size_t>(i) * 2 + 1] = v;
        }
        host.feed_float_pcm_at(pcm.data(), kFrame, 2, 48000, 1'000'000 + k * 20'000);
    }
    host.stop();
    EXPECT_GE(host.loss_perc(), 15);
    ASSERT_GE(packets.size(), 95u);
    int with_lbrr = 0;
    for (const auto& p : packets) {
        with_lbrr += opus_packet_has_lbrr(p.data(), static_cast<opus_int32>(p.size())) > 0;
    }
    EXPECT_GT(with_lbrr, 80) << "the game-audio encoder sent no in-band FEC";
    // Each frame's capture time is in the link for the peer.
    EXPECT_EQ(link->capture_us(99 * 960), 1'000'000 + 99 * 20'000);
}

namespace {

/// Feed 2 s of speech to `host`; return how many packets carry LBRR.
int speech_packets_with_lbrr(mello::audio::StreamAudioHostPipeline& host,
                             std::vector<std::vector<uint8_t>>& packets, int64_t from_us) {
    const auto speech = load_speech("m1_davison.wav");
    EXPECT_FALSE(speech.empty());
    if (speech.empty()) return -1;
    packets.clear();
    std::vector<float> pcm(static_cast<size_t>(kFrame) * 2);
    for (int k = 0; k < 100; ++k) {
        for (int i = 0; i < kFrame; ++i) {
            const float v =
                static_cast<float>(speech[(static_cast<size_t>(k) * kFrame + i) % speech.size()]) / 32768.0f;
            pcm[static_cast<size_t>(i) * 2] = v;
            pcm[static_cast<size_t>(i) * 2 + 1] = v;
        }
        host.feed_float_pcm_at(pcm.data(), kFrame, 2, 48000, from_us + k * 20'000);
    }
    int with_lbrr = 0;
    for (const auto& p : packets) {
        with_lbrr += opus_packet_has_lbrr(p.data(), static_cast<opus_int32>(p.size())) > 0;
    }
    return with_lbrr;
}

} // namespace

// The game audio's own loss rule. A clean link spends no bits on FEC: the
// expected loss is 0. Any reported loss sets at least 10 %, because Opus in
// music mode sends no LBRR at the voice floor of 5 % (it needs more than
// about 8 %).
TEST(StreamAudioHostFec, AnyReportedLossSetsTheGameFloorAndACleanLinkPaysNothing) {
    EXPECT_EQ(mello::audio::game_loss_perc(-1), 0);
    EXPECT_EQ(mello::audio::game_loss_perc(0), 0);
    EXPECT_EQ(mello::audio::game_loss_perc(1), 10);
    EXPECT_EQ(mello::audio::game_loss_perc(20), 20);
    EXPECT_EQ(mello::audio::game_loss_perc(80), 30);

    auto link = std::make_shared<mello::audio::StreamAudioLink>();
    mello::audio::StreamAudioHostPipeline host;
    host.set_link(link);
    std::vector<std::vector<uint8_t>> packets;
    ASSERT_TRUE(host.start(
        [&packets](const uint8_t* d, int size, uint64_t) {
            if (size > 0) packets.emplace_back(d, d + size);
        },
        0, /*external_source=*/true));
    EXPECT_EQ(link->loss_perc(), 0) << "no reports yet";

    // A clean link: 5 s of reports without loss.
    for (int i = 0; i < 5; ++i) link->on_send_feedback(1000 * (i + 1), 50, 0, 40.0f, 5.0f);
    EXPECT_EQ(link->loss_perc(), 0);
    EXPECT_EQ(speech_packets_with_lbrr(host, packets, 1'000'000), 0) << "FEC on a clean link";
    EXPECT_EQ(host.loss_perc(), 0);

    // 1 packet in 50 lost (2 %): the floor, and the encoder sends LBRR.
    for (int i = 5; i < 10; ++i) link->on_send_feedback(1000 * (i + 1), 50, 1, 40.0f, 5.0f);
    EXPECT_EQ(link->loss_perc(), 10);
    const int with_lbrr = speech_packets_with_lbrr(host, packets, 3'000'000);
    EXPECT_EQ(host.loss_perc(), 10);
    ASSERT_GE(packets.size(), 95u);
    EXPECT_GT(with_lbrr, 70) << "the 10 % floor sent no in-band FEC";
    host.stop();
}

// ─────────────────────────────────────────────────────────────────────────────
// The same through the C API, as mello-core uses it: a stream view's game
// audio comes out of the context's voice output (the render path the device
// thread pulls). This test builds on the code before stage 7 too, and fails
// there: the old playout decoded into a playback device of its own, so the
// voice output stayed silent.
// ─────────────────────────────────────────────────────────────────────────────

#include "mello.h"

TEST(StreamAudioCApi, AFedPacketPlaysInTheVoiceOutput) {
    select_test_backend();
    MelloContext* ctx = mello_init();
    ASSERT_NE(ctx, nullptr);
    MelloStreamConfig config{};
    config.width = 640;
    config.height = 360;
    config.fps = 60;
    MelloStreamView* view = mello_stream_start_viewer(ctx, &config, nullptr, nullptr);
    if (view == nullptr) {
        mello_destroy(ctx);
        GTEST_SKIP() << "no viewer pipeline on this machine";
    }
    GameEncoder enc;
    std::vector<int16_t> out;
    int16_t block[480];
    for (int k = 0; k < 60; ++k) {
        const auto payload = enc.encode(tone_frame(k));
        ASSERT_EQ(mello_stream_feed_audio_packet(view, payload.data(), static_cast<int>(payload.size())),
                  MELLO_OK);
        for (int i = 0; i < 2; ++i) {
            ASSERT_GE(mello_voice_test_pull_output(ctx, block, 480), 0);
            out.insert(out.end(), block, block + 480);
        }
    }
    mello_stream_stop_viewer(view);
    mello_destroy(ctx);
    // The last 400 ms: the tone, through NetEQ and the mixer.
    ASSERT_GE(out.size(), 19200u);
    EXPECT_GT(rms(out.data() + out.size() - 19200, 19200), 4000.0)
        << "the game audio did not reach the voice output";
}

// The start buffer: the first packets wait until the A/V sync knows when
// their video is shown; then the playout keeps that much audio and starts
// with it as NetEQ's minimum delay, so the audio starts in sync.
TEST_F(StreamPlayoutTest, TheStartBufferHoldsTheFirstPacketsUntilTheVideoIsKnown) {
    GameEncoder enc;
    int asked = 0;
    playout->set_start_buffer([&asked](uint32_t, int64_t) -> std::optional<int> {
        // No video frame for the first five packets.
        return ++asked <= 5 ? std::nullopt : std::optional<int>(80);
    });
    for (int k = 0; k < 6; ++k) {
        const auto pkt = rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame),
                                    enc.encode(tone_frame(k)), 1800u * static_cast<uint32_t>(k));
        step(&pkt);
    }
    // Five steps of 20 ms while held: silence.
    EXPECT_EQ(rms(out.data(), 5 * 960), 0.0) << "audio played before the start buffer was known";
    auto s = playout->stats();
    EXPECT_EQ(s.start_buffers, 1u);
    EXPECT_EQ(s.last_start_buffer_ms, 80);
    EXPECT_EQ(s.source.minimum_delay_ms, 80);
    // Five held packets (100 ms) and the sixth: 80 ms of them stay, and the
    // audio plays from the next pull on.
    for (int k = 6; k < 40; ++k) {
        const auto pkt = rtp_packet(static_cast<uint16_t>(k), static_cast<uint32_t>(k * kFrame),
                                    enc.encode(tone_frame(k)), 1800u * static_cast<uint32_t>(k));
        step(&pkt);
    }
    EXPECT_GT(rms(out.data() + out.size() - 4800, 4800), 4000.0);
    EXPECT_EQ(asked, 6) << "the start buffer is asked only while the playout starts";
    // A pause starts it again.
    playout->set_muted(true);
    playout->set_muted(false);
    const auto pkt = rtp_packet(40, 40u * kFrame, enc.encode(tone_frame(40)), 72000u);
    step(&pkt);
    EXPECT_EQ(playout->stats().start_buffers, 2u);
}
