#pragma once
#include "capture_source.hpp"
#include "process_liveness.hpp"

#ifdef _WIN32
#include <thread>
#include <atomic>
#include <condition_variable>
#include <functional>
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
/// the caller passes the backend policy decision, and libmello runs its own
/// run-time checks before it injects anything (streaming-reliability plan
/// section 8).
std::vector<LadderStep> initial_order(bool allow_hook);

/// A method that delivered no frame at all by this point has failed.
static constexpr uint64_t kFirstFrameDeadlineUs = 2'000'000;
/// A continuous method must deliver at least `kProbationFrames` by this point.
/// Desktop duplication hands over one initial desktop image and then goes
/// silent under a game, which a first-frame test alone accepts forever
/// (measured against Unigine Heaven on 2026-09-16).
static constexpr uint64_t kProbationUs = 3'000'000;
static constexpr uint64_t kProbationFrames = 3;

/// What to tell the user about a capture, from what the ladder knows.
///
/// Pure, because it is the sentence a viewer reads: a wrong answer here shows a
/// pause card over a live game, or leaves a still picture unexplained.
///
/// `target_in_exclusive_fullscreen` is proven by the OS (QUNS plus the
/// foreground pid): the game owns the scanout, so a screen-level method is
/// blind whatever frames the compositor still delivers. That proof outranks
/// every wait, the same way `exhausted` does.
CaptureState capture_state_for(bool exhausted, bool deferred_start, bool waiting_for_the_game,
                               bool captures_while_minimized, bool target_can_present,
                               bool target_in_exclusive_fullscreen);

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

/// Wait between attempts to go back to the best method while a game is in
/// exclusive fullscreen.
///
/// The ladder only walks down by itself, because whatever it settles on keeps
/// delivering. That is the wrong outcome for a fullscreen game: screen capture
/// can deliver the desktop with the game as a small window inside it, forever.
/// A failed attempt costs nothing: the better method starts beside the current
/// one, and the current one keeps the stream until the better one delivers its
/// first frame (`ProcessCapture::activate`).
static constexpr uint64_t kBestMethodRetryUs = 30'000'000;

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
    CursorMode cursor_mode() const override;
    bool consume_swap_event() override;
    bool failed() const override { return exhausted_.load(std::memory_order_relaxed); }
    CaptureState state() const override;
    bool target_exited() const override { return target_liveness_.exited(); }
    bool stop_timed_out() const override { return stop_timed_out_.load(std::memory_order_relaxed); }
    std::string method_history() const override;
    void set_present_delay_histogram(PresentDelayHistogram* hist) override;

    // --- Test seams. Production code does not call these. ---

    /// Builds and initializes (not starts) the backend for one ladder step.
    using StepFactory = std::function<std::unique_ptr<CaptureSource>(LadderStep step)>;
    /// Tests only. Uses `factory` instead of the real backends and the window
    /// lookup, makes `ladder[start_index]` the active backend, and runs no
    /// monitor thread, so the test makes every ladder move itself.
    bool use_steps_for_test(std::vector<LadderStep> ladder, StepFactory factory,
                            size_t start_index);
    /// Tests only. One ladder move, as the monitor thread makes it.
    bool move_for_test(size_t index, bool keep_old_until_first_frame) {
        return activate(index, "test", keep_old_until_first_frame);
    }

private:
    void monitor_thread();
    // The body of monitor_thread, so the thread entry point can contain any
    // exception the capture backends throw.
    void monitor_pass();

    /// Build and initialize (not start) the backend for one ladder step.
    std::unique_ptr<CaptureSource> make_step(LadderStep step, HWND hwnd) const;
    /// The test factory when there is one, else `make_step` on the game window.
    std::unique_ptr<CaptureSource> build_step(LadderStep step) const;
    /// Goes back to the first method in the ladder, on evidence that it can
    /// work now. The ladder never walks back up by itself.
    void restart_from_best(const char* reason);
    /// Replace the active backend with ladder step `index`.
    ///
    /// The next backend starts before the old one stops. With
    /// `keep_old_until_first_frame`, the old backend keeps the stream until
    /// the next one delivers its first frame (bounded by
    /// `ladder::kFirstFrameDeadlineUs`); when it does not, the next backend
    /// stops and the old one never stopped. Without it, the swap happens as
    /// soon as the next backend starts: the old backend is not delivering, so
    /// there is nothing to keep.
    ///
    /// Runs backend calls without `swap_mutex_` held: starting or stopping a
    /// backend can block for seconds inside the display driver, and telemetry
    /// callers (`backend_name`, `method_history`) must never wait for that.
    /// Serialized by `ladder_op_mutex_`. Returns false when the step cannot
    /// initialize or start, or delivers no first frame in time.
    bool activate(size_t index, const char* reason, bool keep_old_until_first_frame);
    /// The callback a backend started with `token` delivers through. Only the
    /// active token reaches the pipeline; a candidate's frames only count
    /// towards its first frame.
    FrameCallback delivery_for(uint64_t token);
    /// Move to the next ladder step that starts. Marks the ladder exhausted
    /// when none does.
    void advance(const char* reason);
    void note_history(const std::string& entry);

    bool start_deferred();

    uint32_t                         pid_ = 0;
    // Tracks the captured process object itself, not the pid, so pid reuse
    // cannot confuse it. Refreshed on every monitor pass; sticky once the
    // process is known dead. Ends the stream via get_host_telemetry, where
    // CaptureState only pauses it.
    // Written on the monitor thread, read on any thread.
    ProcessLiveness                  target_liveness_;
    GraphicsDevice                   device_{};
    FrameCallback                    callback_;
    PresentDelayHistogram*           delay_hist_ = nullptr;

    // Frame delivery during a swap. Two backends run at the same time while
    // the next one starts, and only one of them may reach the pipeline: the
    // pipeline's frame path is single-threaded. `deliver_mutex_` serializes
    // delivery and guards `active_token_`. It is taken before `swap_mutex_`
    // (the pipeline reads the cursor inside a delivery), never after it.
    std::mutex                       deliver_mutex_;
    uint64_t                         active_token_ = 0;
    std::atomic<uint64_t>            next_token_{1};
    // The candidate of a swap in progress, and whether it has delivered.
    std::mutex                       candidate_mutex_;
    std::condition_variable          candidate_cv_;
    uint64_t                         candidate_token_ = 0;
    bool                             candidate_delivered_ = false;

    // Tests only (use_steps_for_test).
    StepFactory                      test_factory_;
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
    uint64_t                         next_best_retry_us_ = 0;
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
