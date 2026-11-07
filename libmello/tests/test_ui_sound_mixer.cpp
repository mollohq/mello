// Notification sounds (specs/features/NOTIFICATION-SOUNDS.md): the mixer that the
// render thread runs, and the sound in the pipeline's speaker output.

#include <gtest/gtest.h>

#include "audio/audio_pipeline.hpp"
#include "audio/ui_sound_mixer.hpp"

#include <array>
#include <cstdlib>
#include <vector>

#ifdef _WIN32
#include <windows.h>
#endif

using mello::audio::AudioPipeline;
using mello::audio::UiSoundMixer;

namespace {

constexpr size_t k10ms = 480;

std::vector<int16_t> constant_stereo(size_t frames, int16_t left, int16_t right) {
    std::vector<int16_t> pcm(frames * 2);
    for (size_t i = 0; i < frames; ++i) {
        pcm[2 * i] = left;
        pcm[2 * i + 1] = right;
    }
    return pcm;
}

struct Frame10ms {
    std::array<int16_t, k10ms * 2> stereo{};
    std::array<int16_t, k10ms> mono{};
};

}  // namespace

TEST(UiSoundMixer, PlaysTheSoundOnceAtItsGain) {
    UiSoundMixer mixer;
    const auto pcm = constant_stereo(k10ms + 100, 1000, -400);
    ASSERT_TRUE(mixer.load(3, pcm.data(), k10ms + 100));
    ASSERT_TRUE(mixer.play(3, 0.5f));

    Frame10ms f;
    ASSERT_TRUE(mixer.mix_into(f.stereo.data(), f.mono.data(), k10ms));
    EXPECT_EQ(f.stereo[0], 500);
    EXPECT_EQ(f.stereo[1], -200);
    EXPECT_EQ(f.mono[0], 150);  // (500 - 200) / 2
    EXPECT_EQ(mixer.active_voices(), 1u);

    // The last 100 frames, then silence.
    Frame10ms g;
    ASSERT_TRUE(mixer.mix_into(g.stereo.data(), g.mono.data(), k10ms));
    EXPECT_EQ(g.stereo[2 * 99], 500);
    EXPECT_EQ(g.stereo[2 * 100], 0);
    EXPECT_EQ(mixer.active_voices(), 0u);

    Frame10ms h;
    EXPECT_FALSE(mixer.mix_into(h.stereo.data(), h.mono.data(), k10ms));
    EXPECT_EQ(h.stereo[0], 0);
}

TEST(UiSoundMixer, AddsToWhatPlaysAndSaturates) {
    UiSoundMixer mixer;
    const auto pcm = constant_stereo(k10ms, 30000, -30000);
    ASSERT_TRUE(mixer.load(0, pcm.data(), k10ms));
    ASSERT_TRUE(mixer.play(0, 1.0f));

    Frame10ms f;
    f.stereo.fill(10000);
    f.mono.fill(100);
    ASSERT_TRUE(mixer.mix_into(f.stereo.data(), f.mono.data(), k10ms));
    EXPECT_EQ(f.stereo[0], 32767);
    EXPECT_EQ(f.stereo[1], -20000);
    EXPECT_EQ(f.mono[0], 100);  // the sound's mono mix is 0
}

TEST(UiSoundMixer, TwoSoundsPlayTogether) {
    UiSoundMixer mixer;
    const auto a = constant_stereo(k10ms, 100, 100);
    const auto b = constant_stereo(k10ms, 20, 20);
    ASSERT_TRUE(mixer.load(1, a.data(), k10ms));
    ASSERT_TRUE(mixer.load(2, b.data(), k10ms));
    ASSERT_TRUE(mixer.play(1, 1.0f));
    ASSERT_TRUE(mixer.play(2, 1.0f));

    Frame10ms f;
    ASSERT_TRUE(mixer.mix_into(f.stereo.data(), f.mono.data(), k10ms));
    EXPECT_EQ(f.stereo[0], 120);
}

TEST(UiSoundMixer, AFifthSoundReplacesTheFirst) {
    UiSoundMixer mixer;
    // Slot i plays the value 1 << i, so the sum names the sounds that play.
    for (uint32_t i = 0; i < 5; ++i) {
        const auto pcm = constant_stereo(k10ms * 4, static_cast<int16_t>(1 << i), 0);
        ASSERT_TRUE(mixer.load(i, pcm.data(), k10ms * 4));
    }
    for (uint32_t i = 0; i < 4; ++i) ASSERT_TRUE(mixer.play(i, 1.0f));
    Frame10ms f;
    ASSERT_TRUE(mixer.mix_into(f.stereo.data(), f.mono.data(), k10ms));
    EXPECT_EQ(f.stereo[0], 1 + 2 + 4 + 8);

    ASSERT_TRUE(mixer.play(4, 1.0f));
    Frame10ms g;
    ASSERT_TRUE(mixer.mix_into(g.stereo.data(), g.mono.data(), k10ms));
    EXPECT_EQ(g.stereo[0], 2 + 4 + 8 + 16);
    EXPECT_EQ(mixer.active_voices(), UiSoundMixer::kMaxVoices);
}

TEST(UiSoundMixer, RejectsBadInput) {
    UiSoundMixer mixer;
    const auto pcm = constant_stereo(10, 1, 1);
    EXPECT_FALSE(mixer.load(UiSoundMixer::kMaxSounds, pcm.data(), 10));
    EXPECT_FALSE(mixer.load(0, nullptr, 10));
    EXPECT_FALSE(mixer.load(0, pcm.data(), 0));
    EXPECT_FALSE(mixer.play(0, 1.0f));  // not loaded
    EXPECT_FALSE(mixer.play(UiSoundMixer::kMaxSounds, 1.0f));

    ASSERT_TRUE(mixer.load(0, pcm.data(), 10));
    for (size_t i = 0; i < UiSoundMixer::kQueueSize; ++i) ASSERT_TRUE(mixer.play(0, 1.0f));
    EXPECT_FALSE(mixer.play(0, 1.0f));  // the queue is full until a mix takes it

    Frame10ms f;
    mixer.mix_into(f.stereo.data(), f.mono.data(), k10ms);
    EXPECT_TRUE(mixer.play(0, 1.0f));
}

TEST(UiSoundMixer, GainIsClamped) {
    UiSoundMixer mixer;
    const auto pcm = constant_stereo(k10ms, 1000, 1000);
    ASSERT_TRUE(mixer.load(0, pcm.data(), k10ms));
    ASSERT_TRUE(mixer.play(0, 5.0f));
    Frame10ms f;
    ASSERT_TRUE(mixer.mix_into(f.stereo.data(), f.mono.data(), k10ms));
    EXPECT_EQ(f.stereo[0], 1000);
}

// The pipeline plays a sound outside a call and while deafened, through the
// device-free test backend.
TEST(UiSoundPipeline, PlaysWhileDeafenedOutsideACall) {
#ifdef _WIN32
    SetEnvironmentVariableA("MELLO_AUDIO_BACKEND", "test");
#else
    setenv("MELLO_AUDIO_BACKEND", "test", 1);
#endif
    AudioPipeline pipeline;
    ASSERT_TRUE(pipeline.initialize());
    pipeline.set_deafen(true);

    std::vector<int16_t> out(k10ms);
    ASSERT_EQ(pipeline.render_test_output(out.data(), out.size()), 0);  // silence

    const auto pcm = constant_stereo(k10ms * 3, 2000, 2000);
    ASSERT_TRUE(pipeline.load_ui_sound(5, pcm.data(), k10ms * 3));
    ASSERT_TRUE(pipeline.play_ui_sound(5, 0.5f));

    ASSERT_EQ(pipeline.render_test_output(out.data(), out.size()),
              static_cast<int>(out.size()));
    // The test backend is mono: the downmix of the stereo frame.
    EXPECT_EQ(out[k10ms - 1], 1000);
    pipeline.shutdown();
}
