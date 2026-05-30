// iOS audio backend — Step 1 stub (silent).
// Lets mello-core link and run on iOS against Nakama with libmello present but
// silent. The real RemoteIO + AVAudioSession backend lands in IOS-LIBMELLO-PORT
// §1a Step 3 and replaces this file in the CMake iOS source list.
#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include "../util/log.hpp"

namespace mello::audio {

class IosStubCapture : public AudioCapture {
public:
    bool initialize(const char*) override {
        MELLO_LOG_WARN("capture", "iOS stub: audio capture not implemented yet (Step 1)");
        return true;
    }
    bool start(Callback) override { return false; }
    void stop() override {}
    uint32_t sample_rate() const override { return 48000; }
    uint32_t channels() const override { return 1; }
};

class IosStubPlayback : public AudioPlayback {
public:
    bool initialize(const char*) override {
        MELLO_LOG_WARN("playback", "iOS stub: audio playback not implemented yet (Step 1)");
        return true;
    }
    bool start() override { return false; }
    void stop() override {}
    size_t feed(const int16_t*, size_t) override { return 0; }
    uint32_t sample_rate() const override { return 48000; }
};

std::unique_ptr<AudioCapture> create_audio_capture() {
    return std::make_unique<IosStubCapture>();
}

std::unique_ptr<AudioPlayback> create_audio_playback() {
    return std::make_unique<IosStubPlayback>();
}

} // namespace mello::audio
