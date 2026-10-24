#pragma once
// Reference clip capture: the capture-less source of the streaming quality
// gate (tools/stream-gate, plans/streaming-quality.md stage 1). It plays a
// raw YUV 4:2:0 clip from a file at a fixed frame rate on its own thread and
// hands each frame to the pipeline with the same callback contract as the
// screen capture backends: a BGRA D3D11 texture and a capture time in
// microseconds of the steady clock.
//
// Upload. The source reads frame n of the file, packs it as NV12 in memory
// (I420 clips interleave U and V; NV12 clips copy), and calls
// UpdateSubresource on a default-usage NV12 texture. A D3D11 video processor
// converts it to BGRA (input BT.709 studio range, output full-range RGB), the
// inverse of the preprocessor's BGRA to NV12 conversion. The GPU does the
// conversion, so the source costs the CPU lane only the file read and the
// chroma interleave.
//
// Timing. Frame n is due at start + n * 1e6 / fps microseconds. The thread
// sleeps on a high-resolution waitable timer until 1 ms before that time and
// then yields until it. The callback carries the due time, not the wake time,
// so timestamps are exact. A frame handed over more than 2 ms after its due
// time counts as late. The source never skips a frame: every clip frame
// reaches the pipeline in order, and the clip loops at its end.
//
// Threads. initialize() and stop() run on the caller's thread. The source
// thread reads the file, uploads, converts and calls the frame callback. The
// immediate context is multithread protected (graphics_device.cpp), as for
// every capture backend. info() is safe on any thread.

#include "capture_source.hpp"
#include <atomic>
#include <cstdint>
#include <memory>

namespace mello::video {

/// Bytes of one 4:2:0 frame, 8 bits per sample.
constexpr uint64_t reference_frame_bytes(uint32_t width, uint32_t height) {
    return static_cast<uint64_t>(width) * height * 3 / 2;
}

/// Frames in a clip file of `file_bytes`, or 0 when the size is not a whole
/// number of frames or the geometry is invalid (zero or odd).
uint64_t reference_clip_frames(uint64_t file_bytes, uint32_t width, uint32_t height);

/// Due time of frame `n` (counted from the first frame, over loops) in
/// microseconds: start + n * 1e6 / fps, exact in integer arithmetic.
constexpr uint64_t reference_due_us(uint64_t start_us, uint64_t n, uint32_t fps) {
    return start_us + (n * 1'000'000ULL) / (fps == 0 ? 60 : fps);
}

/// Pack one I420 frame as NV12: copy Y, interleave U and V.
void reference_i420_to_nv12(const uint8_t* i420, uint32_t width, uint32_t height, uint8_t* nv12);

/// What the source did so far, for the harness.
struct ReferenceInfo {
    uint64_t start_us         = 0; ///< due time of frame 0, steady clock µs
    uint64_t frames_delivered = 0; ///< frames handed to the callback
    uint64_t late_frames      = 0; ///< handed over more than 2 ms after due
    uint64_t max_late_us      = 0;
    uint32_t clip_frames      = 0;
    uint32_t fps              = 0;
    uint32_t loops            = 0; ///< completed passes over the clip
};

#ifdef _WIN32
class ReferenceCapture : public CaptureSource {
public:
    ReferenceCapture();
    ~ReferenceCapture() override;

    bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) override;
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width()  const override;
    uint32_t height() const override;
    const char* backend_name() const override { return "reference"; }
    bool failed() const override { return failed_.load(std::memory_order_relaxed); }

    ReferenceInfo info() const;

    // Stage 7: the clip's game audio (ReferenceClip::audio_path). Chunk j
    // of 10 ms is captured at start + j * 10 ms (the video's clock, frame 0
    // at start) and handed over 10 ms later, when it is complete, as a
    // capture device does. The audio loops with the clip.
    bool provides_timed_audio() const override;
    void set_timed_audio_callback(TimedAudioCallback cb) override;

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;  // the COM objects, the file and the thread
    std::atomic<bool> failed_{false};
};
#endif

} // namespace mello::video
