#ifdef __APPLE__
#include "device_enumerator.hpp"
#include "../util/log.hpp"
#include <CoreAudio/CoreAudio.h>
#include <string>
#include <vector>

namespace mello::audio {

// Convert a CFStringRef to std::string
static std::string cf_string_to_utf8(CFStringRef str) {
    if (!str) return {};
    CFIndex len = CFStringGetLength(str);
    CFIndex maxSize = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1;
    std::string result(static_cast<size_t>(maxSize), '\0');
    if (!CFStringGetCString(str, result.data(), maxSize, kCFStringEncodingUTF8)) {
        return {};
    }
    result.resize(std::strlen(result.c_str()));
    return result;
}

class CoreAudioDeviceEnumerator : public AudioDeviceEnumerator {
public:
    std::vector<AudioDeviceInfo> list_capture_devices() override {
        return enumerate(true);
    }

    std::vector<AudioDeviceInfo> list_playback_devices() override {
        return enumerate(false);
    }

private:
    std::vector<AudioDeviceInfo> enumerate(bool is_input) {
        std::vector<AudioDeviceInfo> result;

        // Get default device for comparison
        AudioDeviceID default_device = kAudioObjectUnknown;
        {
            AudioObjectPropertyAddress prop = {
                is_input ? kAudioHardwarePropertyDefaultInputDevice
                         : kAudioHardwarePropertyDefaultOutputDevice,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain
            };
            UInt32 size = sizeof(default_device);
            AudioObjectGetPropertyData(kAudioObjectSystemObject, &prop, 0, nullptr, &size, &default_device);
        }

        // Get all audio device IDs
        AudioObjectPropertyAddress prop = {
            kAudioHardwarePropertyDevices,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain
        };
        UInt32 size = 0;
        OSStatus status = AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &prop, 0, nullptr, &size);
        if (status != noErr || size == 0) return result;

        UInt32 device_count = size / sizeof(AudioDeviceID);
        std::vector<AudioDeviceID> devices(device_count);
        status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &prop, 0, nullptr, &size, devices.data());
        if (status != noErr) return result;

        AudioObjectPropertyScope scope = is_input ? kAudioObjectPropertyScopeInput
                                                  : kAudioObjectPropertyScopeOutput;

        for (AudioDeviceID dev : devices) {
            // Check if this device has streams in the desired direction
            AudioObjectPropertyAddress stream_prop = {
                kAudioDevicePropertyStreams,
                scope,
                kAudioObjectPropertyElementMain
            };
            UInt32 stream_size = 0;
            status = AudioObjectGetPropertyDataSize(dev, &stream_prop, 0, nullptr, &stream_size);
            if (status != noErr || stream_size == 0) continue; // No streams in this direction

            AudioDeviceInfo info;
            info.id = std::to_string(dev);
            info.is_default = (dev == default_device);

            // Get device name
            AudioObjectPropertyAddress name_prop = {
                kAudioObjectPropertyName,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain
            };
            CFStringRef name_ref = nullptr;
            UInt32 name_size = sizeof(name_ref);
            status = AudioObjectGetPropertyData(dev, &name_prop, 0, nullptr, &name_size, &name_ref);
            if (status == noErr && name_ref) {
                info.name = cf_string_to_utf8(name_ref);
                CFRelease(name_ref);
            } else {
                info.name = "Unknown Device";
            }

            result.push_back(std::move(info));
        }

        MELLO_LOG_INFO("devices", "CoreAudio: enumerated %zu %s devices",
                       result.size(), is_input ? "capture" : "playback");
        for (size_t i = 0; i < result.size(); ++i) {
            MELLO_LOG_DEBUG("devices", "  [%zu] %s%s: %s",
                            i, result[i].name.c_str(),
                            result[i].is_default ? " (default)" : "",
                            result[i].id.c_str());
        }
        return result;
    }
};

std::unique_ptr<AudioDeviceEnumerator> create_device_enumerator() {
    return std::make_unique<CoreAudioDeviceEnumerator>();
}

} // namespace mello::audio
#endif
