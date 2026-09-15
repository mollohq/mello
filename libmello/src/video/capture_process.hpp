#pragma once
#include "capture_source.hpp"

#ifdef _WIN32
#include <thread>
#include <atomic>
#include <mutex>
#include <string>
#include <vector>

namespace mello::video {

/// Capture methods the process capture ladder can use, in no fixed order.
enum class LadderStep : uint8_t {
    Dxgi,        // desktop duplication of the game window's monitor
    WgcWindow,   // Windows Graphics Capture of the game window
    WgcMonitor,  // Windows Graphics Capture of the game window's monitor
};

const char* ladder_step_name(LadderStep step);

/// Pure ladder decisions, split out so they are testable without a GPU.
namespace ladder {
/// Order of capture methods for a process. Today this keeps the pre-ladder
/// preference (DXGI for a window that covers its monitor, WGC otherwise); the
/// DXGI vs WGC benchmark in the streaming reliability plan decides the final
/// order.
std::vector<LadderStep> initial_order(bool window_covers_monitor);

/// A method that delivered no frame since it started has failed once this
/// deadline passes. Silence after the first frame is never a failure.
static constexpr uint64_t kFirstFrameDeadlineUs = 2'000'000;

/// True when the active method has had its chance and delivered nothing.
bool first_frame_overdue(uint64_t frames_since_step_start, uint64_t step_started_us,
                         uint64_t now_us);
}  // namespace ladder

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
    bool failed() const override { return exhausted_.load(std::memory_order_relaxed); }
    std::string method_history() const override;
    void set_present_delay_histogram(PresentDelayHistogram* hist) override;

private:
    void monitor_thread();

    /// Build and initialize (not start) the backend for one ladder step.
    std::unique_ptr<CaptureSource> make_step(LadderStep step, HWND hwnd) const;
    /// Replace the active backend with ladder step `index` and start it.
    /// Caller holds swap_mutex_. Returns false and keeps the old backend when
    /// the step cannot initialize or start.
    bool activate_locked(size_t index, const char* reason);
    /// Move to the next ladder step that starts. Marks the ladder exhausted
    /// when none does. Caller holds swap_mutex_.
    void advance_locked(const char* reason);
    void note_history_locked(const std::string& entry);

    bool start_deferred();

    uint32_t                         pid_ = 0;
    GraphicsDevice                   device_{};
    FrameCallback                    callback_;
    FrameCallback                    counting_callback_;
    PresentDelayHistogram*           delay_hist_ = nullptr;
    uint32_t                         target_fps_ = 60;

    std::unique_ptr<CaptureSource>   active_;
    mutable std::mutex               swap_mutex_;
    std::thread                      monitor_thread_;
    std::atomic<bool>                running_{false};

    // Set when a hot-swap occurs so the pipeline can request a keyframe
    std::atomic<bool>                swap_occurred_{false};

    // Ladder state. `ladder_`, `step_index_`, `step_started_us_` and
    // `history_` are guarded by swap_mutex_. Frames are counted on the capture
    // thread, so the counter is atomic.
    std::vector<LadderStep>          ladder_;
    size_t                           step_index_ = 0;
    uint64_t                         step_started_us_ = 0;
    std::atomic<uint64_t>            step_frames_{0};
    std::atomic<bool>                exhausted_{false};
    std::string                      history_;

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
