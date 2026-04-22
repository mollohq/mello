#pragma once
#include "capture_source.hpp"

#ifdef _WIN32
#include <thread>
#include <atomic>
#include <mutex>

namespace mello::video {

class ProcessCapture : public CaptureSource {
public:
    bool initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) override;
    bool start(uint32_t target_fps, FrameCallback callback) override;
    void stop() override;

    uint32_t width()  const override;
    uint32_t height() const override;
    const char* backend_name() const override;

    bool get_cursor(CursorData& out) override;
    bool consume_swap_event() override;

private:
    void monitor_thread();
    bool swap_to_dxgi();
    bool swap_to_wgc();

    bool start_deferred();

    uint32_t                         pid_ = 0;
    GraphicsDevice                   device_{};
    FrameCallback                    callback_;
    uint32_t                         target_fps_ = 60;

    std::unique_ptr<CaptureSource>   active_;
    mutable std::mutex               swap_mutex_;
    std::thread                      monitor_thread_;
    std::atomic<bool>                running_{false};

    // Set when a hot-swap occurs so the pipeline can request a keyframe
    std::atomic<bool>                swap_occurred_{false};

    // Deferred start: window was minimized at init time; we store the hwnd
    // and restored dimensions, then poll for restore in monitor_thread.
    HWND                             deferred_hwnd_ = nullptr;
    uint32_t                         deferred_w_ = 0;
    uint32_t                         deferred_h_ = 0;
};

/// Check whether a process currently owns a DXGI output (exclusive fullscreen).
int query_exclusive_fullscreen_output(uint32_t pid);

/// Find the main window HWND for a process.
HWND find_main_window(uint32_t pid);

} // namespace mello::video
#endif
