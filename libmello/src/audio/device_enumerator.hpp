#pragma once
#include <string>
#include <vector>
#include <memory>

namespace mello::audio {

struct AudioDeviceInfo {
    std::string id;
    std::string name;
    bool is_default;
};

class AudioDeviceEnumerator {
public:
    virtual ~AudioDeviceEnumerator() = default;
    virtual std::vector<AudioDeviceInfo> list_capture_devices() = 0;
    virtual std::vector<AudioDeviceInfo> list_playback_devices() = 0;
};

std::unique_ptr<AudioDeviceEnumerator> create_device_enumerator();

} // namespace mello::audio
