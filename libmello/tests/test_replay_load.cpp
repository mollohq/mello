// The game clip replay sheds first under overload (12-STREAMING.md 3.7,
// F3), and one game audio capture serves the stream and the replay (F7).
#include <gtest/gtest.h>
#include "replay/replay_load.hpp"

#include <atomic>
#include <chrono>
#include <future>
#include <thread>
#include <vector>

#include "audio/shared_game_audio.hpp"
#include <cstdlib>
#ifdef _WIN32
#include "audio/playback_wasapi.hpp"
#include <windows.h>
#elif defined(__APPLE__)
#include <AudioToolbox/AudioToolbox.h>
#include <unistd.h>
#include <cmath>
#endif

using mello::replay::ReplayLoad;
using Level = ReplayLoad::Level;

namespace {
constexpr uint64_t kS = 1'000'000;
}

TEST(ReplayLoad, TheFirstOverloadDropsTo30FpsAndTheNextPauses) {
    ReplayLoad load;
    EXPECT_EQ(load.fps(60), 60u);
    EXPECT_TRUE(load.shed(10 * kS, 60));
    EXPECT_EQ(load.level(), Level::Reduced);
    EXPECT_EQ(load.fps(60), 30u);
    EXPECT_TRUE(load.shed(11 * kS, 60));
    EXPECT_EQ(load.level(), Level::Paused);
    EXPECT_EQ(load.fps(60), 0u);
    EXPECT_FALSE(load.shed(12 * kS, 60)) << "paused: nothing left, the stream moves its tier";
}

TEST(ReplayLoad, AReplayAt30FpsPausesAtTheFirstOverload) {
    ReplayLoad load;
    EXPECT_TRUE(load.shed(10 * kS, 30));
    EXPECT_EQ(load.level(), Level::Paused);
}

TEST(ReplayLoad, SixtySecondsWithNoOverloadGoOneLevelUp) {
    ReplayLoad load;
    load.shed(10 * kS, 60);
    load.shed(10 * kS, 60);
    ASSERT_EQ(load.level(), Level::Paused);
    EXPECT_FALSE(load.recover(69 * kS));
    EXPECT_TRUE(load.recover(70 * kS));
    EXPECT_EQ(load.level(), Level::Reduced);
    EXPECT_FALSE(load.recover(71 * kS)) << "each level up waits its own 60 s";
    EXPECT_TRUE(load.recover(130 * kS));
    EXPECT_EQ(load.level(), Level::Full);
    EXPECT_FALSE(load.recover(500 * kS));
}

TEST(ReplayLoad, AnOverloadRestartsTheWait) {
    ReplayLoad load;
    load.shed(10 * kS, 60);
    load.shed(50 * kS, 60);
    ASSERT_EQ(load.level(), Level::Paused);
    EXPECT_FALSE(load.recover(70 * kS)) << "60 s after the first overload, not the last";
    EXPECT_TRUE(load.recover(110 * kS));
}

using mello::audio::SharedGameAudio;

TEST(SharedGameAudio, EveryConsumerGetsEveryPacketAndARemovedOneNone) {
    auto shared = SharedGameAudio::for_test(1234);
    std::vector<int16_t> a, b;
    int64_t time_b = 0;
    const auto id_a = shared->add([&](const int16_t* s, size_t n, int64_t) { a.insert(a.end(), s, s + n); });
    const auto id_b = shared->add([&](const int16_t* s, size_t n, int64_t t) {
        b.insert(b.end(), s, s + n);
        time_b = t;
    });
    ASSERT_NE(id_a, 0u);
    ASSERT_NE(id_b, 0u);
    const int16_t first[] = {1, 2, 3, 4};
    shared->deliver_for_test(first, 4);
    shared->remove(id_a);
    const int16_t second[] = {5, 6};
    shared->deliver_for_test(second, 2, 777);
    EXPECT_EQ(a, (std::vector<int16_t>{1, 2, 3, 4}));
    EXPECT_EQ(b, (std::vector<int16_t>{1, 2, 3, 4, 5, 6}));
    EXPECT_EQ(time_b, 777) << "the capture time reaches the consumer";
    EXPECT_EQ(shared->add(nullptr), 0u);
}

// The stream stops its audio while the capture thread is inside its call: the
// remove waits, so the stream's pipeline is never called after it is freed.
TEST(SharedGameAudio, RemoveWaitsForACallInFlight) {
    auto shared = SharedGameAudio::for_test(1234);
    std::promise<void> entered;
    std::promise<void> release;
    auto release_future = release.get_future().share();
    const auto id = shared->add([&](const int16_t*, size_t, int64_t) {
        entered.set_value();
        release_future.wait();
    });
    const int16_t pcm[] = {0, 0};
    std::thread capture([&] { shared->deliver_for_test(pcm, 2); });
    entered.get_future().wait();

    std::atomic<bool> removed{false};
    std::thread stopper([&] {
        shared->remove(id);
        removed = true;
    });
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
    EXPECT_FALSE(removed.load()) << "remove returned while the call ran";
    release.set_value();
    capture.join();
    stopper.join();
    EXPECT_TRUE(removed.load());
}

#ifdef _WIN32
// The real process loopback, with no sound: this process renders silence,
// and one loopback of its own process tree feeds two consumers. Needs an
// audio output device, so it skips under CI.
TEST(SharedGameAudio, OneProcessLoopbackFeedsTwoConsumers) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    mello::audio::WasapiPlayback playback;
    playback.set_input_channels(2);
    if (!playback.initialize(nullptr)) GTEST_SKIP() << "no audio output device";
    playback.set_render_source([](int16_t* out, size_t count) {
        std::fill(out, out + count, int16_t{0});
        return count;
    });
    ASSERT_TRUE(playback.start());

    auto shared = SharedGameAudio::open(static_cast<uint32_t>(GetCurrentProcessId()));
    if (!shared) {
        playback.stop();
        GTEST_SKIP() << "no process loopback on this Windows";
    }
    std::atomic<size_t> a{0}, b{0};
    shared->add([&](const int16_t*, size_t n, int64_t) { a += n; });
    shared->add([&](const int16_t*, size_t n, int64_t) { b += n; });
    std::this_thread::sleep_for(std::chrono::seconds(1));
    shared.reset();
    playback.stop();
    // 48 kHz stereo: about 96000 samples a second to each.
    EXPECT_GT(a.load(), 48000u);
    EXPECT_EQ(a.load(), b.load()) << "both consumers got the same packets";
}
#endif

#ifdef __APPLE__
namespace {

struct TapTone {
    double phase = 0;
};

OSStatus render_tap_tone(void* user, AudioUnitRenderActionFlags*, const AudioTimeStamp*, UInt32, UInt32 frames,
                         AudioBufferList* io) {
    auto* tone = static_cast<TapTone*>(user);
    for (UInt32 i = 0; i < frames; ++i) {
        const float v = static_cast<float>(0.2 * std::sin(tone->phase));
        tone->phase += 2 * 3.14159265358979323846 * 440.0 / 48000.0;
        for (UInt32 b = 0; b < io->mNumberBuffers; ++b) static_cast<float*>(io->mBuffers[b].mData)[i] = v;
    }
    return noErr;
}

}  // namespace

// The real process tap: this process plays 440 Hz on the default output, and
// one tap of its own pid feeds two consumers with the tone and capture times
// on the steady clock. Needs an output device and the audio capture
// permission, so it skips under CI.
TEST(SharedGameAudio, OneProcessTapFeedsTwoConsumers) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    AudioComponentDescription d{kAudioUnitType_Output, kAudioUnitSubType_DefaultOutput, kAudioUnitManufacturer_Apple, 0, 0};
    AudioComponent c = AudioComponentFindNext(nullptr, &d);
    AudioUnit unit = nullptr;
    if (!c || AudioComponentInstanceNew(c, &unit) != noErr) GTEST_SKIP() << "no audio output";
    AudioStreamBasicDescription f{};
    f.mSampleRate = 48000;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonInterleaved;
    f.mChannelsPerFrame = 2;
    f.mFramesPerPacket = 1;
    f.mBitsPerChannel = 32;
    f.mBytesPerFrame = 4;
    f.mBytesPerPacket = 4;
    AudioUnitSetProperty(unit, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 0, &f, sizeof(f));
    TapTone tone;
    AURenderCallbackStruct cb{render_tap_tone, &tone};
    AudioUnitSetProperty(unit, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0, &cb, sizeof(cb));
    if (AudioUnitInitialize(unit) != noErr || AudioOutputUnitStart(unit) != noErr) {
        AudioComponentInstanceDispose(unit);
        GTEST_SKIP() << "the output did not start";
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(300));  // the process object appears

    auto shared = SharedGameAudio::open(static_cast<uint32_t>(getpid()));
    if (!shared) {
        AudioOutputUnitStop(unit);
        AudioComponentInstanceDispose(unit);
        GTEST_SKIP() << "no process tap (audio capture permission?)";
    }
    std::atomic<size_t> a{0}, b{0};
    std::atomic<int64_t> lag_us{-1};
    std::atomic<int32_t> peak{0};
    shared->add([&](const int16_t* s, size_t n, int64_t capture_us) {
        a += n;
        if (capture_us > 0) {
            const int64_t now = static_cast<int64_t>(std::chrono::duration_cast<std::chrono::microseconds>(
                                                         std::chrono::steady_clock::now().time_since_epoch())
                                                         .count());
            lag_us = now - capture_us;
        }
        for (size_t i = 0; i < n; ++i) peak = std::max<int32_t>(peak, std::abs(static_cast<int32_t>(s[i])));
    });
    shared->add([&](const int16_t*, size_t n, int64_t) { b += n; });
    std::this_thread::sleep_for(std::chrono::seconds(1));
    shared.reset();
    AudioOutputUnitStop(unit);
    AudioComponentInstanceDispose(unit);

    EXPECT_GT(a.load(), 48000u) << "about 96000 samples a second";
    EXPECT_EQ(a.load(), b.load()) << "both consumers got the same packets";
    EXPECT_GT(peak.load(), 5000) << "the tone, not silence (0.2 of full scale is 6553)";
    EXPECT_GE(lag_us.load(), 0) << "a capture time on the steady clock";
    EXPECT_LT(lag_us.load(), 100'000) << "the capture time is recent";
}
#endif
