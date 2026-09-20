#include "graphics_device.hpp"
#include "../util/log.hpp"

#ifdef _WIN32
#include <d3d11.h>
#include <d3d11_4.h>  // ID3D11Multithread
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

uint64_t luid_to_u64(const LUID& luid) {
    return (static_cast<uint64_t>(static_cast<uint32_t>(luid.HighPart)) << 32) |
           static_cast<uint64_t>(static_cast<uint32_t>(luid.LowPart));
}

// Pick the adapter every video device in this process uses: the one with the
// most dedicated video memory, which on a laptop is the discrete GPU rather
// than the integrated one. Kept as one function because video_adapter_luid()
// has to answer for the same adapter that create_d3d11_device() will take. A
// second copy of this rule would drift, and the symptom of a drift is silent:
// textures shared from one adapter cannot be opened on another.
ComPtr<IDXGIAdapter1> select_video_adapter(DXGI_ADAPTER_DESC1& out_desc, bool log_candidates) {
    ComPtr<IDXGIFactory2> factory;
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) {
        MELLO_LOG_ERROR(TAG, "CreateDXGIFactory1 failed");
        return nullptr;
    }

    ComPtr<IDXGIAdapter1> best_adapter;
    SIZE_T best_vram = 0;

    for (UINT i = 0; ; ++i) {
        ComPtr<IDXGIAdapter1> candidate;
        if (factory->EnumAdapters1(i, &candidate) == DXGI_ERROR_NOT_FOUND) break;

        DXGI_ADAPTER_DESC1 desc{};
        candidate->GetDesc1(&desc);

        if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) continue;

        if (log_candidates) {
            char name[128]{};
            WideCharToMultiByte(CP_UTF8, 0, desc.Description, -1, name, sizeof(name), nullptr,
                                nullptr);
            MELLO_LOG_INFO(TAG, "  adapter[%u]: \"%s\" luid=0x%016llX vram=%lluMB",
                i, name, static_cast<unsigned long long>(luid_to_u64(desc.AdapterLuid)),
                desc.DedicatedVideoMemory / (1024 * 1024));
        }

        if (desc.DedicatedVideoMemory > best_vram) {
            best_vram = desc.DedicatedVideoMemory;
            best_adapter = candidate;
            out_desc = desc;
        }
    }
    return best_adapter;
}

uint64_t video_adapter_luid() {
    DXGI_ADAPTER_DESC1 desc{};
    ComPtr<IDXGIAdapter1> adapter = select_video_adapter(desc, false);
    if (!adapter) return 0;
    return luid_to_u64(desc.AdapterLuid);
}

GraphicsDevice create_d3d11_device() {
    DXGI_ADAPTER_DESC1 best_desc{};
    ComPtr<IDXGIAdapter1> best_adapter = select_video_adapter(best_desc, true);

    if (!best_adapter) {
        MELLO_LOG_ERROR(TAG, "No suitable DXGI adapter found");
        return {GraphicsBackend::D3D11, nullptr, {}, 0};
    }

    D3D_FEATURE_LEVEL feature_levels[] = {
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
    };

    ComPtr<ID3D11Device> device;
    D3D_FEATURE_LEVEL achieved_level{};
    UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;

    HRESULT hr = D3D11CreateDevice(
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
        return {GraphicsBackend::D3D11, nullptr, {}, 0};
    }

    // The immediate context is shared: capture threads copy frames into it and
    // the encode thread converts them. A D3D11 immediate context is not thread
    // safe on its own, and without this the video processor refuses input views
    // with E_INVALIDARG while the other thread is inside a copy. Measured
    // against Unigine Heaven in Direct3D 9 on 2026-09-16, where capture ran at
    // 50 fps and every frame failed to convert.
    ComPtr<ID3D11DeviceContext> immediate;
    device->GetImmediateContext(&immediate);
    ComPtr<ID3D11Multithread> multithread;
    if (immediate && SUCCEEDED(immediate.As(&multithread)) && multithread) {
        multithread->SetMultithreadProtected(TRUE);
    } else {
        MELLO_LOG_WARN(TAG, "no ID3D11Multithread on this device; the context is unprotected");
    }

    GraphicsDevice result{GraphicsBackend::D3D11, nullptr, {}, 0};
    WideCharToMultiByte(CP_UTF8, 0, best_desc.Description, -1,
                        result.adapter_name, sizeof(result.adapter_name), nullptr, nullptr);
    result.adapter_name[sizeof(result.adapter_name) - 1] = '\0';
    result.adapter_luid = luid_to_u64(best_desc.AdapterLuid);

    MELLO_LOG_INFO(TAG,
        "D3D11 device created: adapter=\"%s\" luid=0x%016llX vram=%lluMB feature_level=0x%04X",
        result.adapter_name,
        static_cast<unsigned long long>(result.adapter_luid),
        best_desc.DedicatedVideoMemory / (1024 * 1024),
        achieved_level);

    device->AddRef();
    result.handle = device.Get();
    return result;
}

#endif

} // namespace mello::video
