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
};

static BOOL CALLBACK enum_window_proc(HWND hwnd, LPARAM lParam) {
    auto* data = reinterpret_cast<EnumWindowData*>(lParam);
    DWORD wnd_pid = 0;
    GetWindowThreadProcessId(hwnd, &wnd_pid);
    if (wnd_pid == data->pid && IsWindowVisible(hwnd) && GetWindow(hwnd, GW_OWNER) == nullptr) {
        data->result = hwnd;
        return FALSE;
    }
    return TRUE;
}

HWND find_main_window(uint32_t pid) {
    EnumWindowData data{pid, nullptr};
    EnumWindows(enum_window_proc, reinterpret_cast<LPARAM>(&data));
    return data.result;
}

static bool is_likely_exclusive_fullscreen(HWND hwnd, HMONITOR mon) {
    if (!hwnd || !mon) return false;

    MONITORINFO mi{};
    mi.cbSize = sizeof(mi);
    if (!GetMonitorInfo(mon, &mi)) return false;

    RECT wr{};
    if (!GetWindowRect(hwnd, &wr)) return false;

    bool covers_monitor =
        wr.left <= mi.rcMonitor.left &&
        wr.top <= mi.rcMonitor.top &&
        wr.right >= mi.rcMonitor.right &&
        wr.bottom >= mi.rcMonitor.bottom;
    if (!covers_monitor) return false;

    LONG style = GetWindowLong(hwnd, GWL_STYLE);
    return (style & WS_OVERLAPPEDWINDOW) == 0;
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

    HMONITOR mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
    if (!mon) return false;
    if (!is_likely_exclusive_fullscreen(hwnd, mon)) return false;

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
    if (!is_likely_exclusive_fullscreen(hwnd, mon)) return -1;

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
        MELLO_LOG_INFO(TAG, "Source: Process(pid=%u) exclusive_fullscreen=true output=%d -> backend=DXGI-DDI",
            pid_, static_cast<int>(fs_output));
    } else {
        if (!hwnd) {
            MELLO_LOG_ERROR(TAG, "Process(pid=%u): no visible window found", pid_);
            return false;
        }
        auto wgc = std::make_unique<WgcCapture>();
        CaptureSourceDesc wnd_desc{};
        wnd_desc.mode = CaptureMode::Window;
        wnd_desc.hwnd = hwnd;
        if (!wgc->initialize(device, wnd_desc)) return false;
        active_ = std::move(wgc);
        MELLO_LOG_INFO(TAG, "Source: Process(pid=%u) exclusive_fullscreen=false -> backend=WGC hwnd=0x%p",
            pid_, hwnd);
    }

    return true;
}

bool ProcessCapture::start(uint32_t target_fps, FrameCallback callback) {
    if (running_.load()) return false;
    target_fps_ = target_fps;
    callback_ = callback;
    swap_occurred_.store(false, std::memory_order_release);

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
    return active_ ? active_->width() : 0;
}

uint32_t ProcessCapture::height() const {
    std::lock_guard<std::mutex> lock(swap_mutex_);
    return active_ ? active_->height() : 0;
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

void ProcessCapture::monitor_thread() {
    bool was_fullscreen = false;
    {
        std::lock_guard<std::mutex> lock(swap_mutex_);
        // Track the initial state based on which backend we started with
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
