#include <TargetConditionals.h>
#if defined(__APPLE__) && TARGET_OS_IPHONE
#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include "capture_remoteio.hpp"
#include "playback_remoteio.hpp"

namespace mello::audio {

std::unique_ptr<AudioCapture> create_audio_capture() {
    return std::make_unique<RemoteIOCapture>();
}

std::unique_ptr<AudioPlayback> create_audio_playback() {
    return std::make_unique<RemoteIOPlayback>();
}

} // namespace mello::audio
#endif
