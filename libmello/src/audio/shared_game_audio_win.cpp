// The game's sound on Windows (shared_game_audio.hpp): WASAPI process
// loopback of the game's process tree (Windows 10 2004 or later).
#ifdef _WIN32
#include "shared_game_audio.hpp"
#include "capture_wasapi_loopback.hpp"
#include "../util/log.hpp"

namespace mello::audio {

namespace {

constexpr const char* TAG = "game_audio";

class WinGameAudioCapture final : public GameAudioCapture {
public:
    explicit WinGameAudioCapture(std::unique_ptr<WasapiLoopbackCapture> capture) : capture_(std::move(capture)) {}
    ~WinGameAudioCapture() override { capture_->stop(); }

private:
    std::unique_ptr<WasapiLoopbackCapture> capture_;
};

}  // namespace

std::unique_ptr<GameAudioCapture> open_game_audio_capture(uint32_t pid, SharedGameAudio* shared) {
    auto capture = std::make_unique<WasapiLoopbackCapture>();
    // initialize_scoped falls back to endpoint loopback when process loopback
    // fails; that fallback is refused here.
    if (!capture->initialize_scoped(LoopbackScope::IncludeProcessTree, pid) ||
        capture->scope() != LoopbackScope::IncludeProcessTree) {
        MELLO_LOG_WARN(TAG, "no process loopback for pid %u", pid);
        return nullptr;
    }
    // The loopback has no capture time: the consumers use the arrival.
    if (!capture->start([shared](const int16_t* s, size_t n) { shared->deliver(s, n, 0); })) {
        MELLO_LOG_WARN(TAG, "process loopback for pid %u did not start", pid);
        return nullptr;
    }
    MELLO_LOG_INFO(TAG, "process loopback for pid %u started (%u Hz, %u ch)", pid, capture->sample_rate(),
                   capture->channels());
    return std::make_unique<WinGameAudioCapture>(std::move(capture));
}

}  // namespace mello::audio
#endif
