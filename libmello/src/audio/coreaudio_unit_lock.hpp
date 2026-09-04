#pragma once
// Serializes CoreAudio AudioUnit setup/teardown across the capture and
// playback backends. Concurrent InstanceNew/Initialize/Dispose cycles on
// one device trap inside CoreAudio on some systems (observed as SIGTRAP
// in parallel device tests). Setup is rare; a coarse lock is fine.
#include <mutex>

namespace mello::audio {

inline std::mutex& coreaudio_unit_mutex() {
    static std::mutex m;
    return m;
}

}  // namespace mello::audio
