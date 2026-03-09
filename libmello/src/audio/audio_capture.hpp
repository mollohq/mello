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
};

/// Create platform-specific capture backend.
std::unique_ptr<AudioCapture> create_audio_capture();

} // namespace mello::audio
