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

    /// Must be called before `initialize()`; the channel count is baked into
    /// the AudioUnit's stream format.
    void set_input_channels(uint32_t channels) override {
        input_channels_ = channels < 1 ? 1 : channels;
    }

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
    /// Interleaved channels in the samples handed to `feed`. Voice is mono;
    /// stream game audio is stereo.
    uint32_t input_channels_ = 1;

    std::atomic<bool> running_{false};
    // ~1 second at stereo 48 kHz; mono uses half of it.
    util::RingBuffer<int16_t> ring_{48000 * 2};
};

} // namespace mello::audio
#endif
