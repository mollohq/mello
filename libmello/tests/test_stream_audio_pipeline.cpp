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
