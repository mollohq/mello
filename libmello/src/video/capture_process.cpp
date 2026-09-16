#ifdef _WIN32
#include "capture_process.hpp"
#include "capture_dxgi.hpp"
#include "capture_hook.hpp"
#include "hook_policy.hpp"
#include "capture_wgc.hpp"
#include "../util/log.hpp"
#include <dxgi.h>
#include <wrl/client.h>
#include <shlobj.h>
#include <chrono>

using Microsoft::WRL::ComPtr;

namespace mello::video {

static constexpr const char* TAG = "video/capture";

// --- Helpers ---

struct EnumWindowData {
    uint32_t pid;
    HWND     result;
    int64_t  best_area;
};

// Use GetWindowPlacement to get the *restored* bounds even when minimized/tabbed-out.
static int64_t get_restored_area(HWND hwnd) {
    WINDOWPLACEMENT wp{};
    wp.length = sizeof(wp);
    if (GetWindowPlacement(hwnd, &wp)) {
        const RECT& r = wp.rcNormalPosition;
        return static_cast<int64_t>(r.right - r.left) * (r.bottom - r.top);
    }
    RECT r{};
    GetWindowRect(hwnd, &r);
    return static_cast<int64_t>(r.right - r.left) * (r.bottom - r.top);
}

static BOOL CALLBACK enum_window_proc(HWND hwnd, LPARAM lParam) {
    auto* data = reinterpret_cast<EnumWindowData*>(lParam);
    DWORD wnd_pid = 0;
    GetWindowThreadProcessId(hwnd, &wnd_pid);
    if (wnd_pid != data->pid) return TRUE;

    // Skip tool windows (tray icons, floating toolbars)
    LONG ex_style = GetWindowLong(hwnd, GWL_EXSTYLE);
    if ((ex_style & WS_EX_TOOLWINDOW) && !(ex_style & WS_EX_APPWINDOW)) return TRUE;

    // Skip windows with no title (internal helper windows)
    char title[128] = {};
    if (GetWindowTextA(hwnd, title, sizeof(title)) == 0) return TRUE;

    int64_t area = get_restored_area(hwnd);

    MELLO_LOG_DEBUG(TAG, "find_main_window: pid=%u hwnd=%p restored_area=%lld visible=%d "
        "exstyle=0x%08X title=\"%.60s\"",
        data->pid, hwnd, (long long)area,
        (int)IsWindowVisible(hwnd), (unsigned)ex_style, title);

    if (area > data->best_area) {
        data->best_area = area;
        data->result = hwnd;
    }
    return TRUE;
}

HWND find_main_window(uint32_t pid) {
    EnumWindowData data{pid, nullptr, 0};
    EnumWindows(enum_window_proc, reinterpret_cast<LPARAM>(&data));
    static std::atomic<HWND> last_selected{nullptr};
    if (data.result) {
        if (last_selected.exchange(data.result) != data.result) {
            int64_t area = get_restored_area(data.result);
            char title[128] = {};
            GetWindowTextA(data.result, title, sizeof(title));
            MELLO_LOG_INFO(TAG,
                "find_main_window: pid=%u selected hwnd=%p restored_area=%lld title=\"%.60s\"",
                pid, data.result, (long long)area, title);
        }
    } else if (last_selected.exchange(nullptr) != nullptr) {
        MELLO_LOG_WARN(TAG, "find_main_window: pid=%u no suitable window found", pid);
    }
    return data.result;
}

static bool is_likely_fullscreen(HWND hwnd, HMONITOR mon) {
    if (!hwnd || !mon) return false;

    MONITORINFO mi{};
    mi.cbSize = sizeof(mi);
    if (!GetMonitorInfo(mon, &mi)) return false;

    // Use restored bounds so minimized/tabbed-out games still match.
    WINDOWPLACEMENT wp{};
    wp.length = sizeof(wp);
    RECT wr{};
    if (GetWindowPlacement(hwnd, &wp)) {
        wr = wp.rcNormalPosition;
    } else if (!GetWindowRect(hwnd, &wr)) {
        return false;
    }

    int64_t mon_w = mi.rcMonitor.right  - mi.rcMonitor.left;
    int64_t mon_h = mi.rcMonitor.bottom - mi.rcMonitor.top;
    int64_t win_w = wr.right  - wr.left;
    int64_t win_h = wr.bottom - wr.top;

    // Window covers >= 90% of the monitor in both dimensions (borderless FS)
    bool covers_monitor = (win_w * 10 >= mon_w * 9) && (win_h * 10 >= mon_h * 9);

    LONG style = GetWindowLong(hwnd, GWL_STYLE);
    bool no_chrome = (style & WS_OVERLAPPEDWINDOW) == 0;

    MELLO_LOG_DEBUG(TAG, "is_likely_fullscreen: hwnd=%p mon=%dx%d win=%dx%d "
        "covers=%d no_chrome=%d minimized=%d",
        hwnd, (int)mon_w, (int)mon_h, (int)win_w, (int)win_h,
        (int)covers_monitor, (int)no_chrome,
        (int)(wp.showCmd == SW_SHOWMINIMIZED));

    return covers_monitor && no_chrome;
}

static bool output_index_for_monitor_on_device(
    ID3D11Device* device,
    HMONITOR monitor,
    uint32_t* out_idx
) {
    if (!device || !monitor || !out_idx) return false;

    ComPtr<IDXGIDevice> dxgi_device;
    if (FAILED(device->QueryInterface(IID_PPV_ARGS(&dxgi_device)))) return false;

    ComPtr<IDXGIAdapter> adapter;
    if (FAILED(dxgi_device->GetAdapter(&adapter))) return false;

    ComPtr<IDXGIOutput> output;
    for (UINT oi = 0; adapter->EnumOutputs(oi, &output) == S_OK; ++oi) {
        DXGI_OUTPUT_DESC desc{};
        if (SUCCEEDED(output->GetDesc(&desc)) && desc.Monitor == monitor) {
            *out_idx = oi;
            return true;
        }
        output.Reset();
    }
    return false;
}

static bool resolve_process_dxgi_output(
    uint32_t pid,
    ID3D11Device* device,
    uint32_t* out_output_index,
    HWND* out_hwnd
) {
    if (!out_output_index) return false;
    HWND hwnd = find_main_window(pid);
    if (out_hwnd) *out_hwnd = hwnd;
    if (!hwnd) return false;

    HMONITOR mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    if (!mon) return false;
    if (!is_likely_fullscreen(hwnd, mon)) return false;

    return output_index_for_monitor_on_device(device, mon, out_output_index);
}

int query_exclusive_fullscreen_output(uint32_t pid) {
    // Legacy helper retained for compatibility. Returns output index in
    // whichever adapter the process monitor maps to; ProcessCapture now uses
    // adapter-aware mapping tied to its active D3D11 device.
    HWND hwnd = find_main_window(pid);
    if (!hwnd) return -1;

    HMONITOR mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
    if (!mon) return -1;
    if (!is_likely_fullscreen(hwnd, mon)) return -1;

    ComPtr<IDXGIFactory1> factory;
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) return -1;

    UINT adapter_idx = 0;
    ComPtr<IDXGIAdapter1> adapter;
    while (factory->EnumAdapters1(adapter_idx++, &adapter) == S_OK) {
        UINT output_idx = 0;
        ComPtr<IDXGIOutput> output;
        while (adapter->EnumOutputs(output_idx++, &output) == S_OK) {
            DXGI_OUTPUT_DESC desc{};
            if (SUCCEEDED(output->GetDesc(&desc)) && desc.Monitor == mon) {
                return static_cast<int>(output_idx - 1);
            }
            output.Reset();
        }
        adapter.Reset();
    }
    return -1;
}

// --- Ladder decisions ---

static uint64_t ladder_now_us() {
    return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::microseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count());
}

const char* ladder_step_name(LadderStep step) {
    switch (step) {
        case LadderStep::Hook:       return "Hook";
        case LadderStep::Dxgi:       return "DXGI-DDI";
        case LadderStep::WgcWindow:  return "WGC";
        case LadderStep::WgcMonitor: return "WGC-Monitor";
    }
    return "unknown";
}

namespace ladder {

std::vector<LadderStep> initial_order(bool allow_hook) {
    if (allow_hook) {
        return {LadderStep::Hook, LadderStep::WgcWindow, LadderStep::WgcMonitor,
                LadderStep::Dxgi};
    }
    return {LadderStep::WgcWindow, LadderStep::WgcMonitor, LadderStep::Dxgi};
}

bool should_report_failure(bool target_in_exclusive_fullscreen) {
    return target_in_exclusive_fullscreen;
}

CaptureState capture_state_for(bool exhausted, bool deferred_start, bool waiting_for_the_game,
                               bool captures_while_minimized, bool target_can_present) {
    if (exhausted) return CaptureState::Failed;
    // No capture has started at all: the game was minimized when the stream
    // began, and nothing can capture a minimized window from outside.
    if (deferred_start) return CaptureState::WaitingMinimized;
    if (waiting_for_the_game) return CaptureState::WaitingForGame;
    // The hook takes the frame inside the game, so a minimized game keeps
    // streaming and the window's state says nothing about what viewers see.
    if (captures_while_minimized) return CaptureState::Capturing;
    if (!target_can_present) return CaptureState::WaitingMinimized;
    return CaptureState::Capturing;
}

bool expects_continuous_frames(LadderStep step) {
    return step == LadderStep::Dxgi;
}

bool startup_failed(bool continuous, uint64_t frames_since_step_start,
                    uint64_t step_started_us, uint64_t now_us,
                    bool waiting_for_the_game) {
    if (now_us < step_started_us) return false;
    const uint64_t elapsed = now_us - step_started_us;
    if (frames_since_step_start == 0) {
        // The game has drawn nothing at all, so no method would have anything
        // to show. Wait for the person to reach their game.
        if (waiting_for_the_game) return elapsed >= kWaitingForGameUs;
        return elapsed >= kFirstFrameDeadlineUs;
    }
    if (!continuous) return false;
    return frames_since_step_start < kProbationFrames && elapsed >= kProbationUs;
}

}  // namespace ladder

/// True when the game has a window that can present: it exists and is not
/// minimized. A minimized game produces no frames through any capture method,
/// so a silent capture then says nothing about the method.
static bool target_can_present(uint32_t pid) {
    HWND hwnd = find_main_window(pid);
    if (!hwnd || !IsWindow(hwnd)) return false;
    WINDOWPLACEMENT wp{};
    wp.length = sizeof(wp);
    if (GetWindowPlacement(hwnd, &wp) && wp.showCmd == SW_SHOWMINIMIZED) return false;
    return true;
}

/// True when the game window is the foreground window and Windows reports an
/// exclusive-fullscreen Direct3D application. Discord uses the same query to
/// detect exclusive fullscreen, which no screen-level capture method can see.
static bool process_in_exclusive_fullscreen(uint32_t pid) {
    QUERY_USER_NOTIFICATION_STATE state{};
    if (FAILED(SHQueryUserNotificationState(&state))) return false;
    if (state != QUNS_RUNNING_D3D_FULL_SCREEN) return false;
    DWORD fg_pid = 0;
    GetWindowThreadProcessId(GetForegroundWindow(), &fg_pid);
    return fg_pid == pid;
}

// --- ProcessCapture ---

std::unique_ptr<CaptureSource> ProcessCapture::make_step(LadderStep step, HWND hwnd) const {
    if (!hwnd) return nullptr;
    HMONITOR mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    switch (step) {
        case LadderStep::Hook: {
            // Every run-time check runs here, on every stream start, however
            // the catalogue classified this game.
            const hook::PolicyResult policy = hook::check_process(pid_, allow_hook_);
            if (!policy.allowed()) {
                MELLO_LOG_INFO(TAG, "ladder: no hook for pid=%u: %s%s%s", pid_,
                               hook::verdict_name(policy.verdict),
                               policy.detail.empty() ? "" : " - ",
                               policy.detail.c_str());
                return nullptr;
            }
            auto hooked = std::make_unique<HookCapture>();
            CaptureSourceDesc desc{};
            desc.mode = CaptureMode::Process;
            desc.pid = pid_;
            desc.allow_hook = true;
            if (!hooked->initialize(device_, desc)) return nullptr;
            return hooked;
        }
        case LadderStep::Dxgi: {
            uint32_t output = 0;
            if (!output_index_for_monitor_on_device(device_.d3d11(), mon, &output)) {
                MELLO_LOG_WARN(TAG, "ladder: no DXGI output on the encoder adapter for pid=%u", pid_);
                return nullptr;
            }
            auto dxgi = std::make_unique<DxgiCapture>();
            CaptureSourceDesc desc{};
            desc.mode = CaptureMode::Monitor;
            desc.monitor_index = output;
            if (!dxgi->initialize(device_, desc)) return nullptr;
            return dxgi;
        }
        case LadderStep::WgcWindow: {
            auto wgc = std::make_unique<WgcCapture>();
            CaptureSourceDesc desc{};
            desc.mode = CaptureMode::Window;
            desc.hwnd = hwnd;
            if (!wgc->initialize(device_, desc)) return nullptr;
            return wgc;
        }
        case LadderStep::WgcMonitor: {
            auto wgc = std::make_unique<WgcCapture>();
            if (!wgc->initialize_monitor(device_, mon)) return nullptr;
            return wgc;
        }
    }
    return nullptr;
}

void ProcessCapture::note_history(const std::string& entry) {
    // Bounded: the history rides in host telemetry, which has a size cap.
    static constexpr size_t kMaxHistory = 96;
    if (!history_.empty()) history_ += ";";
    history_ += entry;
    if (history_.size() > kMaxHistory) {
        history_.erase(0, history_.size() - kMaxHistory);
    }
}

void ProcessCapture::set_present_delay_histogram(PresentDelayHistogram* hist) {
    std::lock_guard<std::mutex> lock(swap_mutex_);
    delay_hist_ = hist;
    if (active_) active_->set_present_delay_histogram(hist);
}

std::string ProcessCapture::method_history() const {
    std::lock_guard<std::mutex> lock(swap_mutex_);
    return history_;
}

bool ProcessCapture::activate(size_t index, const char* reason) {
    std::lock_guard<std::mutex> op(ladder_op_mutex_);
    if (index >= ladder_.size()) return false;
    if (device_poisoned_.load(std::memory_order_relaxed)) return false;
    const LadderStep step = ladder_[index];

    // Take the old backend out under the short lock, then work on it without
    // holding it: stop() can block for up to its own deadline.
    std::unique_ptr<CaptureSource> old;
    std::string old_name;
    uint32_t old_w = 0, old_h = 0;
    {
        std::lock_guard<std::mutex> lock(swap_mutex_);
        old = std::move(active_);
        if (old) {
            old_name = old->backend_name();
            old_w = old->width();
            old_h = old->height();
        }
    }
    if (old_name.empty()) old_name = "none";

    if (old) {
        old->stop();
        if (old->stop_timed_out()) {
            // Abandoned, not stopped: leak it rather than destroy it under a
            // thread that still runs. That thread also keeps the D3D11 device
            // busy in the driver, so no further method can start on it.
            MELLO_LOG_ERROR(TAG,
                "ladder: %s abandoned during a swap; it is leaked and the capture device "
                "is unusable for the rest of this stream", old_name.c_str());
            (void)old.release();
            device_poisoned_ = true;
            stop_timed_out_ = true;
            return false;
        }
    }

    HWND hwnd = find_main_window(pid_);
    auto next = make_step(step, hwnd);
    if (!next) {
        MELLO_LOG_WARN(TAG, "ladder: %s unavailable for pid=%u", ladder_step_name(step), pid_);
    } else {
        next->set_present_delay_histogram(delay_hist_);
        if (!next->start(target_fps_, counting_callback_)) {
            MELLO_LOG_ERROR(TAG, "ladder: %s failed to start for pid=%u",
                            ladder_step_name(step), pid_);
            next.reset();
        }
    }

    if (!next) {
        // Put the old backend back so the stream keeps whatever it had.
        if (old) {
            if (!old->start(target_fps_, counting_callback_)) {
                MELLO_LOG_ERROR(TAG, "ladder: previous backend %s did not restart",
                                old_name.c_str());
            }
            std::lock_guard<std::mutex> lock(swap_mutex_);
            active_ = std::move(old);
        }
        return false;
    }

    if (old_w != 0 && (next->width() != old_w || next->height() != old_h)) {
        // The preprocessor and encoder keep their start size. A different size
        // may scale or crop; the probation rule moves on if it delivers
        // nothing.
        MELLO_LOG_WARN(TAG, "ladder: %s is %ux%u, stream started at %ux%u",
                       ladder_step_name(step), next->width(), next->height(), old_w, old_h);
    }

    {
        std::lock_guard<std::mutex> lock(swap_mutex_);
        step_frames_.store(0, std::memory_order_relaxed);
        active_ = std::move(next);
        step_index_ = index;
        step_started_us_ = ladder_now_us();
        note_history(std::string(ladder_step_name(step)) + ":" + reason);
    }
    swap_occurred_.store(true, std::memory_order_release);
    MELLO_LOG_WARN(TAG, "ladder: pid=%u %s -> %s (%s)", pid_, old_name.c_str(),
                   ladder_step_name(step), reason);
    return true;
}

// Goes back to the first method in the ladder. Used when something changes
// that makes a better method possible again: the ladder only ever walks
// downwards on its own, because the method it is on keeps delivering.
void ProcessCapture::restart_from_best(const char* reason) {
    if (ladder_.empty()) return;
    if (activate(0, reason)) return;
    // The best method still refuses. Whatever is running now keeps the stream;
    // rule 1 moves the ladder on if that stops delivering.
    MELLO_LOG_INFO(TAG, "ladder: %s is still unavailable for pid=%u", ladder_step_name(ladder_[0]),
                   pid_);
    std::lock_guard<std::mutex> lock(swap_mutex_);
    step_started_us_ = ladder_now_us();
}

void ProcessCapture::advance(const char* reason) {
    size_t from = 0;
    {
        std::lock_guard<std::mutex> lock(swap_mutex_);
        from = step_index_;
    }
    for (size_t i = from + 1; i < ladder_.size(); ++i) {
        if (activate(i, reason)) {
            pass_failed_.store(false, std::memory_order_relaxed);
            exhausted_.store(false, std::memory_order_relaxed);
            return;
        }
        if (device_poisoned_.load(std::memory_order_relaxed)) break;
    }

    // Every method has had its turn and none delivered.
    const bool poisoned = device_poisoned_.load(std::memory_order_relaxed);
    const bool exclusive = process_in_exclusive_fullscreen(pid_);
    const bool report = poisoned || ladder::should_report_failure(exclusive);
    const uint64_t now = ladder_now_us();

    if (!pass_failed_.exchange(true, std::memory_order_relaxed)) {
        std::lock_guard<std::mutex> lock(swap_mutex_);
        note_history(report ? "failed" : "quiet");
        if (report) {
            MELLO_LOG_ERROR(TAG,
                "ladder: every capture method failed for pid=%u (last: %s)%s",
                pid_, active_ ? active_->backend_name() : "none",
                poisoned
                    ? ". The capture device is stuck in the display driver; restart the stream."
                    : ". The game is in exclusive fullscreen, which no screen capture method can see.");
        } else {
            MELLO_LOG_WARN(TAG,
                "ladder: no method delivered for pid=%u and the game is not in exclusive "
                "fullscreen. It is probably rendering nothing. Retrying in %llu s.",
                pid_, (unsigned long long)(ladder::kRetryPassUs / 1'000'000));
        }
    }
    if (report) {
        exhausted_.store(true, std::memory_order_relaxed);
    }

    // Go back to the best method and wait. A paused game starts to render
    // again, and the best method must be the one that receives those frames.
    bool back_at_top = false;
    {
        std::lock_guard<std::mutex> lock(swap_mutex_);
        back_at_top = step_index_ == 0;
    }
    if (!back_at_top && !poisoned) {
        activate(0, report ? "retry best method" : "game is quiet");
    }

    std::lock_guard<std::mutex> lock(swap_mutex_);
    next_pass_us_ = now + ladder::kRetryPassUs;
    step_started_us_ = now;
}

bool ProcessCapture::initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) {
    pid_ = desc.pid;
    device_ = device;

    HWND hwnd = find_main_window(pid_);
    if (!hwnd) {
        MELLO_LOG_ERROR(TAG, "Process(pid=%u): no window found", pid_);
        return false;
    }

    allow_hook_ = desc.allow_hook;
    // The developer override names one executable and stands in for the backend
    // safe list, which does not exist yet. The ladder has to offer the step for
    // the override to reach the policy check at all.
    const bool hook_step_available =
        allow_hook_ || GetEnvironmentVariableA(hook::kDeveloperAllowVariable, nullptr, 0) > 0;
    ladder_ = ladder::initial_order(hook_step_available);

    // If the window is minimized (tabbed-out game), WGC would capture at the
    // tiny minimized size. Defer start until the window is restored -- the
    // monitor thread will poll and kick off capture once the game is active.
    WINDOWPLACEMENT wp{};
    wp.length = sizeof(wp);
    bool minimized = GetWindowPlacement(hwnd, &wp) && wp.showCmd == SW_SHOWMINIMIZED;
    if (minimized) {
        const RECT& r = wp.rcNormalPosition;
        deferred_hwnd_ = hwnd;
        deferred_w_ = static_cast<uint32_t>(r.right - r.left)  & ~1u;
        deferred_h_ = static_cast<uint32_t>(r.bottom - r.top)  & ~1u;
        if (deferred_w_ == 0 || deferred_h_ == 0) {
            deferred_w_ = 1920;
            deferred_h_ = 1080;
        }
        MELLO_LOG_INFO(TAG, "Source: Process(pid=%u) minimized -> deferred start "
            "(restored %ux%u, waiting for window restore)", pid_, deferred_w_, deferred_h_);
        return true;
    }

    for (size_t i = 0; i < ladder_.size(); ++i) {
        auto backend = make_step(ladder_[i], hwnd);
        if (backend) {
            // The pipeline attaches the histogram before this call, when there
            // is no backend yet. Hand it to the one we just built, or process
            // capture reports no delay at all.
            backend->set_present_delay_histogram(delay_hist_);
            std::lock_guard<std::mutex> lock(swap_mutex_);
            active_ = std::move(backend);
            step_index_ = i;
            note_history(std::string(ladder_step_name(ladder_[i])) + ":initial");
            MELLO_LOG_INFO(TAG, "Source: Process(pid=%u) ladder step %zu/%zu -> backend=%s",
                pid_, i + 1, ladder_.size(), ladder_step_name(ladder_[i]));
            return true;
        }
    }
    MELLO_LOG_ERROR(TAG, "Process(pid=%u): no capture method could initialize", pid_);
    return false;
}

bool ProcessCapture::start(uint32_t target_fps, FrameCallback callback) {
    if (running_.load()) return false;
    target_fps_ = target_fps;
    callback_ = callback;
    // Every backend delivers through this wrapper, so the ladder counts frames
    // per method without the backends knowing about it.
    counting_callback_ = [this](auto* tex, uint64_t ts) {
        step_frames_.fetch_add(1, std::memory_order_relaxed);
        if (exhausted_.load(std::memory_order_relaxed)) {
            exhausted_.store(false, std::memory_order_relaxed);
            MELLO_LOG_INFO(TAG, "ladder: frames arrived for pid=%u, capture recovered", pid_);
        }
        callback_(tex, ts);
    };
    swap_occurred_.store(false, std::memory_order_release);

    // Deferred mode: no active backend yet, monitor thread will start capture
    if (deferred_hwnd_) {
        running_ = true;
        monitor_thread_ = std::thread(&ProcessCapture::monitor_thread, this);
        return true;
    }

    {
        std::lock_guard<std::mutex> lock(swap_mutex_);
        step_frames_.store(0, std::memory_order_relaxed);
        if (!active_ || !active_->start(target_fps, counting_callback_)) return false;
        step_started_us_ = ladder_now_us();
    }

    running_ = true;
    monitor_thread_ = std::thread(&ProcessCapture::monitor_thread, this);
    return true;
}

void ProcessCapture::stop() {
    running_ = false;
    if (monitor_thread_.joinable()) monitor_thread_.join();

    std::lock_guard<std::mutex> op(ladder_op_mutex_);
    std::unique_ptr<CaptureSource> active;
    {
        std::lock_guard<std::mutex> lock(swap_mutex_);
        active = std::move(active_);
    }
    if (!active) return;
    active->stop();
    if (active->stop_timed_out()) {
        // Its thread is still running and may touch it. Leak it, and tell the
        // owner that this whole capture must not be destroyed.
        stop_timed_out_ = true;
        (void)active.release();
        MELLO_LOG_ERROR(TAG, "ladder: capture backend for pid=%u abandoned; it is leaked", pid_);
        return;
    }
    std::lock_guard<std::mutex> lock(swap_mutex_);
    active_ = std::move(active);
}

uint32_t ProcessCapture::width() const {
    std::lock_guard<std::mutex> lock(swap_mutex_);
    if (active_) return active_->width();
    return deferred_w_;
}

uint32_t ProcessCapture::height() const {
    std::lock_guard<std::mutex> lock(swap_mutex_);
    if (active_) return active_->height();
    return deferred_h_;
}

const char* ProcessCapture::backend_name() const {
    std::lock_guard<std::mutex> lock(swap_mutex_);
    return active_ ? active_->backend_name() : "none";
}

bool ProcessCapture::get_cursor(CursorData& out) {
    std::lock_guard<std::mutex> lock(swap_mutex_);
    return active_ ? active_->get_cursor(out) : false;
}

bool ProcessCapture::consume_swap_event() {
    return swap_occurred_.exchange(false, std::memory_order_acq_rel);
}

bool ProcessCapture::start_deferred() {
    HWND hwnd = deferred_hwnd_;
    if (!hwnd) return false;

    WINDOWPLACEMENT wp{};
    wp.length = sizeof(wp);
    if (!GetWindowPlacement(hwnd, &wp) || wp.showCmd == SW_SHOWMINIMIZED)
        return false;

    MELLO_LOG_INFO(TAG, "Process(pid=%u) deferred: window restored, starting capture", pid_);

    ladder_ = ladder::initial_order(
        allow_hook_ || GetEnvironmentVariableA(hook::kDeveloperAllowVariable, nullptr, 0) > 0);
    for (size_t i = 0; i < ladder_.size(); ++i) {
        if (activate(i, "deferred start")) {
            deferred_hwnd_ = nullptr;
            return true;
        }
    }
    MELLO_LOG_ERROR(TAG, "Deferred start failed for pid=%u: no capture method started", pid_);
    return false;
}

void ProcessCapture::monitor_thread() {
    // If deferred, poll until the window is restored before starting capture
    while (deferred_hwnd_ && running_.load()) {
        std::this_thread::sleep_for(std::chrono::milliseconds(250));
        if (!running_.load()) break;
        if (start_deferred()) break;
    }

    bool was_exclusive = process_in_exclusive_fullscreen(pid_);

    while (running_.load()) {
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
        if (!running_.load()) break;
        if (device_poisoned_.load(std::memory_order_relaxed)) continue;

        const uint64_t now = ladder_now_us();
        uint64_t step_frames = 0;
        uint64_t step_started = 0;
        std::string backend;
        bool has_active = false;
        bool continuous = false;
        bool waiting_for_the_game = false;
        {
            std::lock_guard<std::mutex> lock(swap_mutex_);
            has_active = active_ != nullptr;
            if (has_active) {
                backend = active_->backend_name();
                waiting_for_the_game = active_->waiting_for_the_game();
            }
            if (step_index_ < ladder_.size()) {
                continuous = ladder::expects_continuous_frames(ladder_[step_index_]);
            }
            step_frames = step_frames_.load(std::memory_order_relaxed);
            step_started = step_started_us_;
        }
        if (!has_active) continue;

        // 1. A method that is not delivering video has failed — but only while
        // the game can actually present. A minimized game renders nothing, so
        // its capture method is not at fault; restart the window when the game
        // comes back.
        // A pass that delivered nothing waits, on the best method, for the
        // game to render again. Frames while it waits end the wait.
        if (pass_failed_.load(std::memory_order_relaxed)) {
            if (step_frames >= ladder::kProbationFrames) {
                MELLO_LOG_INFO(TAG, "ladder: pid=%u delivers frames again on %s",
                               pid_, backend.c_str());
                pass_failed_.store(false, std::memory_order_relaxed);
                exhausted_.store(false, std::memory_order_relaxed);
                continue;
            }
            if (now < next_pass_us_) continue;
            // The wait is over: give every method another turn.
            pass_failed_.store(false, std::memory_order_relaxed);
            std::lock_guard<std::mutex> lock(swap_mutex_);
            step_frames_.store(0, std::memory_order_relaxed);
            step_started_us_ = now;
            continue;
        }
        if (ladder::startup_failed(continuous, step_frames, step_started, now,
                                   waiting_for_the_game)) {
            if (target_can_present(pid_)) {
                MELLO_LOG_WARN(TAG,
                               "ladder: %s delivered %llu frames in %llu ms for pid=%u%s",
                               backend.c_str(), (unsigned long long)step_frames,
                               (unsigned long long)((now - step_started) / 1000), pid_,
                               waiting_for_the_game ? " (the game drew nothing at all)" : "");
                advance(step_frames == 0 ? "no frames" : "too few frames");
            } else {
                std::lock_guard<std::mutex> lock(swap_mutex_);
                step_started_us_ = now;
            }
            continue;
        }

        // 2. A backend that stopped for good has failed.
        bool backend_failed = false;
        {
            std::lock_guard<std::mutex> lock(swap_mutex_);
            backend_failed = active_ && active_->failed();
        }
        if (backend_failed) {
            advance("backend failed");
            continue;
        }

        // 3. Evidence: the game entered exclusive fullscreen.
        //
        // This changes which method is right, so the ladder starts again from
        // the top rather than only restarting the current method's deadline. A
        // game that goes fullscreen while the stream runs on monitor capture
        // is the case: monitor capture keeps delivering, so nothing else would
        // ever move the ladder, and the viewer watches the whole desktop with
        // the game as a small window in it (2026-09-16).
        const bool exclusive = process_in_exclusive_fullscreen(pid_);
        if (exclusive && !was_exclusive) {
            was_exclusive = true;
            exhausted_.store(false, std::memory_order_relaxed);
            pass_failed_.store(false, std::memory_order_relaxed);

            size_t step = 0;
            {
                std::lock_guard<std::mutex> lock(swap_mutex_);
                step = step_index_;
                step_frames_.store(0, std::memory_order_relaxed);
                step_started_us_ = now;
            }
            MELLO_LOG_WARN(TAG, "ladder: pid=%u entered exclusive fullscreen%s", pid_,
                           step == 0 ? "" : "; trying the best method again");
            if (step != 0) restart_from_best("entered exclusive fullscreen");
            continue;
        }
        if (!exclusive) was_exclusive = false;
    }
}

const char* capture_state_name(CaptureState state) {
    switch (state) {
        case CaptureState::Capturing:        return "capturing";
        case CaptureState::WaitingMinimized: return "the game is minimized";
        case CaptureState::WaitingForGame:   return "the game has drawn nothing yet";
        case CaptureState::Failed:           return "capture failed";
    }
    return "unknown";
}

CaptureState ProcessCapture::state() const {
    bool deferred = false;
    bool waiting_for_the_game = false;
    bool captures_while_minimized = false;
    {
        std::lock_guard<std::mutex> lock(swap_mutex_);
        deferred = deferred_hwnd_ != nullptr;
        if (active_) {
            waiting_for_the_game = active_->waiting_for_the_game();
            captures_while_minimized = active_->captures_while_minimized();
        }
    }
    return ladder::capture_state_for(exhausted_.load(std::memory_order_relaxed), deferred,
                                     waiting_for_the_game, captures_while_minimized,
                                     target_can_present(pid_));
}

// --- Choosing what to capture ---

bool window_is_capturable(uint32_t client_width, uint32_t client_height,
                          const std::string& window_class) {
    // Direct3D 9 leaves this behind when a game takes exclusive fullscreen. It
    // is a real window with a real title, so it appears in any window picker,
    // and it never carries a single frame of the game.
    std::string lowered;
    lowered.reserve(window_class.size());
    for (char c : window_class) lowered.push_back(static_cast<char>(std::tolower(c)));
    if (lowered.rfind("d3dproxywindow", 0) == 0) return false;

    return client_width >= kMinEncodeWidth && client_height >= kMinEncodeHeight;
}

CaptureSourceDesc resolve_capture_target(const CaptureSourceDesc& desc) {
    if (desc.mode != CaptureMode::Window || !desc.hwnd) return desc;

    HWND hwnd = static_cast<HWND>(desc.hwnd);
    RECT client{};
    if (!GetClientRect(hwnd, &client)) return desc;
    const uint32_t width = static_cast<uint32_t>(client.right - client.left);
    const uint32_t height = static_cast<uint32_t>(client.bottom - client.top);

    char class_name[128]{};
    GetClassNameA(hwnd, class_name, sizeof(class_name));

    if (window_is_capturable(width, height, class_name)) return desc;

    DWORD pid = 0;
    GetWindowThreadProcessId(hwnd, &pid);
    if (pid == 0) return desc;

    MELLO_LOG_WARN(TAG,
        "Window(hwnd=%p) is %ux%u, class \"%s\": it cannot carry a stream. Capturing "
        "process %lu instead, which runs the whole capture ladder.",
        hwnd, width, height, class_name, pid);

    CaptureSourceDesc process = desc;
    process.mode = CaptureMode::Process;
    process.pid = static_cast<uint32_t>(pid);
    return process;
}

// --- Factory ---

std::unique_ptr<CaptureSource> create_capture_source(const CaptureSourceDesc& desc) {
    switch (desc.mode) {
        case CaptureMode::Monitor:
            if (desc.prefer_wgc) return std::make_unique<WgcCapture>();
            return std::make_unique<DxgiCapture>();
        case CaptureMode::Window:
            return std::make_unique<WgcCapture>();
        case CaptureMode::Process:
            return std::make_unique<ProcessCapture>();
    }
    return nullptr;
}

} // namespace mello::video
#endif
