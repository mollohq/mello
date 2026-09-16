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
    Hook,        // the m3llo game capture hook, inside the game process
    Dxgi,        // desktop duplication of the game window's monitor
    WgcWindow,   // Windows Graphics Capture of the game window
    WgcMonitor,  // Windows Graphics Capture of the game window's monitor
};

const char* ladder_step_name(LadderStep step);

/// Pure ladder decisions, split out so they are testable without a GPU.
namespace ladder {
/// Order of capture methods for a process.
///
/// Window capture first, always. Measured against Unigine Heaven on
/// 2026-09-16: desktop duplication delivered one frame and then nothing for a
/// fullscreen game, wedged inside the display driver so its thread could not be
/// stopped, and left the D3D11 device unusable for the next method. Window
/// capture ran the same game at 48 fps with a 1 ms present-to-capture delay.
/// Desktop duplication stays last, as a fallback for cases window capture
/// cannot serve.
///
/// `allow_hook` puts the game capture hook first. It is the only method that
/// sees an exclusive-fullscreen game, and the only one that needs permission:
/// the caller passes the catalogue and backend decision, and libmello runs its
/// own run-time checks before it injects anything (plan 3.6).
std::vector<LadderStep> initial_order(bool allow_hook);

/// A method that delivered no frame at all by this point has failed.
static constexpr uint64_t kFirstFrameDeadlineUs = 2'000'000;
/// A continuous method must deliver at least `kProbationFrames` by this point.
/// Desktop duplication hands over one initial desktop image and then goes
/// silent under a game, which a first-frame test alone accepts forever
/// (measured against Unigine Heaven on 2026-09-16).
static constexpr uint64_t kProbationUs = 3'000'000;
static constexpr uint64_t kProbationFrames = 3;

/// True when this method delivers a frame for every change on screen.
///
/// Desktop duplication does: a game that presents keeps it busy, so silence
/// means it is blind. Windows Graphics Capture does not: it delivers a frame
/// only when the captured content changes, so a paused game is silent on a
/// method that works perfectly.
bool expects_continuous_frames(LadderStep step);

/// How long a method may wait while the game itself has drawn nothing.
///
/// A person starts a stream and then goes to the game: switching windows,
/// loading a level and taking fullscreen all take seconds. Two seconds of
/// silence in that window means nothing, and moving the ladder on because of it
/// trades the right method for one that shows the desktop (2026-09-16). Only
/// the hook can report this, because only the hook sees the game's presents.
/// The wait is bounded because a game that draws through an API the hook does
/// not cover is silent for the same reason and will stay silent.
static constexpr uint64_t kWaitingForGameUs = 15'000'000;

/// True when the active method has had its chance and is not delivering video.
///
/// Applies only while the game can present. Silence from a method that is
/// delivering normally is never a failure: a paused game and an idle desktop
/// are quiet and healthy.
///
/// `waiting_for_the_game` is the backend saying the game has drawn nothing at
/// all. Then the deadline is `kWaitingForGameUs`, not the first-frame one: the
/// method is not the problem.
bool startup_failed(bool continuous, uint64_t frames_since_step_start,
                    uint64_t step_started_us, uint64_t now_us,
                    bool waiting_for_the_game = false);

/// Wait between ladder passes when no method delivered. A visible game that
/// renders nothing (paused, or a benchmark waiting for input) looks exactly
/// like a blind capture method, so the ladder retries instead of churning.
static constexpr uint64_t kRetryPassUs = 30'000'000;

/// True when a failed ladder pass should be reported to the user.
///
/// Only exclusive fullscreen is proof that the game renders while every method
/// sees nothing. Without that proof a quiet stream is not an error: it is a
/// paused game, and telling the user to change a setting would be wrong.
bool should_report_failure(bool target_in_exclusive_fullscreen);
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
    CaptureState state() const override;
    bool stop_timed_out() const override { return stop_timed_out_.load(std::memory_order_relaxed); }
    std::string method_history() const override;
    void set_present_delay_histogram(PresentDelayHistogram* hist) override;

private:
    void monitor_thread();

    /// Build and initialize (not start) the backend for one ladder step.
    std::unique_ptr<CaptureSource> make_step(LadderStep step, HWND hwnd) const;
    /// Goes back to the first method in the ladder, on evidence that it can
    /// work now. The ladder never walks back up by itself.
    void restart_from_best(const char* reason);
    /// Replace the active backend with ladder step `index` and start it.
    ///
    /// Runs backend calls without `swap_mutex_` held: starting or stopping a
    /// backend can block for seconds inside the display driver, and telemetry
    /// callers (`backend_name`, `method_history`) must never wait for that.
    /// Serialized by `ladder_op_mutex_`. Returns false when the step cannot
    /// initialize or start.
    bool activate(size_t index, const char* reason);
    /// Move to the next ladder step that starts. Marks the ladder exhausted
    /// when none does.
    void advance(const char* reason);
    void note_history(const std::string& entry);

    bool start_deferred();

    uint32_t                         pid_ = 0;
    GraphicsDevice                   device_{};
    FrameCallback                    callback_;
    FrameCallback                    counting_callback_;
    PresentDelayHistogram*           delay_hist_ = nullptr;
    uint32_t                         target_fps_ = 60;

    std::unique_ptr<CaptureSource>   active_;
    // Guards the fields below. Held only for short reads and pointer swaps,
    // never across a backend call.
    mutable std::mutex               swap_mutex_;
    // Serializes ladder operations (monitor thread against stop()).
    std::mutex                       ladder_op_mutex_;
    std::thread                      monitor_thread_;
    std::atomic<bool>                running_{false};

    // Set when a hot-swap occurs so the pipeline can request a keyframe
    std::atomic<bool>                swap_occurred_{false};

    // Ladder state. `ladder_`, `step_index_`, `step_started_us_` and
    // `history_` are guarded by swap_mutex_. Frames are counted on the capture
    // thread, so the counter is atomic.
    std::vector<LadderStep>          ladder_;
    // The caller's decision for this game, from the catalogue and the backend.
    bool                             allow_hook_ = false;
    size_t                           step_index_ = 0;
    uint64_t                         step_started_us_ = 0;
    std::atomic<uint64_t>            step_frames_{0};
    // A full ladder pass delivered nothing. Internal: drives the retry timer.
    std::atomic<bool>                pass_failed_{false};
    // Reported to the user through failed(): a pass failed and the game is in
    // exclusive fullscreen, which no capture method here can see.
    std::atomic<bool>                exhausted_{false};
    uint64_t                         next_pass_us_ = 0;
    std::atomic<bool>                stop_timed_out_{false};
    // An abandoned capture thread keeps the D3D11 device busy inside the
    // driver: the next method blocks on the same device. Once this is set the
    // ladder stops trying.
    std::atomic<bool>                device_poisoned_{false};
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
