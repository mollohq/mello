#ifdef __APPLE__
#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include "capture_coreaudio.hpp"
#include "playback_coreaudio.hpp"

namespace mello::audio {

std::unique_ptr<AudioCapture> create_audio_capture() {
    return std::make_unique<CoreAudioCapture>();
}

std::unique_ptr<AudioPlayback> create_audio_playback() {
    return std::make_unique<CoreAudioPlayback>();
}

} // namespace mello::audio
#endif
