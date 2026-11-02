#if defined(__linux__)
#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include "audio_backend_test.hpp"
#include "capture_pipewire.hpp"
#include "playback_pipewire.hpp"

namespace mello::audio {

std::unique_ptr<AudioCapture> create_audio_capture() {
    if (test_audio_backend_requested()) return create_test_audio_capture();
    return std::make_unique<PipeWireCapture>();
}

std::unique_ptr<AudioPlayback> create_audio_playback() {
    if (test_audio_backend_requested()) return create_test_audio_playback();
    return std::make_unique<PipeWirePlayback>();
}

} // namespace mello::audio
#endif
