#ifdef _WIN32
#include "capture_process.hpp"
#include "capture_dxgi.hpp"
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
        case LadderStep::Dxgi:       return "DXGI-DDI";
        case LadderStep::WgcWindow:  return "WGC";
        case LadderStep::WgcMonitor: return "WGC-Monitor";
    }
    return "unknown";
}

namespace ladder {

std::vector<LadderStep> initial_order(bool window_covers_monitor) {
    if (window_covers_monitor) {
        return {LadderStep::Dxgi, LadderStep::WgcWindow, LadderStep::WgcMonitor};
    }
    return {LadderStep::WgcWindow, LadderStep::WgcMonitor, LadderStep::Dxgi};
}

bool first_frame_overdue(uint64_t frames_since_step_start, uint64_t step_started_us,
                         uint64_t now_us) {
    if (frames_since_step_start > 0) return false;
    if (now_us < step_started_us) return false;
    return now_us - step_started_us >= kFirstFrameDeadlineUs;
}

}  // namespace ladder

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

void ProcessCapture::note_history_locked(const std::string& entry) {
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

bool ProcessCapture::activate_locked(size_t index, const char* reason) {
    if (index >= ladder_.size()) return false;
    const LadderStep step = ladder_[index];
    HWND hwnd = find_main_window(pid_);
    auto next = make_step(step, hwnd);
    if (!next) {
        MELLO_LOG_WARN(TAG, "ladder: %s unavailable for pid=%u", ladder_step_name(step), pid_);
        return false;
    }

    const char* old_name = active_ ? active_->backend_name() : "none";
    uint32_t old_w = active_ ? active_->width() : 0;
    uint32_t old_h = active_ ? active_->height() : 0;
    if (active_) active_->stop();

    step_frames_.store(0, std::memory_order_relaxed);
    next->set_present_delay_histogram(delay_hist_);
    if (!next->start(target_fps_, counting_callback_)) {
        MELLO_LOG_ERROR(TAG, "ladder: %s failed to start for pid=%u", ladder_step_name(step), pid_);
        if (active_) {
            if (!active_->start(target_fps_, counting_callback_)) {
                MELLO_LOG_ERROR(TAG, "ladder: previous backend %s did not restart", old_name);
            }
        }
        return false;
    }

    if (old_w != 0 && (next->width() != old_w || next->height() != old_h)) {
        // The preprocessor and encoder keep their start size. A different size
        // may scale or crop; the first-frame rule moves on if it delivers
        // nothing.
        MELLO_LOG_WARN(TAG, "ladder: %s is %ux%u, stream started at %ux%u",
                       ladder_step_name(step), next->width(), next->height(), old_w, old_h);
    }

    active_ = std::move(next);
    step_index_ = index;
    step_started_us_ = ladder_now_us();
    swap_occurred_.store(true, std::memory_order_release);
    note_history_locked(std::string(ladder_step_name(step)) + ":" + reason);
    MELLO_LOG_WARN(TAG, "ladder: pid=%u %s -> %s (%s)", pid_, old_name, ladder_step_name(step), reason);
    return true;
}

void ProcessCapture::advance_locked(const char* reason) {
    for (size_t i = step_index_ + 1; i < ladder_.size(); ++i) {
        if (activate_locked(i, reason)) {
            exhausted_.store(false, std::memory_order_relaxed);
            return;
        }
    }
    if (!exhausted_.exchange(true, std::memory_order_relaxed)) {
        note_history_locked("exhausted");
        MELLO_LOG_ERROR(TAG,
            "ladder: every capture method failed for pid=%u (last: %s). "
            "The game is probably in exclusive fullscreen.",
            pid_, active_ ? active_->backend_name() : "none");
    }
    // Keep the last method running: if the game leaves exclusive fullscreen
    // it may start delivering, which clears the exhausted state.
    step_started_us_ = ladder_now_us();
}

bool ProcessCapture::initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) {
    pid_ = desc.pid;
    device_ = device;

    HWND hwnd = find_main_window(pid_);
    if (!hwnd) {
        MELLO_LOG_ERROR(TAG, "Process(pid=%u): no window found", pid_);
        return false;
    }

    uint32_t fs_output = 0;
    const bool covers_monitor = resolve_process_dxgi_output(
        pid_, device_.d3d11(), &fs_output, nullptr);
    ladder_ = ladder::initial_order(covers_monitor);

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

    std::lock_guard<std::mutex> lock(swap_mutex_);
    for (size_t i = 0; i < ladder_.size(); ++i) {
        auto backend = make_step(ladder_[i], hwnd);
        if (backend) {
            active_ = std::move(backend);
            step_index_ = i;
            note_history_locked(std::string(ladder_step_name(ladder_[i])) + ":initial");
            MELLO_LOG_INFO(TAG, "Source: Process(pid=%u) ladder step %zu/%zu -> backend=%s%s",
                pid_, i + 1, ladder_.size(), ladder_step_name(ladder_[i]),
                covers_monitor ? " (window covers monitor)" : "");
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

    std::lock_guard<std::mutex> lock(swap_mutex_);
    if (active_) active_->stop();
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

    uint32_t fs_output = 0;
    const bool covers_monitor = resolve_process_dxgi_output(pid_, device_.d3d11(), &fs_output, nullptr);

    std::lock_guard<std::mutex> lock(swap_mutex_);
    ladder_ = ladder::initial_order(covers_monitor);
    for (size_t i = 0; i < ladder_.size(); ++i) {
        if (activate_locked(i, "deferred start")) {
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

    bool was_fullscreen = false;
    {
        std::lock_guard<std::mutex> lock(swap_mutex_);
        if (active_) {
            was_fullscreen = (std::string(active_->backend_name()) == "DXGI-DDI");
        }
    }
    bool was_exclusive = process_in_exclusive_fullscreen(pid_);

    while (running_.load()) {
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
        if (!running_.load()) break;

        const uint64_t now = ladder_now_us();
        std::lock_guard<std::mutex> lock(swap_mutex_);
        if (!active_) continue;

        // 1. A method that never delivered a first frame has failed.
        if (!exhausted_.load(std::memory_order_relaxed) &&
            ladder::first_frame_overdue(step_frames_.load(std::memory_order_relaxed),
                                        step_started_us_, now)) {
            advance_locked("no first frame within 2 s");
            continue;
        }

        // 2. A backend that stopped for good has failed.
        if (active_->failed()) {
            advance_locked("backend failed");
            continue;
        }

        // 3. Evidence: the game entered exclusive fullscreen. Rebuild the
        // current method and give it a new first-frame deadline; the ladder
        // moves on if it stays silent. Silence alone is never evidence.
        const bool exclusive = process_in_exclusive_fullscreen(pid_);
        if (exclusive && !was_exclusive) {
            was_exclusive = true;
            MELLO_LOG_WARN(TAG, "ladder: pid=%u entered exclusive fullscreen", pid_);
            exhausted_.store(false, std::memory_order_relaxed);
            activate_locked(step_index_, "entered exclusive fullscreen");
            continue;
        }
        if (!exclusive) was_exclusive = false;

        // 4. Evidence: the window started or stopped covering its monitor.
        uint32_t output_idx = 0;
        const bool is_fullscreen = resolve_process_dxgi_output(
            pid_, device_.d3d11(), &output_idx, nullptr);
        if (is_fullscreen == was_fullscreen) continue;

        // A minimized window has no real surface; WGC hands back the ~160x28
        // iconic size. Keep the current method until the window comes back.
        HWND hwnd = find_main_window(pid_);
        WINDOWPLACEMENT wp{};
        wp.length = sizeof(wp);
        if (!hwnd || (GetWindowPlacement(hwnd, &wp) && wp.showCmd == SW_SHOWMINIMIZED)) {
            continue;
        }

        const std::vector<LadderStep> order = ladder::initial_order(is_fullscreen);
        ladder_ = order;
        exhausted_.store(false, std::memory_order_relaxed);
        bool switched = false;
        for (size_t i = 0; i < ladder_.size() && !switched; ++i) {
            switched = activate_locked(i, is_fullscreen ? "window covers monitor" : "window left fullscreen");
        }
        if (switched) was_fullscreen = is_fullscreen;
    }
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
