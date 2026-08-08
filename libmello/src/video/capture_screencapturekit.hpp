#pragma once
#ifdef __APPLE__

#include "capture_source.hpp"
#include <atomic>
#include <functional>
#include <mutex>

namespace mello::video {

class SCKCapture : public CaptureSource {
public:
    SCKCapture();
    ~SCKCapture() override;

    bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) override;
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width()  const override { return width_; }
    uint32_t height() const override { return height_; }
    const char* backend_name() const override { return "ScreenCaptureKit"; }

    bool get_cursor(CursorData& out) override;

    /// Route captured game audio to `cb`. Unlike `capturesAudio` — which
    /// SCStream bakes in at creation — this may be set at any time, because
    /// `mello_stream_start_audio()` runs after the host stream is already up.
    void set_audio_callback(AudioCallback cb) override;

    /// Called by the ObjC delegate on the audio sample queue. Public only
    /// because the delegate is not a friend of this class.
    void deliver_audio(const float* samples, uint32_t frame_count,
                       uint32_t channels, uint32_t sample_rate);

private:
    void* stream_   = nullptr; // SCStream*
    void* delegate_ = nullptr; // SCKDelegate* (ObjC helper)
    void* filter_   = nullptr; // SCContentFilter*

    uint32_t width_  = 0;
    uint32_t height_ = 0;
    std::atomic<bool> running_{false};
    FrameCallback callback_;

    // Written by whichever thread calls set_audio_callback, read on the SCK
    // audio queue. Assigning a live std::function under another thread's call
    // is UB, so both sides take the lock.
    mutable std::mutex audio_mutex_;
    AudioCallback audio_callback_;
};

} // namespace mello::video

#endif
