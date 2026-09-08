// VPIO duplex backend tests (macOS only). The duplex unit needs real
// CoreAudio devices, so the hardware cases SKIP (never fail) when no
// usable route exists — headless CI stays green, dev machines exercise
// the real path.
#ifdef __APPLE__

#include <gtest/gtest.h>
#include <atomic>
#include <chrono>
#include <cstring>
#include <thread>
#include "audio/vpio_duplex.hpp"

using namespace mello::audio;

TEST(VpioDuplex, InitializesOnDefaultDevicesWhenPresent) {
    auto unit = VpioUnit::create();
    ASSERT_NE(unit, nullptr);
    if (!unit->initialize(nullptr, nullptr)) {
        GTEST_SKIP() << "no usable CoreAudio duplex route on this machine";
    }
    EXPECT_TRUE(unit->initialized());
    EXPECT_EQ(unit->sample_rate(), 48000u);
    EXPECT_EQ(unit->channels(), 1u);

    VpioCaptureAdapter cap(unit);
    VpioPlaybackAdapter play(unit);
    EXPECT_TRUE(cap.initialize(nullptr));
    EXPECT_TRUE(play.initialize(nullptr));
    EXPECT_TRUE(cap.provides_echo_cancellation());

    std::atomic<int> captured_samples{0};
    EXPECT_TRUE(
        cap.start([&](const int16_t* /*samples*/, size_t count) {
            captured_samples.fetch_add(static_cast<int>(count),
                                       std::memory_order_relaxed);
        }));

    // Rendered-frame counting: guards render-source forwarding (a missing
    // forward once left VPIO playout permanently silent — no crash, just
    // no audio out, including clips).
    std::atomic<int> rendered_samples{0};
    play.set_render_source([&](int16_t* out, size_t count) {
        std::memset(out, 0, count * sizeof(int16_t));
        rendered_samples.fetch_add(static_cast<int>(count),
                                   std::memory_order_relaxed);
        return count;
    });
    EXPECT_TRUE(play.start());

    // Live-audio smoke: callbacks must deliver frames within the window.
    // Generous by design (callback startup alone can take tens of ms).
    std::this_thread::sleep_for(std::chrono::milliseconds(500));
    EXPECT_GT(captured_samples.load(std::memory_order_relaxed), 0);
    EXPECT_GT(rendered_samples.load(std::memory_order_relaxed), 0);

    cap.stop();
    play.stop();
    unit->shutdown();
    EXPECT_FALSE(unit->initialized());
}

TEST(VpioDuplex, InvalidDeviceIdFailsCleanly) {
    auto unit = VpioUnit::create();
    ASSERT_NE(unit, nullptr);
    // Must fail (not trap): bogus ids route to the plain-HAL fallback.
    EXPECT_FALSE(unit->initialize("nonexistent_device_xyz", nullptr));
    EXPECT_FALSE(unit->initialized());
}

#endif  // __APPLE__
