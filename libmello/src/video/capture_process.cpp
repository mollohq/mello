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

int query_exclusive_fullscreen_output(uint32_t pid) {
    ComPtr<IDXGIFactory1> factory;
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) return -1;

    UINT adapter_idx = 0;
    ComPtr<IDXGIAdapter1> adapter;
    while (factory->EnumAdapters1(adapter_idx++, &adapter) == S_OK) {
        UINT output_idx = 0;
        ComPtr<IDXGIOutput> output;
        while (adapter->EnumOutputs(output_idx++, &output) == S_OK) {
            BOOL fullscreen = FALSE;
            ComPtr<IDXGISwapChain> swap_chain; // Can't directly query this way
            // Heuristic: check if the process HWND matches the fullscreen output's target
            HWND hwnd = find_main_window(pid);
            if (hwnd) {
                HMONITOR mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
                DXGI_OUTPUT_DESC desc{};
                output->GetDesc(&desc);
                if (desc.Monitor == mon) {
                    // Check if the window is fullscreen-sized
                    RECT wr{};
                    GetWindowRect(hwnd, &wr);
                    RECT dr = desc.DesktopCoordinates;
                    if (wr.left <= dr.left && wr.top <= dr.top &&
                        wr.right >= dr.right && wr.bottom >= dr.bottom) {
                        LONG style = GetWindowLong(hwnd, GWL_STYLE);
                        if (!(style & WS_OVERLAPPEDWINDOW)) {
                            return static_cast<int>(output_idx - 1);
                        }
                    }
                }
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

    int fs_output = query_exclusive_fullscreen_output(pid_);
    bool exclusive_fs = fs_output >= 0;

    if (exclusive_fs) {
        auto dxgi = std::make_unique<DxgiCapture>();
        CaptureSourceDesc monitor_desc{};
        monitor_desc.mode = CaptureMode::Monitor;
        monitor_desc.monitor_index = static_cast<uint32_t>(fs_output);
        if (!dxgi->initialize(device, monitor_desc)) return false;
        active_ = std::move(dxgi);
        MELLO_LOG_INFO(TAG, "Source: Process(pid=%u) exclusive_fullscreen=true output=%d -> backend=DXGI-DDI",
            pid_, fs_output);
    } else {
        HWND hwnd = find_main_window(pid_);
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

    std::lock_guard<std::mutex> lock(swap_mutex_);
    if (!active_->start(target_fps, callback)) return false;

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

void ProcessCapture::swap_to_dxgi() {
    std::lock_guard<std::mutex> lock(swap_mutex_);
    int fs_output = query_exclusive_fullscreen_output(pid_);
    if (fs_output < 0) return;

    MELLO_LOG_WARN(TAG, "Hot-swap triggered for pid=%u -- exclusive_fullscreen gained -> switching WGC -> DXGI-DDI", pid_);

    active_->stop();
    auto dxgi = std::make_unique<DxgiCapture>();
    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Monitor;
    desc.monitor_index = static_cast<uint32_t>(fs_output);
    if (dxgi->initialize(device_, desc) && dxgi->start(target_fps_, callback_)) {
        active_ = std::move(dxgi);
        swap_occurred_ = true;
        MELLO_LOG_INFO(TAG, "Hot-swap complete, keyframe requested");
    }
}

void ProcessCapture::swap_to_wgc() {
    std::lock_guard<std::mutex> lock(swap_mutex_);
    HWND hwnd = find_main_window(pid_);
    if (!hwnd) return;

    MELLO_LOG_WARN(TAG, "Hot-swap triggered for pid=%u -- exclusive_fullscreen lost -> switching DXGI-DDI -> WGC", pid_);

    active_->stop();
    auto wgc = std::make_unique<WgcCapture>();
    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Window;
    desc.hwnd = hwnd;
    if (wgc->initialize(device_, desc) && wgc->start(target_fps_, callback_)) {
        active_ = std::move(wgc);
        swap_occurred_ = true;
        MELLO_LOG_INFO(TAG, "Hot-swap complete, keyframe requested");
    }
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

        bool is_fullscreen = query_exclusive_fullscreen_output(pid_) >= 0;

        if (is_fullscreen && !was_fullscreen) {
            swap_to_dxgi();
            was_fullscreen = true;
        } else if (!is_fullscreen && was_fullscreen) {
            swap_to_wgc();
            was_fullscreen = false;
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
