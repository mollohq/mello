#pragma once

#ifdef __APPLE__
#include "audio_capture.hpp"
#include <AudioToolbox/AudioToolbox.h>
#include <CoreAudio/CoreAudio.h>
#include <thread>
#include <atomic>
#include <vector>

namespace mello::audio {

class CoreAudioCapture : public AudioCapture {
public:
    CoreAudioCapture();
    ~CoreAudioCapture() override;

    bool initialize(const char* device_id = nullptr) override;
    bool start(Callback callback) override;
    void stop() override;

    uint32_t sample_rate() const override { return sample_rate_; }
    uint32_t channels() const override { return channels_; }
    int input_latency_ms() const override { return cached_input_latency_ms_; }

private:
    int query_input_latency_ms();    static OSStatus input_callback(
        void* inRefCon,
        AudioUnitRenderActionFlags* ioActionFlags,
        const AudioTimeStamp* inTimeStamp,
        UInt32 inBusNumber,
        UInt32 inNumberFrames,
        AudioBufferList* ioData);

    AudioComponentInstance audio_unit_ = nullptr;
    AudioDeviceID device_id_ = kAudioObjectUnknown;

    uint32_t sample_rate_ = 48000;
    uint32_t channels_ = 1;
    int cached_input_latency_ms_ = 0;

    std::atomic<bool> running_{false};
    Callback callback_;

    // Buffer for the render callback to fill
    std::vector<int16_t> render_buf_;
    AudioBufferList* buffer_list_ = nullptr;
    // Capture buffer capacity in frames. Sized defensively above the
    // unit's MaximumFramesPerSlice: oversized slices overflow a
    // max-sized buffer and corrupt the heap (seen on VPIO; same guard).
    size_t buffer_capacity_frames_ = 0;
    static constexpr size_t kBufferCapFrames = 8192;
};

} // namespace mello::audio
#endif
