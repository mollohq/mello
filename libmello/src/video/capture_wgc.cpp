#ifdef _WIN32
#include "capture_wgc.hpp"
#include "../util/log.hpp"

#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Foundation.Metadata.h>
#include <future>
#include <memory>
#include <winrt/Windows.Graphics.Capture.h>
#include <winrt/Windows.Graphics.DirectX.h>
#include <winrt/Windows.Graphics.DirectX.Direct3D11.h>
#include <Windows.Graphics.Capture.Interop.h>
#include <windows.graphics.directx.direct3d11.interop.h>
#include <inspectable.h>

namespace mello::video {

static constexpr const char* TAG = "video/capture";

// Helper: create a WinRT Direct3D11 device from a raw ID3D11Device
static winrt::Windows::Graphics::DirectX::Direct3D11::IDirect3DDevice
create_winrt_device(ID3D11Device* d3d_device) {
    Microsoft::WRL::ComPtr<IDXGIDevice> dxgi_device;
    d3d_device->QueryInterface(IID_PPV_ARGS(&dxgi_device));

    winrt::com_ptr<::IInspectable> inspectable;
    CreateDirect3D11DeviceFromDXGIDevice(dxgi_device.Get(), inspectable.put());

    return inspectable.as<winrt::Windows::Graphics::DirectX::Direct3D11::IDirect3DDevice>();
}

// Helper: create a GraphicsCaptureItem from an HWND
static winrt::Windows::Graphics::Capture::GraphicsCaptureItem
create_capture_item_for_hwnd(HWND hwnd) {
    auto factory = winrt::get_activation_factory<
        winrt::Windows::Graphics::Capture::GraphicsCaptureItem,
        IGraphicsCaptureItemInterop>();

    winrt::Windows::Graphics::Capture::GraphicsCaptureItem item{nullptr};
    HRESULT hr = factory->CreateForWindow(
        hwnd,
        winrt::guid_of<ABI::Windows::Graphics::Capture::IGraphicsCaptureItem>(),
        winrt::put_abi(item));

    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateForWindow failed: hr=0x%08X", hr);
        return nullptr;
    }
    return item;
}

// Helper: create a GraphicsCaptureItem for a monitor
static winrt::Windows::Graphics::Capture::GraphicsCaptureItem
create_capture_item_for_monitor(HMONITOR monitor) {
    try {
        auto factory = winrt::get_activation_factory<
            winrt::Windows::Graphics::Capture::GraphicsCaptureItem,
            IGraphicsCaptureItemInterop>();
        winrt::Windows::Graphics::Capture::GraphicsCaptureItem item{nullptr};
        HRESULT hr = factory->CreateForMonitor(
            monitor,
            winrt::guid_of<ABI::Windows::Graphics::Capture::IGraphicsCaptureItem>(),
            winrt::put_abi(item));
        if (FAILED(hr)) {
            MELLO_LOG_ERROR(TAG, "CreateForMonitor failed: hr=0x%08X", hr);
            return nullptr;
        }
        return item;
    } catch (const winrt::hresult_error& e) {
        MELLO_LOG_ERROR(TAG, "CreateForMonitor threw: hr=0x%08X", static_cast<unsigned>(e.code()));
        return nullptr;
    }
}

// Ask once per process for borderless capture access (Windows 11), the same
// way OBS does. Unpackaged apps get it without a prompt. The request runs on
// its own thread because blocking on a WinRT async call is not allowed on an
// STA thread, and the caller may be one. Bounded: if Windows does not answer
// in time, capture starts with the border.
static void ensure_borderless_access() {
    static std::once_flag once;
    std::call_once(once, [] {
        try {
            if (!winrt::Windows::Foundation::Metadata::ApiInformation::IsPropertyPresent(
                    L"Windows.Graphics.Capture.GraphicsCaptureSession", L"IsBorderRequired")) {
                MELLO_LOG_INFO(TAG, "WGC: borderless capture not supported on this Windows build");
                return;
            }
        } catch (...) {
            return;
        }
        auto done = std::make_shared<std::promise<void>>();
        auto future = done->get_future();
        std::thread([done] {
            try {
                winrt::init_apartment(winrt::apartment_type::multi_threaded);
            } catch (...) {
            }
            try {
                auto status = winrt::Windows::Graphics::Capture::GraphicsCaptureAccess::RequestAccessAsync(
                                  winrt::Windows::Graphics::Capture::GraphicsCaptureAccessKind::Borderless)
                                  .get();
                MELLO_LOG_INFO(TAG, "WGC: borderless access request answered (status=%d)",
                               static_cast<int>(status));
            } catch (const winrt::hresult_error& e) {
                MELLO_LOG_WARN(TAG, "WGC: borderless access request failed: hr=0x%08X",
                               static_cast<unsigned>(e.code()));
            } catch (...) {
            }
            done->set_value();
        }).detach();
        if (future.wait_for(std::chrono::seconds(1)) != std::future_status::ready) {
            MELLO_LOG_WARN(TAG, "WGC: borderless access request did not answer within 1 s");
        }
    });
}

bool WgcCapture::initialize_monitor(const GraphicsDevice& device, HMONITOR monitor) {
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);
    if (!monitor) {
        MELLO_LOG_ERROR(TAG, "WGC: invalid monitor");
        return false;
    }
    item_ = create_capture_item_for_monitor(monitor);
    if (!item_) return false;
    monitor_ = true;
    auto size = item_.Size();
    width_  = static_cast<uint32_t>(size.Width);
    height_ = static_cast<uint32_t>(size.Height);
    MELLO_LOG_INFO(TAG, "Source: Monitor(hmonitor=0x%p) backend=WGC resolution=%ux%u",
        monitor, width_, height_);
    return true;
}

bool WgcCapture::initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) {
    if (desc.mode == CaptureMode::Monitor) {
        // Same index meaning as DXGI: output index on the encoder device's
        // adapter.
        Microsoft::WRL::ComPtr<IDXGIDevice> dxgi_device;
        Microsoft::WRL::ComPtr<IDXGIAdapter> adapter;
        Microsoft::WRL::ComPtr<IDXGIOutput> output;
        DXGI_OUTPUT_DESC out_desc{};
        if (FAILED(device.d3d11()->QueryInterface(IID_PPV_ARGS(&dxgi_device))) ||
            FAILED(dxgi_device->GetAdapter(&adapter)) ||
            FAILED(adapter->EnumOutputs(desc.monitor_index, &output)) ||
            FAILED(output->GetDesc(&out_desc))) {
            MELLO_LOG_ERROR(TAG, "WGC: monitor index %u not found on the encoder adapter", desc.monitor_index);
            return false;
        }
        return initialize_monitor(device, out_desc.Monitor);
    }

    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);

    HWND hwnd = static_cast<HWND>(desc.hwnd);
    if (!hwnd || !IsWindow(hwnd)) {
        MELLO_LOG_ERROR(TAG, "WGC: Invalid HWND");
        return false;
    }

    item_ = create_capture_item_for_hwnd(hwnd);
    if (!item_) return false;

    DWORD wnd_pid = 0;
    GetWindowThreadProcessId(hwnd, &wnd_pid);
    target_liveness_.track(static_cast<uint32_t>(wnd_pid));

    auto size = item_.Size();
    width_  = static_cast<uint32_t>(size.Width);
    height_ = static_cast<uint32_t>(size.Height);

    MELLO_LOG_INFO(TAG, "Source: Window(hwnd=0x%p) backend=WGC resolution=%ux%u",
        hwnd, width_, height_);
    return true;
}

bool WgcCapture::start(uint32_t target_fps, FrameCallback callback) {
    if (running_.load()) return false;

    {
        std::lock_guard<std::mutex> lock(throttle_mutex_);
        target_fps_ = target_fps > 0 ? target_fps : 60;
        frame_credit_us_ = 0.0;
        last_frame_us_ = 0;
        throttle_primed_ = false;
    }

    callback_ = std::move(callback);

    auto winrt_device = create_winrt_device(device_.Get());

    frame_pool_ = winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool::CreateFreeThreaded(
        winrt_device,
        winrt::Windows::Graphics::DirectX::DirectXPixelFormat::B8G8R8A8UIntNormalized,
        3,
        {static_cast<int32_t>(width_), static_cast<int32_t>(height_)});

    frame_pool_.FrameArrived({this, &WgcCapture::on_frame_arrived});

    session_ = frame_pool_.CreateCaptureSession(item_);

    // Disable cursor compositing — we handle cursor as a separate channel
    session_.IsCursorCaptureEnabled(false);

    // No yellow capture border on Windows 11. Every WGC stream showed it,
    // which also made WGC look worse than DXGI in side-by-side tests.
    ensure_borderless_access();
    try {
        if (winrt::Windows::Foundation::Metadata::ApiInformation::IsPropertyPresent(
                L"Windows.Graphics.Capture.GraphicsCaptureSession", L"IsBorderRequired")) {
            session_.IsBorderRequired(false);
        }
    } catch (const winrt::hresult_error& e) {
        MELLO_LOG_WARN(TAG, "WGC: IsBorderRequired(false) failed: hr=0x%08X",
                       static_cast<unsigned>(e.code()));
    }

    closed_ = false;
    closed_token_ = item_.Closed([this](auto&&, auto&&) {
        MELLO_LOG_WARN(TAG, "WGC: capture item closed (%s)", monitor_ ? "monitor" : "window");
        closed_ = true;
    });

    running_ = true;
    session_.StartCapture();
    return true;
}

void WgcCapture::stop() {
    running_ = false;
    target_liveness_.reset();
    if (item_ && closed_token_.value != 0) {
        try {
            item_.Closed(closed_token_);
        } catch (...) {
        }
        closed_token_ = {};
    }
    if (session_) {
        session_.Close();
        session_ = nullptr;
    }
    if (frame_pool_) {
        frame_pool_.Close();
        frame_pool_ = nullptr;
    }
    item_ = nullptr;
}

bool WgcCapture::get_cursor(CursorData& out) {
    CURSORINFO ci{};
    ci.cbSize = sizeof(ci);
    if (!GetCursorInfo(&ci)) return false;

    std::lock_guard<std::mutex> lock(cursor_mutex_);
    cursor_.x = ci.ptScreenPos.x;
    cursor_.y = ci.ptScreenPos.y;
    cursor_.visible = (ci.flags & CURSOR_SHOWING) != 0;
    cursor_.shape_changed = false;
    out = cursor_;
    return true;
}

void WgcCapture::on_frame_arrived(
    winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool const& pool,
    winrt::Windows::Foundation::IInspectable const&)
{
    if (!running_.load()) return;

    auto frame = pool.TryGetNextFrame();
    if (!frame) return;

    auto surface = frame.Surface();

    // Get the underlying ID3D11Texture2D from the WinRT surface
    auto access = surface.as<Windows::Graphics::DirectX::Direct3D11::IDirect3DDxgiInterfaceAccess>();
    Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
    HRESULT hr = access->GetInterface(IID_PPV_ARGS(&texture));
    if (FAILED(hr)) return;

    // Throttle compositor-rate delivery down to target_fps. Without this the
    // encode queue absorbs a permanent 2x+ arrival rate on high-refresh
    // desktops and churns drop-oldest frames instead of encoding useful ones.
    const uint64_t now = static_cast<uint64_t>(
        std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count());
    {
        std::lock_guard<std::mutex> lock(throttle_mutex_);
        const double target_interval_us = 1'000'000.0 / target_fps_;
        if (!throttle_primed_) {
            throttle_primed_ = true;
        } else {
            frame_credit_us_ += static_cast<double>(now - last_frame_us_);
            if (frame_credit_us_ < target_interval_us) {
                frame.Close();
                return;
            }
            frame_credit_us_ -= target_interval_us;
            // Clamp runaway credit after stalls so we never burst-catch-up.
            const double max_credit = 2.0 * target_interval_us;
            if (frame_credit_us_ > max_credit) frame_credit_us_ = max_credit;
        }
        last_frame_us_ = now;
    }

    if (delay_hist_) {
        // SystemRelativeTime is the QPC-based time the frame was composed, in
        // 100 ns units.
        LARGE_INTEGER qpc{}, freq{};
        if (QueryPerformanceCounter(&qpc) && QueryPerformanceFrequency(&freq) && freq.QuadPart > 0) {
            const double now_100ns = static_cast<double>(qpc.QuadPart) * 10'000'000.0 /
                                     static_cast<double>(freq.QuadPart);
            const double ms = (now_100ns - static_cast<double>(frame.SystemRelativeTime().count())) / 10'000.0;
            delay_hist_->record_ms(ms);
        }
    }

    if (callback_) {
        callback_(texture.Get(), now);
    }

    frame.Close();
}

} // namespace mello::video
#endif
