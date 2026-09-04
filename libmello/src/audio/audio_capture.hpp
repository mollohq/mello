#pragma once
#include <functional>
#include <cstdint>
#include <cstddef>
#include <memory>

namespace mello::audio {

/// Abstract audio capture interface.
/// Platform backends (WASAPI, CoreAudio) implement this.
class AudioCapture {
public:
    using Callback = std::function<void(const int16_t* samples, size_t count)>;

    virtual ~AudioCapture() = default;

    virtual bool initialize(const char* device_id = nullptr) = 0;
    virtual bool start(Callback callback) = 0;
    virtual void stop() = 0;

    virtual uint32_t sample_rate() const = 0;
    virtual uint32_t channels() const = 0;

    /// Estimated device + safety-offset input latency in ms.
    /// Default 0 (unknown). CoreAudio overrides with
    /// kAudioUnitProperty_Latency + SafetyOffset. Windows override
    /// (WASAPI GetStreamLatency + GetDevicePeriod) is a handoff TODO.
    virtual int input_latency_ms() const { return 0; }

    /// Select the OS voice-processing capture path (macOS
    /// VoiceProcessingIO). Must be set before initialize(); ignored by
    /// backends without one. Default no-op.
    virtual void set_voice_processing_enabled(bool /*enabled*/) {}

    /// True when the active backend cancels echo itself (VPIO). The
    /// pipeline skips its own APM capture pass in that case to avoid
    /// double processing. Default false.
    virtual bool provides_echo_cancellation() const { return false; }
};

/// Create platform-specific capture backend.
std::unique_ptr<AudioCapture> create_audio_capture();

} // namespace mello::audio
