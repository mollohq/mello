#pragma once
#if defined(__APPLE__)
#include <TargetConditionals.h>
#endif

#if defined(__APPLE__) && TARGET_OS_IPHONE
#include "audio_playback.hpp"
#include "../util/ring_buffer.hpp"
#include <AudioToolbox/AudioToolbox.h>
#include <AudioUnit/AudioUnit.h>
#include <atomic>

namespace mello::audio {

// iOS speaker playback via a RemoteIO AudioUnit (IOS-LIBMELLO-PORT §5). Mirrors the
// macOS CoreAudioPlayback but uses kAudioUnitSubType_RemoteIO and the system route.
class RemoteIOPlayback : public AudioPlayback {
public:
    RemoteIOPlayback();
    ~RemoteIOPlayback() override;

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
    uint32_t sample_rate_ = 48000;

    std::atomic<bool> running_{false};
    util::RingBuffer<int16_t> ring_{48000}; // ~1s at mono 48kHz
};

} // namespace mello::audio
#endif
