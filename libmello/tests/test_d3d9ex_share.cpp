// Spike for the D3D9 shared-texture path.
//
// A game on a D3D9Ex device can publish a shared texture the 64-bit client
// opens directly, with no read-back stall and no CPU copy. A game on a plain
// D3D9 device cannot share at all. This test proves the interop the GPU path
// needs: create a shared texture on a D3D9Ex device, open its handle on the
// client's D3D11 device, and read back the pixels the D3D9 side drew.
//
// RED: the hook still always takes the memory path (`hook_d3d9.cpp` builds
// read-back surfaces and publishes CPU_COPY). When this test passes, GREEN is
// the hook publishing real shared handles for Ex games with the memory path
// kept as the fallback for plain D3D9.
//
// It needs a GPU and a desktop session, so it skips under CI like the other
// GPU tests in this suite.

#include <gtest/gtest.h>

#ifdef _WIN32

#include <windows.h>

#include <d3d9.h>
#include <d3d11.h>
#include <wrl/client.h>

#include <cstdlib>
#include <string>

#include "video/graphics_device.hpp"

namespace {

bool running_under_ci() {
    const char* ci = std::getenv("CI");
    return ci && *ci && std::string(ci) != "0" && std::string(ci) != "false";
}

} // namespace

// A D3D9Ex shared texture opens on the D3D11 device and carries pixels.
TEST(D3D9ExShare, SharedTextureOpensOnD3D11WithTheRightPixels) {
    if (running_under_ci()) GTEST_SKIP() << "needs a GPU and a desktop session";

    Microsoft::WRL::ComPtr<IDirect3D9Ex> d3d;
    HRESULT hr = Direct3DCreate9Ex(D3D_SDK_VERSION, &d3d);
    if (FAILED(hr)) GTEST_SKIP() << "no D3D9Ex on this machine";
    ASSERT_NE(d3d.Get(), nullptr);

    // Client side first: the D3D11 device picks the adapter with the most
    // video memory, which need not be D3DADAPTER_DEFAULT. A shared texture
    // is only valid on the adapter that made it, so the D3D9Ex device must
    // be created on the same LUID (the laptop black-viewer lesson).
    mello::video::GraphicsDevice graphics = mello::video::create_d3d11_device();
    ID3D11Device* d3d11 = graphics.d3d11();
    ASSERT_NE(d3d11, nullptr);

    UINT d3d9_adapter = D3DADAPTER_DEFAULT;
    {
        const UINT count = d3d->GetAdapterCount();
        bool matched = false;
        for (UINT i = 0; i < count; ++i) {
            LUID luid{};
            if (FAILED(d3d->GetAdapterLUID(i, &luid))) continue;
            const uint64_t luid64 = (static_cast<uint64_t>(static_cast<uint32_t>(luid.HighPart))
                                     << 32) |
                                    static_cast<uint64_t>(static_cast<uint32_t>(luid.LowPart));
            if (luid64 == graphics.adapter_luid) {
                d3d9_adapter = i;
                matched = true;
                break;
            }
        }
        ASSERT_TRUE(matched) << "no D3D9 adapter matches the D3D11 LUID";
    }

    D3DPRESENT_PARAMETERS present{};
    present.Windowed = TRUE;
    present.SwapEffect = D3DSWAPEFFECT_DISCARD;
    present.hDeviceWindow = GetDesktopWindow();
    present.BackBufferFormat = D3DFMT_UNKNOWN;

    Microsoft::WRL::ComPtr<IDirect3DDevice9Ex> device;
    hr = d3d->CreateDeviceEx(d3d9_adapter, D3DDEVTYPE_HAL, present.hDeviceWindow,
                             D3DCREATE_HARDWARE_VERTEXPROCESSING | D3DCREATE_MULTITHREADED,
                             &present, nullptr, &device);
    if (FAILED(hr)) GTEST_SKIP() << "no hardware D3D9Ex device";
    ASSERT_NE(device.Get(), nullptr);

    // The shared pair: one texture, one slot. Multisample stays NONE, because
    // only a single-sample surface can be shared; the hook keeps resolving
    // the back buffer into it with StretchRect exactly as it does into the
    // resolve target today.
    constexpr UINT kWidth = 320;
    constexpr UINT kHeight = 180;
    HANDLE shared = nullptr;
    Microsoft::WRL::ComPtr<IDirect3DTexture9> texture;
    hr = device->CreateTexture(kWidth, kHeight, 1, D3DUSAGE_RENDERTARGET, D3DFMT_A8R8G8B8,
                               D3DPOOL_DEFAULT, &texture, &shared);
    ASSERT_HRESULT_SUCCEEDED(hr) << "CreateTexture with a shared handle failed";
    ASSERT_NE(shared, nullptr) << "plain D3D9 ignores the shared handle; Ex must return one";

    Microsoft::WRL::ComPtr<IDirect3DSurface9> surface;
    ASSERT_HRESULT_SUCCEEDED(texture->GetSurfaceLevel(0, &surface));

    // Draw the fakegame colour (64, 128, 191): a channel swap cannot hide.
    ASSERT_HRESULT_SUCCEEDED(device->ColorFill(surface.Get(), nullptr, D3DCOLOR_XRGB(64, 128, 191)));

    // D3D9 queues work asynchronously and the D3D11 side has no lock to wait
    // on, so flush the D3D9 pipe before the other API reads: an event query
    // whose GetData only returns once the fill retired. Proven necessary:
    // without it the texture opens but reads zeros.
    {
        Microsoft::WRL::ComPtr<IDirect3DQuery9> query;
        ASSERT_HRESULT_SUCCEEDED(device->CreateQuery(D3DQUERYTYPE_EVENT, &query));
        ASSERT_HRESULT_SUCCEEDED(query->Issue(D3DISSUE_END));
        BOOL done = FALSE;
        for (int i = 0; i < 200 && query->GetData(&done, sizeof(done), D3DGETDATA_FLUSH) != S_OK;
             ++i) {
            Sleep(5);
        }
        ASSERT_TRUE(done) << "the D3D9 fill never retired";
    }

    // Client side: the shared D3D11 device opens the 32-bit legacy handle,
    // exactly like HookCapture::refresh_textures does for DXGI games.
    Microsoft::WRL::ComPtr<ID3D11Texture2D> opened;
    hr = d3d11->OpenSharedResource(shared, __uuidof(ID3D11Texture2D),
                                   reinterpret_cast<void**>(opened.GetAddressOf()));
    ASSERT_HRESULT_SUCCEEDED(hr) << "D3D11 could not open the D3D9Ex shared handle";

    D3D11_TEXTURE2D_DESC desc{};
    opened->GetDesc(&desc);
    EXPECT_EQ(desc.Width, kWidth);
    EXPECT_EQ(desc.Height, kHeight);

    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
    d3d11->GetImmediateContext(&context);

    D3D11_TEXTURE2D_DESC staging_desc = desc;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    staging_desc.MiscFlags = 0;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> staging;
    ASSERT_HRESULT_SUCCEEDED(d3d11->CreateTexture2D(&staging_desc, nullptr, &staging));
    context->CopyResource(staging.Get(), opened.Get());

    D3D11_MAPPED_SUBRESOURCE mapped{};
    ASSERT_HRESULT_SUCCEEDED(context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped));
    const auto* rows = static_cast<const uint8_t*>(mapped.pData);
    const uint8_t* pixel =
        rows + (desc.Height / 2) * mapped.RowPitch + (desc.Width / 2) * 4;
    // B8G8R8A8_UNORM: memory order is B, G, R.
    EXPECT_EQ(pixel[0], 191);
    EXPECT_EQ(pixel[1], 128);
    EXPECT_EQ(pixel[2], 64);
    context->Unmap(staging.Get(), 0);

    // The test owns this device (GraphicsDevice is a plain handle holder
    // here, not the pipeline's RAII owner).
    d3d11->Release();
}

#endif // _WIN32
