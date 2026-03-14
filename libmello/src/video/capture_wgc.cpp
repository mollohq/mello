#ifdef _WIN32
#include "capture_wgc.hpp"
#include "../util/log.hpp"

#include <winrt/Windows.Foundation.h>
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

bool WgcCapture::initialize(const GraphicsDevice& device, const CaptureSourceDesc& desc) {
    device_ = device.d3d11();
    device_->GetImmediateContext(&context_);

    HWND hwnd = static_cast<HWND>(desc.hwnd);
    if (!hwnd || !IsWindow(hwnd)) {
        MELLO_LOG_ERROR(TAG, "WGC: Invalid HWND");
        return false;
    }

    item_ = create_capture_item_for_hwnd(hwnd);
    if (!item_) return false;

    auto size = item_.Size();
    width_  = static_cast<uint32_t>(size.Width);
    height_ = static_cast<uint32_t>(size.Height);

    MELLO_LOG_INFO(TAG, "Source: Window(hwnd=0x%p) backend=WGC resolution=%ux%u",
        hwnd, width_, height_);
    return true;
}

bool WgcCapture::start(uint32_t target_fps, FrameCallback callback) {
    if (running_.load()) return false;
    (void)target_fps; // WGC fires at compositor rate; we accept all frames

    callback_ = std::move(callback);

    auto winrt_device = create_winrt_device(device_.Get());

    frame_pool_ = winrt::Windows::Graphics::Capture::Direct3D11CaptureFramePool::CreateFreeThreaded(
        winrt_device,
        winrt::Windows::Graphics::DirectX::DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        {static_cast<int32_t>(width_), static_cast<int32_t>(height_)});

    frame_pool_.FrameArrived({this, &WgcCapture::on_frame_arrived});

    session_ = frame_pool_.CreateCaptureSession(item_);

    // Disable cursor compositing — we handle cursor as a separate channel
    session_.IsCursorCaptureEnabled(false);

    running_ = true;
    session_.StartCapture();
    return true;
}

void WgcCapture::stop() {
    running_ = false;
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

    if (callback_) {
        auto now = std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count();
        callback_(texture.Get(), static_cast<uint64_t>(now));
    }

    frame.Close();
}

} // namespace mello::video
#endif
