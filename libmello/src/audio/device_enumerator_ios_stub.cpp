// iOS audio device enumerator — Step 1 stub.
// iOS exposes a single system-managed audio route (no device picker), so this
// returns no devices. The route-aware AVAudioSession version lands with the
// RemoteIO backend (IOS-LIBMELLO-PORT §1a Step 3).
#include "device_enumerator.hpp"

namespace mello::audio {

class IosStubDeviceEnumerator : public AudioDeviceEnumerator {
public:
    std::vector<AudioDeviceInfo> list_capture_devices() override { return {}; }
    std::vector<AudioDeviceInfo> list_playback_devices() override { return {}; }
};

std::unique_ptr<AudioDeviceEnumerator> create_device_enumerator() {
    return std::make_unique<IosStubDeviceEnumerator>();
}

} // namespace mello::audio
