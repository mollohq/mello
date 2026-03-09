#if !defined(_WIN32) && !defined(__APPLE__)
#include "device_enumerator.hpp"

namespace mello::audio {

class StubDeviceEnumerator : public AudioDeviceEnumerator {
public:
    std::vector<AudioDeviceInfo> list_capture_devices() override { return {}; }
    std::vector<AudioDeviceInfo> list_playback_devices() override { return {}; }
};

std::unique_ptr<AudioDeviceEnumerator> create_device_enumerator() {
    return std::make_unique<StubDeviceEnumerator>();
}

} // namespace mello::audio
#endif
