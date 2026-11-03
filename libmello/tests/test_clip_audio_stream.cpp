// The sound and the clock of a game clip that plays (CLIPS.md
// 15.9): the demux thread writes 48 kHz stereo PCM, the render thread mixes
// it into the speaker output, and the frames it played are the clock that
// the picture follows.

#include <gtest/gtest.h>

#include "audio/clip_audio_stream.hpp"

#include <array>
#include <vector>

using mello::audio::ClipAudioStream;

namespace {

std::vector<int16_t> stereo_ramp(size_t frames, int16_t start) {
    std::vector<int16_t> pcm(frames * 2);
    for (size_t i = 0; i < frames; ++i) {
        pcm[2 * i] = static_cast<int16_t>(start + i);
        pcm[2 * i + 1] = static_cast<int16_t>(-(start + static_cast<int16_t>(i)));
    }
    return pcm;
}

struct Frame10ms {
    std::array<int16_t, 480 * 2> stereo{};
    std::array<int16_t, 480> mono{};
};

}  // namespace

TEST(ClipAudioStream, PcmIsMixedInOrderAndTheClockCountsIt) {
    ClipAudioStream stream(2000);
    const auto pcm = stereo_ramp(960, 100);
    ASSERT_EQ(stream.write(pcm.data(), 960), 960u);

    Frame10ms f;
    f.stereo.fill(1);
    ASSERT_TRUE(stream.mix_into(f.stereo.data(), f.mono.data(), 480));
    EXPECT_EQ(f.stereo[0], 1 + 100) << "added to what the voice mix has";
    EXPECT_EQ(f.stereo[1], 1 - 100);
    EXPECT_EQ(f.mono[0], 0) << "the mono mix of L and R";
    EXPECT_EQ(stream.played_frames(), 480u);

    Frame10ms g;
    ASSERT_TRUE(stream.mix_into(g.stereo.data(), g.mono.data(), 480));
    EXPECT_EQ(g.stereo[0], 100 + 480) << "the next 10 ms, in order";
    EXPECT_EQ(stream.played_frames(), 960u);
}

TEST(ClipAudioStream, TheClockStopsWhilePausedAndWhileNothingWaits) {
    ClipAudioStream stream(2000);
    const auto pcm = stereo_ramp(480, 1);
    stream.write(pcm.data(), 480);
    stream.set_paused(true);
    Frame10ms f;
    EXPECT_FALSE(stream.mix_into(f.stereo.data(), f.mono.data(), 480));
    EXPECT_EQ(stream.played_frames(), 0u) << "paused";
    EXPECT_EQ(f.stereo[0], 0) << "nothing played";

    stream.set_paused(false);
    EXPECT_TRUE(stream.mix_into(f.stereo.data(), f.mono.data(), 480));
    EXPECT_EQ(stream.played_frames(), 480u);
    // Buffering: the fetch fell behind and no PCM waits.
    EXPECT_FALSE(stream.mix_into(f.stereo.data(), f.mono.data(), 480));
    EXPECT_EQ(stream.played_frames(), 480u);
}

TEST(ClipAudioStream, TheRingWrapsAndHoldsNoMoreThanItsCapacity) {
    ClipAudioStream stream(1000);
    const auto pcm = stereo_ramp(800, 0);
    ASSERT_EQ(stream.write(pcm.data(), 800), 800u);
    EXPECT_EQ(stream.write(pcm.data(), 800), 200u) << "full at 1000 frames";
    Frame10ms f;
    stream.mix_into(f.stereo.data(), f.mono.data(), 480);
    stream.mix_into(f.stereo.data(), f.mono.data(), 480);
    // 960 read: 40 frames wait (160 to 199 of the second write), and new PCM
    // fits again, across the end of the ring.
    const auto more = stereo_ramp(500, 7000);
    ASSERT_EQ(stream.write(more.data(), 500), 500u);
    Frame10ms g;
    stream.mix_into(g.stereo.data(), g.mono.data(), 480);
    EXPECT_EQ(g.stereo[0], 160) << "the 40 frames that waited come first";
    EXPECT_EQ(g.stereo[2 * 40], 7000) << "then the new PCM, read across the ring end";
}

TEST(ClipAudioStream, ASeekDropsTheWaitingPcmAndMovesTheClock) {
    ClipAudioStream stream(2000);
    const auto old_pcm = stereo_ramp(960, 1);
    stream.write(old_pcm.data(), 960);
    stream.reset(48000 * 10);  // to 10 s
    EXPECT_EQ(stream.played_frames(), 480000u) << "the clock moves at once";
    const auto new_pcm = stereo_ramp(480, 5000);
    stream.write(new_pcm.data(), 480);

    Frame10ms f;
    ASSERT_TRUE(stream.mix_into(f.stereo.data(), f.mono.data(), 480));
    EXPECT_EQ(f.stereo[0], 5000) << "the PCM from before the seek is gone";
    EXPECT_EQ(stream.played_frames(), 480000u + 480u);
}
