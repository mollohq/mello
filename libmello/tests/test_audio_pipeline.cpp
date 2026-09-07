// AudioPipeline backend contract tests (macOS duplex / plain HAL).
// Needs real audio devices: initialize() fails headless, so the suite
// skips (never fails) without hardware — same pattern as VpioDuplex.
#ifdef __APPLE__

#include <gtest/gtest.h>
#include <thread>
#include <chrono>
#include "audio/audio_pipeline.hpp"

using namespace mello::audio;

// The voice-session contract, end to end:
// - startup is always the plain pair (no mic-capable unit before voice),
// - joining with the toggle on activates VPIO (APM capture goes quiet),
// - leaving drops back to plain (APM capture resumes).
// APM capture frames only advance on the software path, so the counter
// observes the backend without any new API.
TEST(PipelineBackend, VoiceSessionScopesDuplex) {
    AudioPipeline pipeline;
    if (!pipeline.initialize()) {
        GTEST_SKIP() << "no audio devices on this machine";
    }

    pipeline.set_echo_cancellation(false);  // plain, deterministic start
    ASSERT_TRUE(pipeline.start_capture());
    std::this_thread::sleep_for(std::chrono::milliseconds(400));
    pipeline.stop_capture();  // quiesce before reading: the mic keeps
    const uint32_t plain_frames = pipeline.aec_capture_frames();
    EXPECT_GT(plain_frames, 0u) << "APM must run on the plain backend";

    pipeline.set_echo_cancellation(true);  // stored only: no session
    ASSERT_TRUE(pipeline.start_capture());  // join -> duplex activates
    std::this_thread::sleep_for(std::chrono::milliseconds(400));
    pipeline.stop_capture();
    EXPECT_EQ(pipeline.aec_capture_frames(), plain_frames)
        << "APM must stay quiet on the VPIO duplex path";

    // Desired is still on from above, so rejoining would go duplex again.
    // Flip off first: plain must resume APM after leaving voice.
    pipeline.set_echo_cancellation(false);
    ASSERT_TRUE(pipeline.start_capture());
    std::this_thread::sleep_for(std::chrono::milliseconds(400));
    pipeline.stop_capture();
    EXPECT_GT(pipeline.aec_capture_frames(), plain_frames)
        << "APM must resume on plain after leaving voice";

    pipeline.shutdown();
}

#endif  // __APPLE__
