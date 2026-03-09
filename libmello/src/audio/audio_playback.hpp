#pragma once
#include <cstdint>
#include <cstddef>
#include <memory>

namespace mello::audio {

/// Abstract audio playback interface.
/// Platform backends (WASAPI, CoreAudio) implement this.
class AudioPlayback {
public:
    virtual ~AudioPlayback() = default;

    virtual bool initialize(const char* device_id = nullptr) = 0;
    virtual bool start() = 0;
    virtual void stop() = 0;

    /// Feed mono 16-bit PCM samples into the playback buffer.
    virtual size_t feed(const int16_t* samples, size_t count) = 0;

    virtual uint32_t sample_rate() const = 0;
};

/// Create platform-specific playback backend.
std::unique_ptr<AudioPlayback> create_audio_playback();

} // namespace mello::audio
