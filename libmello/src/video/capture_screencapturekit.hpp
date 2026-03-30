#pragma once
#ifdef __APPLE__

#include "capture_source.hpp"
#include <atomic>
#include <functional>

namespace mello::video {

class SCKCapture : public CaptureSource {
public:
    using AudioCallback = std::function<void(const float* samples, uint32_t frame_count,
                                              uint32_t channels, uint32_t sample_rate)>;

    SCKCapture();
    ~SCKCapture() override;

    bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) override;
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width()  const override { return width_; }
    uint32_t height() const override { return height_; }
    const char* backend_name() const override { return "ScreenCaptureKit"; }

    bool get_cursor(CursorData& out) override;

    /// Enable game audio capture. Must be called before start().
    /// The callback receives interleaved float PCM at the source sample rate.
    void set_audio_callback(AudioCallback cb);
    bool audio_enabled() const { return audio_enabled_; }

private:
    void* stream_   = nullptr; // SCStream*
    void* delegate_ = nullptr; // SCKDelegate* (ObjC helper)
    void* filter_   = nullptr; // SCContentFilter*

    uint32_t width_  = 0;
    uint32_t height_ = 0;
    std::atomic<bool> running_{false};
    FrameCallback callback_;
    AudioCallback audio_callback_;
    bool audio_enabled_ = false;
};

} // namespace mello::video

#endif
