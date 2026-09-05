#pragma once

// Combined VoiceProcessingIO duplex unit (macOS only).
//
// Apple's AEC reference is the audio rendered through the VPIO unit's own
// output bus, so capture and playback must share one unit: an input-only
// VPIO unit never initializes (-10875), and a split pair cannot cancel.
// This file owns the shared unit plus thin AudioCapture / AudioPlayback
// adapters over it. The pipeline activates the pair when the echo toggle
// is on and falls back to the plain HAL pair otherwise.
//
// Threading mirrors the split backends: the input callback runs on the
// capture realtime thread, the render callback on the playout thread.
// Unit setup/teardown serializes on coreaudio_unit_mutex(); start/stop
// refcounts serialize under an internal mutex. No locks on realtime paths.
#ifdef __APPLE__

#include "audio_capture.hpp"
#include "audio_playback.hpp"
#include "../util/ring_buffer.hpp"
#include <AudioToolbox/AudioToolbox.h>
#include <CoreAudio/CoreAudio.h>
#include <atomic>
#include <cstdint>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

namespace mello::audio {

class VpioUnit : public std::enable_shared_from_this<VpioUnit> {
public:
    static std::shared_ptr<VpioUnit> create() {
        return std::make_shared<VpioUnit>();
    }

    VpioUnit() = default;
    ~VpioUnit();

    VpioUnit(const VpioUnit&) = delete;
    VpioUnit& operator=(const VpioUnit&) = delete;

    /// Empty ids follow the system default device per direction.
    bool initialize(const char* capture_device_id, const char* playback_device_id);
    void shutdown();

    bool initialized() const { return audio_unit_ != nullptr; }
    // Input side (called via VpioCaptureAdapter, main thread).
    bool start_capture(AudioCapture::Callback callback);
    void stop_capture();

    // Output side (called via VpioPlaybackAdapter, main thread).
    void set_render_source(RenderSourceFn fn);
    bool start_playback();
    void stop_playback();
    size_t feed(const int16_t* samples, size_t count);

    uint32_t sample_rate() const { return sample_rate_; }
    uint32_t channels() const { return 1; }
    int input_latency_ms() const { return cached_input_latency_ms_; }
    int output_latency_ms() const { return cached_output_latency_ms_; }

private:
    static OSStatus input_callback(void* inRefCon,
                                   AudioUnitRenderActionFlags* ioActionFlags,
                                   const AudioTimeStamp* inTimeStamp,
                                   UInt32 inBusNumber,
                                   UInt32 inNumberFrames,
                                   AudioBufferList* ioData);
    static OSStatus render_callback(void* inRefCon,
                                    AudioUnitRenderActionFlags* ioActionFlags,
                                    const AudioTimeStamp* inTimeStamp,
                                    UInt32 inBusNumber,
                                    UInt32 inNumberFrames,
                                    AudioBufferList* ioData);

    // Teardown without taking coreaudio_unit_mutex() (for init-failure
    // paths, which already hold it).
    void shutdown_locked();
    int query_latency_ms(AudioObjectPropertyScope scope, AudioDeviceID device) const;

    AudioComponentInstance audio_unit_ = nullptr;
    AudioDeviceID input_device_ = kAudioObjectUnknown;
    AudioDeviceID output_device_ = kAudioObjectUnknown;

    uint32_t sample_rate_ = 48000;
    int cached_input_latency_ms_ = 0;
    int cached_output_latency_ms_ = 0;

    std::mutex mutex_;
    int input_users_ = 0;
    int output_users_ = 0;

    std::atomic<bool> capturing_{false};
    AudioCapture::Callback capture_callback_;
    RenderSourceFn render_source_;
    // Voice playout is mono; sized like the HAL playback ring.
    util::RingBuffer<int16_t> ring_{48000 * 2};

    AudioBufferList* capture_buffer_list_ = nullptr;
    // Capture buffer capacity in frames. Deliberately larger than the
    // unit's MaximumFramesPerSlice: VPIO has delivered 960-frame slices
    // against a reported max of 512, overflowing a max-sized buffer and
    // corrupting the heap (field abort + ASan heap-buffer-overflow).
    size_t capture_buffer_capacity_frames_ = 0;
    static constexpr size_t kCaptureBufferCapFrames = 8192;
};

/// AudioCapture hat over a shared VpioUnit. initialize() only verifies the
/// unit is live; the pipeline creates the unit before the adapters.
class VpioCaptureAdapter : public AudioCapture {
public:
    explicit VpioCaptureAdapter(std::shared_ptr<VpioUnit> unit)
        : unit_(std::move(unit)) {}

    bool initialize(const char* device_id = nullptr) override;
    bool start(Callback callback) override;
    void stop() override;

    uint32_t sample_rate() const override { return 48000; }
    uint32_t channels() const override { return 1; }
    int input_latency_ms() const override;
    bool provides_echo_cancellation() const override;

private:
    std::shared_ptr<VpioUnit> unit_;
};

/// AudioPlayback hat over a shared VpioUnit.
class VpioPlaybackAdapter : public AudioPlayback {
public:
    explicit VpioPlaybackAdapter(std::shared_ptr<VpioUnit> unit)
        : unit_(std::move(unit)) {}

    bool initialize(const char* device_id = nullptr) override;
    bool start() override;
    void stop() override;
    size_t feed(const int16_t* samples, size_t count) override;
    void set_render_source(RenderSourceFn fn) override;
    void set_input_channels(uint32_t channels) override;
    uint32_t sample_rate() const override { return 48000; }
    int output_latency_ms() const override;

private:
    std::shared_ptr<VpioUnit> unit_;
};

}  // namespace mello::audio

#endif  // __APPLE__
