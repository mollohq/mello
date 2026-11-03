// The game clip replay sheds first under overload (12-STREAMING.md 3.7,
// F3), and one game audio capture serves the stream and the replay (F7).
#include <gtest/gtest.h>
#include "replay/replay_load.hpp"

#include <atomic>
#include <chrono>
#include <future>
#include <thread>
#include <vector>

#ifdef _WIN32
#include "audio/playback_wasapi.hpp"
#include "audio/shared_game_audio.hpp"
#include <windows.h>
#include <cstdlib>
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

#ifdef _WIN32
using mello::audio::SharedGameAudio;

TEST(SharedGameAudio, EveryConsumerGetsEveryPacketAndARemovedOneNone) {
    auto shared = SharedGameAudio::for_test(1234);
    std::vector<int16_t> a, b;
    const auto id_a = shared->add([&](const int16_t* s, size_t n) { a.insert(a.end(), s, s + n); });
    const auto id_b = shared->add([&](const int16_t* s, size_t n) { b.insert(b.end(), s, s + n); });
    ASSERT_NE(id_a, 0u);
    ASSERT_NE(id_b, 0u);
    const int16_t first[] = {1, 2, 3, 4};
    shared->deliver_for_test(first, 4);
    shared->remove(id_a);
    const int16_t second[] = {5, 6};
    shared->deliver_for_test(second, 2);
    EXPECT_EQ(a, (std::vector<int16_t>{1, 2, 3, 4}));
    EXPECT_EQ(b, (std::vector<int16_t>{1, 2, 3, 4, 5, 6}));
    EXPECT_EQ(shared->add(nullptr), 0u);
}

// The stream stops its audio while the capture thread is inside its call: the
// remove waits, so the stream's pipeline is never called after it is freed.
TEST(SharedGameAudio, RemoveWaitsForACallInFlight) {
    auto shared = SharedGameAudio::for_test(1234);
    std::promise<void> entered;
    std::promise<void> release;
    auto release_future = release.get_future().share();
    const auto id = shared->add([&](const int16_t*, size_t) {
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
    shared->add([&](const int16_t*, size_t n) { a += n; });
    shared->add([&](const int16_t*, size_t n) { b += n; });
    std::this_thread::sleep_for(std::chrono::seconds(1));
    shared.reset();
    playback.stop();
    // 48 kHz stereo: about 96000 samples a second to each.
    EXPECT_GT(a.load(), 48000u);
    EXPECT_EQ(a.load(), b.load()) << "both consumers got the same packets";
}
#endif
