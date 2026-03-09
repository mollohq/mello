#if !defined(_WIN32) && !defined(__APPLE__)
#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include "../util/log.hpp"

namespace mello::audio {

// Stub capture for unsupported platforms
class StubCapture : public AudioCapture {
public:
    bool initialize(const char*) override {
        MELLO_LOG_WARN("capture", "stub: no audio capture on this platform");
        return true;
    }
    bool start(Callback) override { return false; }
    void stop() override {}
    uint32_t sample_rate() const override { return 48000; }
    uint32_t channels() const override { return 1; }
};

// Stub playback for unsupported platforms
class StubPlayback : public AudioPlayback {
public:
    bool initialize(const char*) override {
        MELLO_LOG_WARN("playback", "stub: no audio playback on this platform");
        return true;
    }
    bool start() override { return false; }
    void stop() override {}
    size_t feed(const int16_t*, size_t) override { return 0; }
    uint32_t sample_rate() const override { return 48000; }
};

std::unique_ptr<AudioCapture> create_audio_capture() {
    return std::make_unique<StubCapture>();
}

std::unique_ptr<AudioPlayback> create_audio_playback() {
    return std::make_unique<StubPlayback>();
}

} // namespace mello::audio
#endif
