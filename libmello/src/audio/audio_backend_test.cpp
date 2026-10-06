#include "audio_backend_test.hpp"
#include "../util/log.hpp"
#include <cstdlib>
#include <cstring>
#ifdef _WIN32
#include <windows.h>
#endif

namespace mello::audio {

bool test_audio_backend_requested() {
#ifdef _WIN32
    // Read the process environment block, not the CRT copy: the CRT
    // snapshots the environment at startup, so getenv() misses a variable
    // that the host process (the Rust harness) sets after it started.
    char buf[16] = {};
    DWORD n = GetEnvironmentVariableA("MELLO_AUDIO_BACKEND", buf, sizeof(buf));
    return n > 0 && n < sizeof(buf) && std::strcmp(buf, "test") == 0;
#else
    const char* env = std::getenv("MELLO_AUDIO_BACKEND");
    return env != nullptr && std::strcmp(env, "test") == 0;
#endif
}

bool TestCapture::initialize(const char*) {
    MELLO_LOG_INFO("capture", "test backend: no capture device (use capture inject)");
    return true;
}

bool TestCapture::start(Callback) {
    // No device thread. Audio enters through AudioPipeline::inject_capture().
    return true;
}

void TestCapture::stop() {}

bool TestPlayback::initialize(const char*) {
    MELLO_LOG_INFO("playback", "test backend: no playback device (pull output explicitly)");
    return true;
}

bool TestPlayback::start() { return true; }

void TestPlayback::stop() {}

size_t TestPlayback::feed(const int16_t*, size_t count) { return count; }

size_t TestPlayback::render(int16_t* out, size_t count) {
    // Same contract as the WASAPI render thread: ask the render source for
    // `count` samples and pad the rest with silence.
    size_t got = 0;
    if (render_source_) {
        got = render_source_(out, count);
    }
    if (got < count) {
        std::memset(out + got, 0, (count - got) * sizeof(int16_t));
    }
    return got;
}

std::unique_ptr<AudioCapture> create_test_audio_capture() {
    return std::make_unique<TestCapture>();
}

std::unique_ptr<AudioPlayback> create_test_audio_playback() {
    return std::make_unique<TestPlayback>();
}

} // namespace mello::audio
