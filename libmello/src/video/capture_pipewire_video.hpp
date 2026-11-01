#pragma once
#if defined(__linux__)
#include "capture_source.hpp"

#include <memory>

namespace mello::video {

/// Linux screen capture: the PipeWire video stream of a ScreenCast portal
/// session. The client runs the portal (the desktop's dialog picks a monitor
/// or a window) and passes the session's PipeWire socket and node; this
/// connects to that node and hands each picture to the pipeline as a
/// CpuFrame (cpu_frame.hpp). Shared memory buffers; the pointer is in the
/// picture (the portal's embedded cursor mode).
///
/// Threads: initialize, start and stop on the caller's thread. The frame
/// callback runs on this capture's own PipeWire loop thread; the pipeline
/// encodes there, as on macOS. A slow encode makes PipeWire skip pictures,
/// never queue them.
class PipeWireVideoCapture : public CaptureSource {
public:
    PipeWireVideoCapture();
    ~PipeWireVideoCapture() override;

    /// Connects to the portal's node and waits up to 3 s for the format, so
    /// width() and height() are known when it returns true.
    bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) override;
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width() const override;
    uint32_t height() const override;
    const char* backend_name() const override { return "PipeWire"; }
    CursorMode cursor_mode() const override { return CursorMode::InFrame; }
    /// True once the stream ended for good: the portal session closed (the
    /// user stopped sharing in the desktop) or PipeWire reported an error.
    bool failed() const override;

    struct Impl;

private:
    std::unique_ptr<Impl> impl_;
};

} // namespace mello::video
#endif
