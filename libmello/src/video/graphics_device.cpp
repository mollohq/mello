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

    ComPtr<IDXGIAdapter1> adapter;
    hr = factory->EnumAdapters1(0, &adapter);
    if (FAILED(hr)) {
        MELLO_LOG_ERROR(TAG, "EnumAdapters1 failed: hr=0x%08X", hr);
        return {GraphicsBackend::D3D11, nullptr};
    }

    DXGI_ADAPTER_DESC1 desc{};
    adapter->GetDesc1(&desc);

    D3D_FEATURE_LEVEL feature_levels[] = {
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
    };

    ComPtr<ID3D11Device> device;
    D3D_FEATURE_LEVEL achieved_level{};
    UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
#ifndef NDEBUG
    flags |= D3D11_CREATE_DEVICE_DEBUG;
#endif

    hr = D3D11CreateDevice(
        adapter.Get(),
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
    WideCharToMultiByte(CP_UTF8, 0, desc.Description, -1, adapter_name, sizeof(adapter_name), nullptr, nullptr);

    MELLO_LOG_INFO(TAG, "D3D11 device created: adapter=\"%s\" vram=%lluMB feature_level=0x%04X",
        adapter_name,
        desc.DedicatedVideoMemory / (1024 * 1024),
        achieved_level);

    // AddRef because we're handing out a raw pointer that the pipeline will own
    device->AddRef();
    return {GraphicsBackend::D3D11, device.Get()};
}

#else

GraphicsDevice create_d3d11_device() {
    MELLO_LOG_ERROR(TAG, "D3D11 not available on this platform");
    return {GraphicsBackend::D3D11, nullptr};
}

#endif

} // namespace mello::video
