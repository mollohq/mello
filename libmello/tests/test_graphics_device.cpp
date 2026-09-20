// The adapter contract between libmello and anything that opens its textures.
//
// libmello hands out shared texture handles. A shared handle is only valid on
// the adapter that created it, so a consumer has to create its own D3D11 device
// on that same adapter. `video_adapter_luid()` is how a consumer learns which
// one that is, before any pipeline exists.
//
// These tests hold that contract in place. They need a GPU, so they skip under
// CI like the other GPU tests in this suite.

#include <gtest/gtest.h>

#ifdef _WIN32

#include <windows.h>

#include <d3d11.h>
#include <d3d11_1.h>
#include <dxgi1_2.h>
#include <wrl/client.h>

#include <cstdlib>
#include <vector>

#include "video/graphics_device.hpp"

using Microsoft::WRL::ComPtr;
using namespace mello::video;

namespace {

bool running_under_ci() {
    const char* ci = std::getenv("CI");
    return ci && *ci && std::string(ci) != "0";
}

uint64_t luid_of(const DXGI_ADAPTER_DESC1& desc) {
    return (static_cast<uint64_t>(static_cast<uint32_t>(desc.AdapterLuid.HighPart)) << 32) |
           static_cast<uint64_t>(static_cast<uint32_t>(desc.AdapterLuid.LowPart));
}

// Every hardware adapter on this machine, in enumeration order. Index 0 is what
// D3D11CreateDevice picks when it is given no adapter.
std::vector<ComPtr<IDXGIAdapter1>> hardware_adapters() {
    std::vector<ComPtr<IDXGIAdapter1>> adapters;
    ComPtr<IDXGIFactory1> factory;
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) return adapters;
    for (UINT i = 0;; ++i) {
        ComPtr<IDXGIAdapter1> adapter;
        if (factory->EnumAdapters1(i, &adapter) == DXGI_ERROR_NOT_FOUND) break;
        DXGI_ADAPTER_DESC1 desc{};
        adapter->GetDesc1(&desc);
        if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) continue;
        adapters.push_back(adapter);
    }
    return adapters;
}

ComPtr<ID3D11Device> device_on(IDXGIAdapter1* adapter) {
    ComPtr<ID3D11Device> device;
    D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
    const HRESULT hr =
        D3D11CreateDevice(adapter, adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE,
                          nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT, levels, _countof(levels),
                          D3D11_SDK_VERSION, &device, nullptr, nullptr);
    if (FAILED(hr)) return nullptr;
    return device;
}

// A shared NT-handle texture shaped like the one the viewer presents.
HANDLE make_shared_texture(ID3D11Device* device, ComPtr<ID3D11Texture2D>& out_tex) {
    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = 64;
    desc.Height = 64;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE;
    desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE;
    if (FAILED(device->CreateTexture2D(&desc, nullptr, &out_tex))) return nullptr;

    ComPtr<IDXGIResource1> resource;
    if (FAILED(out_tex.As(&resource))) return nullptr;
    HANDLE handle = nullptr;
    if (FAILED(resource->CreateSharedHandle(
            nullptr, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, nullptr, &handle))) {
        return nullptr;
    }
    return handle;
}

}  // namespace

// The LUID a consumer is told to use must be the adapter the device is really
// created on. Both answers come from one selection rule; this catches the day
// someone gives them two.
TEST(GraphicsDevice, ReportedAdapterLuidIsTheOneTheDeviceUses) {
    if (running_under_ci()) GTEST_SKIP() << "needs a GPU";

    const uint64_t advertised = video_adapter_luid();
    ASSERT_NE(advertised, 0u) << "no usable adapter on this machine";

    const GraphicsDevice device = create_d3d11_device();
    ASSERT_NE(device.handle, nullptr);
    EXPECT_EQ(device.adapter_luid, advertised);
}

// The contract the DComp presenter depends on: a device created on the
// advertised adapter can open libmello's shared textures.
TEST(GraphicsDevice, SharedTextureOpensOnTheAdvertisedAdapter) {
    if (running_under_ci()) GTEST_SKIP() << "needs a GPU";

    const GraphicsDevice source = create_d3d11_device();
    ASSERT_NE(source.handle, nullptr);

    ComPtr<ID3D11Texture2D> texture;
    HANDLE shared = make_shared_texture(source.d3d11(), texture);
    ASSERT_NE(shared, nullptr);

    ComPtr<IDXGIAdapter1> advertised;
    for (auto& adapter : hardware_adapters()) {
        DXGI_ADAPTER_DESC1 desc{};
        adapter->GetDesc1(&desc);
        if (luid_of(desc) == video_adapter_luid()) advertised = adapter;
    }
    ASSERT_NE(advertised.Get(), nullptr) << "video_adapter_luid() names no enumerated adapter";

    ComPtr<ID3D11Device> consumer = device_on(advertised.Get());
    ASSERT_NE(consumer.Get(), nullptr);
    ComPtr<ID3D11Device1> consumer1;
    ASSERT_HRESULT_SUCCEEDED(consumer.As(&consumer1));

    ComPtr<ID3D11Texture2D> opened;
    EXPECT_HRESULT_SUCCEEDED(consumer1->OpenSharedResource1(shared, IID_PPV_ARGS(&opened)));
    CloseHandle(shared);
}

// Why the presenter may not let D3D11 choose for it. On a machine with two
// GPUs, a device on the wrong adapter fails every open with E_INVALIDARG, and
// the viewer shows black while the decoder reports healthy frames. Skips where
// there is only one adapter to choose from.
TEST(GraphicsDevice, SharedTextureDoesNotOpenOnADifferentAdapter) {
    if (running_under_ci()) GTEST_SKIP() << "needs a GPU";

    auto adapters = hardware_adapters();
    if (adapters.size() < 2) GTEST_SKIP() << "needs a machine with two GPUs";

    const GraphicsDevice source = create_d3d11_device();
    ASSERT_NE(source.handle, nullptr);

    ComPtr<ID3D11Texture2D> texture;
    HANDLE shared = make_shared_texture(source.d3d11(), texture);
    ASSERT_NE(shared, nullptr);

    ComPtr<IDXGIAdapter1> other;
    for (auto& adapter : adapters) {
        DXGI_ADAPTER_DESC1 desc{};
        adapter->GetDesc1(&desc);
        if (luid_of(desc) != source.adapter_luid) other = adapter;
    }
    ASSERT_NE(other.Get(), nullptr);

    ComPtr<ID3D11Device> consumer = device_on(other.Get());
    ASSERT_NE(consumer.Get(), nullptr);
    ComPtr<ID3D11Device1> consumer1;
    ASSERT_HRESULT_SUCCEEDED(consumer.As(&consumer1));

    ComPtr<ID3D11Texture2D> opened;
    EXPECT_HRESULT_FAILED(consumer1->OpenSharedResource1(shared, IID_PPV_ARGS(&opened)));
    CloseHandle(shared);
}

#endif  // _WIN32
