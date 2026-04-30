#ifdef _WIN32
#include "capture_process.hpp"
#include "capture_dxgi.hpp"
#include "capture_wgc.hpp"
#include "../util/log.hpp"
#include <dxgi.h>
#include <wrl/client.h>

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

    MELLO_LOG_INFO(TAG, "find_main_window: pid=%u hwnd=%p restored_area=%lld visible=%d "
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
    if (data.result) {
        int64_t area = get_restored_area(data.result);
        char title[128] = {};
        GetWindowTextA(data.result, title, sizeof(title));
        MELLO_LOG_INFO(TAG, "find_main_window: pid=%u selected hwnd=%p restored_area=%lld title=\"%.60s\"",
            pid, data.result, (long long)area, title);
    } else {
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

    MELLO_LOG_INFO(TAG, "is_likely_fullscreen: hwnd=%p mon=%dx%d win=%dx%d "
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

// --- ProcessCapture ---

bool ProcessCapture::initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) {
    pid_ = desc.pid;
    device_ = device;

    uint32_t fs_output = 0;
    HWND hwnd = nullptr;
    bool exclusive_fs = resolve_process_dxgi_output(
        pid_, device_.d3d11(), &fs_output, &hwnd);

    if (exclusive_fs) {
        auto dxgi = std::make_unique<DxgiCapture>();
        CaptureSourceDesc monitor_desc{};
        monitor_desc.mode = CaptureMode::Monitor;
        monitor_desc.monitor_index = fs_output;
        if (!dxgi->initialize(device, monitor_desc)) return false;
        active_ = std::move(dxgi);
        MELLO_LOG_INFO(TAG, "Source: Process(pid=%u) fullscreen -> backend=DXGI-DDI output=%d",
            pid_, static_cast<int>(fs_output));
        return true;
    }

    if (!hwnd) {
        MELLO_LOG_ERROR(TAG, "Process(pid=%u): no window found", pid_);
        return false;
    }

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

    auto wgc = std::make_unique<WgcCapture>();
    CaptureSourceDesc wnd_desc{};
    wnd_desc.mode = CaptureMode::Window;
    wnd_desc.hwnd = hwnd;
    if (!wgc->initialize(device, wnd_desc)) return false;
    active_ = std::move(wgc);
    MELLO_LOG_INFO(TAG, "Source: Process(pid=%u) -> backend=WGC hwnd=0x%p",
        pid_, hwnd);
    return true;
}

bool ProcessCapture::start(uint32_t target_fps, FrameCallback callback) {
    if (running_.load()) return false;
    target_fps_ = target_fps;
    callback_ = callback;
    swap_occurred_.store(false, std::memory_order_release);

    // Deferred mode: no active backend yet, monitor thread will start capture
    if (deferred_hwnd_) {
        running_ = true;
        monitor_thread_ = std::thread(&ProcessCapture::monitor_thread, this);
        return true;
    }

    std::lock_guard<std::mutex> lock(swap_mutex_);
    if (!active_ || !active_->start(target_fps, callback)) return false;

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

bool ProcessCapture::swap_to_dxgi() {
    std::lock_guard<std::mutex> lock(swap_mutex_);
    if (!active_) return false;

    uint32_t fs_output = 0;
    if (!resolve_process_dxgi_output(pid_, device_.d3d11(), &fs_output, nullptr)) {
        MELLO_LOG_WARN(TAG, "Hot-swap skipped for pid=%u: fullscreen output unresolved on current adapter", pid_);
        return false;
    }

    const char* old_backend = active_->backend_name();
    auto dxgi = std::make_unique<DxgiCapture>();
    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Monitor;
    desc.monitor_index = fs_output;
    if (!dxgi->initialize(device_, desc)) {
        MELLO_LOG_WARN(TAG, "Hot-swap %s->DXGI init failed for pid=%u output=%u",
                       old_backend, pid_, fs_output);
        return false;
    }

    active_->stop();
    if (!dxgi->start(target_fps_, callback_)) {
        bool recovered = active_->start(target_fps_, callback_);
        MELLO_LOG_ERROR(TAG, "Hot-swap %s->DXGI start failed for pid=%u output=%u",
                        old_backend, pid_, fs_output);
        if (!recovered) {
            MELLO_LOG_ERROR(TAG, "Hot-swap rollback failed for pid=%u; previous backend did not restart", pid_);
        }
        return false;
    }

    active_ = std::move(dxgi);
    swap_occurred_.store(true, std::memory_order_release);
    MELLO_LOG_WARN(TAG, "Hot-swap complete for pid=%u: %s -> DXGI-DDI (output=%u)",
                   pid_, old_backend, fs_output);
    return true;
}

bool ProcessCapture::swap_to_wgc() {
    std::lock_guard<std::mutex> lock(swap_mutex_);
    if (!active_) return false;

    HWND hwnd = find_main_window(pid_);
    if (!hwnd) {
        MELLO_LOG_WARN(TAG, "Hot-swap skipped for pid=%u: no window found for WGC", pid_);
        return false;
    }

    const char* old_backend = active_->backend_name();
    auto wgc = std::make_unique<WgcCapture>();
    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Window;
    desc.hwnd = hwnd;
    if (!wgc->initialize(device_, desc)) {
        MELLO_LOG_WARN(TAG, "Hot-swap %s->WGC init failed for pid=%u hwnd=0x%p",
                       old_backend, pid_, hwnd);
        return false;
    }

    active_->stop();
    if (!wgc->start(target_fps_, callback_)) {
        bool recovered = active_->start(target_fps_, callback_);
        MELLO_LOG_ERROR(TAG, "Hot-swap %s->WGC start failed for pid=%u hwnd=0x%p",
                        old_backend, pid_, hwnd);
        if (!recovered) {
            MELLO_LOG_ERROR(TAG, "Hot-swap rollback failed for pid=%u; previous backend did not restart", pid_);
        }
        return false;
    }

    active_ = std::move(wgc);
    swap_occurred_.store(true, std::memory_order_release);
    MELLO_LOG_WARN(TAG, "Hot-swap complete for pid=%u: %s -> WGC", pid_, old_backend);
    return true;
}

bool ProcessCapture::start_deferred() {
    HWND hwnd = deferred_hwnd_;
    if (!hwnd) return false;

    WINDOWPLACEMENT wp{};
    wp.length = sizeof(wp);
    if (!GetWindowPlacement(hwnd, &wp) || wp.showCmd == SW_SHOWMINIMIZED)
        return false;

    // Window is restored/visible -- start capture
    MELLO_LOG_INFO(TAG, "Process(pid=%u) deferred: window restored, starting capture", pid_);

    uint32_t fs_output = 0;
    bool exclusive_fs = resolve_process_dxgi_output(pid_, device_.d3d11(), &fs_output, nullptr);

    std::lock_guard<std::mutex> lock(swap_mutex_);
    if (exclusive_fs) {
        auto dxgi = std::make_unique<DxgiCapture>();
        CaptureSourceDesc desc{};
        desc.mode = CaptureMode::Monitor;
        desc.monitor_index = fs_output;
        if (!dxgi->initialize(device_, desc) || !dxgi->start(target_fps_, callback_)) {
            MELLO_LOG_ERROR(TAG, "Deferred DXGI start failed for pid=%u", pid_);
            return false;
        }
        active_ = std::move(dxgi);
        MELLO_LOG_INFO(TAG, "Source: Process(pid=%u) deferred -> DXGI-DDI output=%u", pid_, fs_output);
    } else {
        auto wgc = std::make_unique<WgcCapture>();
        CaptureSourceDesc desc{};
        desc.mode = CaptureMode::Window;
        desc.hwnd = hwnd;
        if (!wgc->initialize(device_, desc) || !wgc->start(target_fps_, callback_)) {
            MELLO_LOG_ERROR(TAG, "Deferred WGC start failed for pid=%u", pid_);
            return false;
        }
        active_ = std::move(wgc);
        MELLO_LOG_INFO(TAG, "Source: Process(pid=%u) deferred -> WGC hwnd=0x%p", pid_, hwnd);
    }

    deferred_hwnd_ = nullptr;
    swap_occurred_.store(true, std::memory_order_release);
    return true;
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

    while (running_.load()) {
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
        if (!running_.load()) break;

        uint32_t output_idx = 0;
        bool is_fullscreen = resolve_process_dxgi_output(
            pid_, device_.d3d11(), &output_idx, nullptr);

        if (is_fullscreen && !was_fullscreen) {
            if (swap_to_dxgi()) {
                was_fullscreen = true;
            }
        } else if (!is_fullscreen && was_fullscreen) {
            if (swap_to_wgc()) {
                was_fullscreen = false;
            }
        }
    }
}

// --- Factory ---

std::unique_ptr<CaptureSource> create_capture_source(const CaptureSourceDesc& desc) {
    switch (desc.mode) {
        case CaptureMode::Monitor:
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
