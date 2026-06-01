#pragma once
#if defined(__APPLE__)
#include <TargetConditionals.h>
#endif

#if defined(__APPLE__) && TARGET_OS_IPHONE
#include "audio_capture.hpp"
#include <AudioToolbox/AudioToolbox.h>
#include <AudioUnit/AudioUnit.h>
#include <atomic>
#include <vector>

namespace mello::audio {

// iOS mic capture via a RemoteIO AudioUnit (IOS-LIBMELLO-PORT §5). Mirrors the
// macOS CoreAudioCapture but uses kAudioUnitSubType_RemoteIO and the
// system-managed AVAudioSession route (no AudioDeviceID — iOS has no device picker).
class RemoteIOCapture : public AudioCapture {
public:
    RemoteIOCapture();
    ~RemoteIOCapture() override;

    bool initialize(const char* device_id = nullptr) override;
    bool start(Callback callback) override;
    void stop() override;

    uint32_t sample_rate() const override { return sample_rate_; }
    uint32_t channels() const override { return channels_; }

private:
    static OSStatus input_callback(
        void* inRefCon,
        AudioUnitRenderActionFlags* ioActionFlags,
        const AudioTimeStamp* inTimeStamp,
        UInt32 inBusNumber,
        UInt32 inNumberFrames,
        AudioBufferList* ioData);

    AudioComponentInstance audio_unit_ = nullptr;

    uint32_t sample_rate_ = 48000;
    uint32_t channels_ = 1;

    std::atomic<bool> running_{false};
    Callback callback_;

    AudioBufferList* buffer_list_ = nullptr;
};

} // namespace mello::audio
#endif
