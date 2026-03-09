#ifdef _WIN32
#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include "capture_wasapi.hpp"
#include "playback_wasapi.hpp"

namespace mello::audio {

std::unique_ptr<AudioCapture> create_audio_capture() {
    return std::make_unique<WasapiCapture>();
}

std::unique_ptr<AudioPlayback> create_audio_playback() {
    return std::make_unique<WasapiPlayback>();
}

} // namespace mello::audio
#endif
