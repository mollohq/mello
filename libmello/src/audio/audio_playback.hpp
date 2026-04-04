#pragma once
#include <cstdint>
#include <cstddef>
#include <memory>
#include <functional>

namespace mello::audio {

using RenderSourceFn = std::function<size_t(int16_t* out, size_t count)>;

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

    /// Set an external render source that provides mixed audio.
    /// When set, the render callback pulls from this instead of feed().
    virtual void set_render_source(RenderSourceFn fn) { render_source_ = std::move(fn); }

    virtual uint32_t sample_rate() const = 0;

protected:
    RenderSourceFn render_source_;
};

/// Create platform-specific playback backend.
std::unique_ptr<AudioPlayback> create_audio_playback();

} // namespace mello::audio
