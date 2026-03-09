#pragma once

#ifdef __APPLE__
#include "audio_playback.hpp"
#include "../util/ring_buffer.hpp"
#include <AudioToolbox/AudioToolbox.h>
#include <CoreAudio/CoreAudio.h>
#include <atomic>

namespace mello::audio {

class CoreAudioPlayback : public AudioPlayback {
public:
    CoreAudioPlayback();
    ~CoreAudioPlayback() override;

    bool initialize(const char* device_id = nullptr) override;
    bool start() override;
    void stop() override;

    size_t feed(const int16_t* samples, size_t count) override;

    uint32_t sample_rate() const override { return sample_rate_; }

private:
    static OSStatus render_callback(
        void* inRefCon,
        AudioUnitRenderActionFlags* ioActionFlags,
        const AudioTimeStamp* inTimeStamp,
        UInt32 inBusNumber,
        UInt32 inNumberFrames,
        AudioBufferList* ioData);

    AudioComponentInstance audio_unit_ = nullptr;
    AudioDeviceID device_id_ = kAudioObjectUnknown;

    uint32_t sample_rate_ = 48000;
    uint32_t device_channels_ = 2;

    std::atomic<bool> running_{false};
    util::RingBuffer<int16_t> ring_{48000}; // ~1 second buffer at mono 48kHz
};

} // namespace mello::audio
#endif
