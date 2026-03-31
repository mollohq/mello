#include "graphics_device.hpp"
#include "../util/log.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <dxgi1_2.h>
#include <wrl/client.h>
#include <cassert>

using Microsoft::WRL::ComPtr;
#endif

namespace mello::video {

static constexpr const char* TAG = "video/device";

#ifdef _WIN32

ID3D11Device* GraphicsDevice::d3d11() const {
    assert(backend == GraphicsBackend::D3D11 && "GraphicsDevice is not D3D11");
    return static_cast<ID3D11Device*>(handle);
}

GraphicsDevice create_d3d11_device() {
    ComPtr<IDXGIFactory2> factory;
    HRESULT hr = CreateDXGIFactory1(IID_PPV_ARGS(&factory));
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "CreateDXGIFactory1 failed: hr=0x%08X", hr);
        return {GraphicsBackend::D3D11, nullptr};
    }

    // Enumerate all adapters, prefer discrete GPU (most VRAM) for HW encoding
    ComPtr<IDXGIAdapter1> best_adapter;
    DXGI_ADAPTER_DESC1 best_desc{};
    SIZE_T best_vram = 0;

    for (UINT i = 0; ; ++i) {
        ComPtr<IDXGIAdapter1> candidate;
        if (factory->EnumAdapters1(i, &candidate) == DXGI_ERROR_NOT_FOUND) break;

        DXGI_ADAPTER_DESC1 desc{};
        candidate->GetDesc1(&desc);

        if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) continue;

        char name[128]{};
        WideCharToMultiByte(CP_UTF8, 0, desc.Description, -1, name, sizeof(name), nullptr, nullptr);
        MELLO_LOG_INFO(TAG, "  adapter[%u]: \"%s\" vram=%lluMB",
            i, name, desc.DedicatedVideoMemory / (1024 * 1024));

        if (desc.DedicatedVideoMemory > best_vram) {
            best_vram = desc.DedicatedVideoMemory;
            best_adapter = candidate;
            best_desc = desc;
        }
    }

    if (!best_adapter) {
        MELLO_LOG_ERROR(TAG, "No suitable DXGI adapter found");
        return {GraphicsBackend::D3D11, nullptr};
    }

    D3D_FEATURE_LEVEL feature_levels[] = {
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
    };

    ComPtr<ID3D11Device> device;
    D3D_FEATURE_LEVEL achieved_level{};
    UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;

    hr = D3D11CreateDevice(
        best_adapter.Get(),
        D3D_DRIVER_TYPE_UNKNOWN,
        nullptr,
        flags,
        feature_levels,
        _countof(feature_levels),
        D3D11_SDK_VERSION,
        &device,
        &achieved_level,
        nullptr
    );

    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "D3D11CreateDevice failed: hr=0x%08X", hr);
        return {GraphicsBackend::D3D11, nullptr};
    }

    char adapter_name[128]{};
    WideCharToMultiByte(CP_UTF8, 0, best_desc.Description, -1, adapter_name, sizeof(adapter_name), nullptr, nullptr);

    MELLO_LOG_INFO(TAG, "D3D11 device created: adapter=\"%s\" vram=%lluMB feature_level=0x%04X",
        adapter_name,
        best_desc.DedicatedVideoMemory / (1024 * 1024),
        achieved_level);

    device->AddRef();
    return {GraphicsBackend::D3D11, device.Get()};
}

#endif

} // namespace mello::video
