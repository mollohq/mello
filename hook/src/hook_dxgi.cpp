#include "hook_dxgi.hpp"

#include <windows.h>

#include <d3d11.h>
#include <dxgi1_2.h>

#include <atomic>
#include <cstring>

#include <detours/detours.h>

#include "hook_log.hpp"
#include "hook_state.hpp"

namespace mello_hook {
namespace {

using PresentFn       = HRESULT(STDMETHODCALLTYPE*)(IDXGISwapChain*, UINT, UINT);
using Present1Fn      = HRESULT(STDMETHODCALLTYPE*)(IDXGISwapChain1*, UINT, UINT,
                                                    const DXGI_PRESENT_PARAMETERS*);
using ResizeBuffersFn = HRESULT(STDMETHODCALLTYPE*)(IDXGISwapChain*, UINT, UINT, UINT,
                                                    DXGI_FORMAT, UINT);

PresentFn       g_real_present        = nullptr;
Present1Fn      g_real_present1       = nullptr;
ResizeBuffersFn g_real_resize_buffers = nullptr;

// Set when a detour body faults. From then on the hook does nothing and the
// game keeps running on the original functions.
std::atomic<bool> g_disabled{false};

// One thread at a time in the capture body. It keeps two present threads out of
// each other's way, and it stops the second capture when a DXGI implementation
// calls Present from inside Present1.
std::atomic<bool> g_in_capture{false};

// --- Capture resources -------------------------------------------------------
//
// Raw COM pointers on purpose: the functions that touch them sit under a
// structured exception guard, which cannot live in a function that needs C++
// unwinding. Every path releases them by hand.

struct Resources {
    ID3D11Device*        device   = nullptr;
    ID3D11DeviceContext* context  = nullptr;
    ID3D11Texture2D*     texture[MELLO_HOOK_TEXTURE_COUNT]{};
    uint32_t             handle[MELLO_HOOK_TEXTURE_COUNT]{};
    IDXGISwapChain*      swap     = nullptr;   // not owned, compared only
    UINT                 width    = 0;
    UINT                 height   = 0;
    DXGI_FORMAT          format   = DXGI_FORMAT_UNKNOWN;
    uint32_t             next     = 0;
    bool                 ready    = false;
};

Resources g_res;

void release_resources() {
    for (uint32_t i = 0; i < MELLO_HOOK_TEXTURE_COUNT; ++i) {
        if (g_res.texture[i]) {
            g_res.texture[i]->Release();
            g_res.texture[i] = nullptr;
        }
        g_res.handle[i] = 0;
    }
    if (g_res.context) {
        g_res.context->Release();
        g_res.context = nullptr;
    }
    if (g_res.device) {
        g_res.device->Release();
        g_res.device = nullptr;
    }
    g_res.swap = nullptr;
    g_res.width = 0;
    g_res.height = 0;
    g_res.format = DXGI_FORMAT_UNKNOWN;
    g_res.next = 0;
    g_res.ready = false;
}

bool format_is_hdr(DXGI_FORMAT f) {
    return f == DXGI_FORMAT_R16G16B16A16_FLOAT || f == DXGI_FORMAT_R10G10B10A2_UNORM;
}

bool format_is_srgb(DXGI_FORMAT f) {
    return f == DXGI_FORMAT_R8G8B8A8_UNORM_SRGB || f == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB;
}

uint64_t adapter_luid_of(ID3D11Device* device) {
    IDXGIDevice* dxgi_device = nullptr;
    if (FAILED(device->QueryInterface(__uuidof(IDXGIDevice),
                                      reinterpret_cast<void**>(&dxgi_device)))) {
        return 0;
    }
    IDXGIAdapter* adapter = nullptr;
    uint64_t luid = 0;
    if (SUCCEEDED(dxgi_device->GetAdapter(&adapter)) && adapter) {
        DXGI_ADAPTER_DESC desc{};
        if (SUCCEEDED(adapter->GetDesc(&desc))) {
            luid = (static_cast<uint64_t>(static_cast<uint32_t>(desc.AdapterLuid.HighPart)) << 32) |
                   static_cast<uint32_t>(desc.AdapterLuid.LowPart);
        }
        adapter->Release();
    }
    dxgi_device->Release();
    return luid;
}

// Builds the pair of shared textures for this swap chain. Returns false and
// sets `last_error` on any failure; the caller then leaves capture off for this
// swap chain instead of trying again on every present.
bool build_resources(IDXGISwapChain* swap, const DXGI_SWAP_CHAIN_DESC& desc) {
    release_resources();

    ID3D11Device* device = nullptr;
    if (FAILED(swap->GetDevice(__uuidof(ID3D11Device), reinterpret_cast<void**>(&device))) ||
        !device) {
        // A D3D12 or D3D9 swap chain. Those have their own hooks.
        HookState::instance().set_error(MELLO_HOOK_ERR_NO_DEVICE);
        return false;
    }

    if (desc.SampleDesc.Count > 1) {
        // A multisampled back buffer needs a resolve into a matching
        // single-sample texture. Handled below by ResolveSubresource, which
        // needs the same format, so nothing else changes here.
        log_line("back buffer is multisampled (%u samples); frames are resolved",
                 desc.SampleDesc.Count);
    }

    D3D11_TEXTURE2D_DESC td{};
    td.Width = desc.BufferDesc.Width;
    td.Height = desc.BufferDesc.Height;
    td.MipLevels = 1;
    td.ArraySize = 1;
    td.Format = desc.BufferDesc.Format;
    td.SampleDesc.Count = 1;
    td.SampleDesc.Quality = 0;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET;
    // The legacy shared flag, not the NT-handle one: a legacy handle is a
    // 32-bit value that opens in the 64-bit client with no DuplicateHandle,
    // including from a 32-bit game (plan 3.5).
    td.MiscFlags = D3D11_RESOURCE_MISC_SHARED;

    uint32_t handles[MELLO_HOOK_TEXTURE_COUNT]{};
    for (uint32_t i = 0; i < MELLO_HOOK_TEXTURE_COUNT; ++i) {
        ID3D11Texture2D* texture = nullptr;
        if (FAILED(device->CreateTexture2D(&td, nullptr, &texture)) || !texture) {
            log_line("shared texture %u of %ux%u fmt=%u failed", i, td.Width, td.Height,
                     static_cast<unsigned>(td.Format));
            HookState::instance().set_error(MELLO_HOOK_ERR_SHARED_TEXTURE);
            device->Release();
            release_resources();
            return false;
        }
        IDXGIResource* resource = nullptr;
        HANDLE shared = nullptr;
        if (FAILED(texture->QueryInterface(__uuidof(IDXGIResource),
                                           reinterpret_cast<void**>(&resource))) ||
            FAILED(resource->GetSharedHandle(&shared)) || !shared) {
            if (resource) resource->Release();
            texture->Release();
            HookState::instance().set_error(MELLO_HOOK_ERR_SHARED_TEXTURE);
            device->Release();
            release_resources();
            return false;
        }
        resource->Release();
        g_res.texture[i] = texture;
        // A legacy shared handle fits in 32 bits by contract.
        handles[i] = static_cast<uint32_t>(reinterpret_cast<uintptr_t>(shared));
        g_res.handle[i] = handles[i];
    }

    device->GetImmediateContext(&g_res.context);
    g_res.device = device;  // reference passed on from GetDevice
    g_res.swap = swap;
    g_res.width = td.Width;
    g_res.height = td.Height;
    g_res.format = td.Format;
    g_res.next = 0;
    g_res.ready = true;

    uint32_t flags = 0;
    if (format_is_hdr(td.Format)) flags |= MELLO_HOOK_FLAG_HDR;
    if (format_is_srgb(td.Format)) flags |= MELLO_HOOK_FLAG_SRGB;

    HookState::instance().publish_description(MELLO_HOOK_API_D3D11, td.Width, td.Height,
                                              static_cast<uint32_t>(td.Format),
                                              adapter_luid_of(device), handles, flags);
    HookState::instance().set_error(MELLO_HOOK_OK);
    log_line("capturing %ux%u fmt=%u handles=%08x,%08x", td.Width, td.Height,
             static_cast<unsigned>(td.Format), handles[0], handles[1]);
    return true;
}

// True when this swap chain is the one to capture. A game can present several:
// take the largest back buffer, which is the game's own output rather than a
// launcher or an overlay.
bool is_target_swap_chain(IDXGISwapChain* swap, const DXGI_SWAP_CHAIN_DESC& desc) {
    if (!g_res.ready || g_res.swap == swap) return true;
    const uint64_t candidate = static_cast<uint64_t>(desc.BufferDesc.Width) * desc.BufferDesc.Height;
    const uint64_t current = static_cast<uint64_t>(g_res.width) * g_res.height;
    return candidate > current;
}

// The body of the present hook. Runs on the game's present thread.
void capture_present(IDXGISwapChain* swap) {
    HookState& state = HookState::instance();
    if (!state.capture_wanted()) {
        if (g_res.ready) {
            log_line("client stopped asking for frames; releasing capture resources");
            release_resources();
        }
        return;
    }

    DXGI_SWAP_CHAIN_DESC desc{};
    if (FAILED(swap->GetDesc(&desc))) return;
    if (desc.BufferDesc.Width == 0 || desc.BufferDesc.Height == 0) return;
    if (!is_target_swap_chain(swap, desc)) return;

    const bool changed = !g_res.ready || g_res.swap != swap ||
                         g_res.width != desc.BufferDesc.Width ||
                         g_res.height != desc.BufferDesc.Height ||
                         g_res.format != desc.BufferDesc.Format;
    if (changed && !build_resources(swap, desc)) {
        return;
    }

    ID3D11Texture2D* back = nullptr;
    if (FAILED(swap->GetBuffer(0, __uuidof(ID3D11Texture2D),
                               reinterpret_cast<void**>(&back))) ||
        !back) {
        state.count_drop();
        return;
    }

    const uint32_t index = g_res.next;
    if (desc.SampleDesc.Count > 1) {
        g_res.context->ResolveSubresource(g_res.texture[index], 0, back, 0, g_res.format);
    } else {
        g_res.context->CopyResource(g_res.texture[index], back);
    }
    back->Release();

    // The copy must reach the GPU before the client reads the texture. Flush
    // costs less than the copy itself and keeps the frame from sitting in the
    // game's command buffer until its next submit.
    g_res.context->Flush();

    g_res.next = (index + 1) % MELLO_HOOK_TEXTURE_COUNT;
    state.publish_frame(index, static_cast<uint64_t>(qpc_now()));
    state.signal_frame();
}

void note_fault(const char* where) {
    if (!g_disabled.exchange(true)) {
        HookState::instance().count_fault();
        HookState::instance().set_error(MELLO_HOOK_ERR_EXCEPTION);
        log_line("fault in %s; capture is off and the game keeps running", where);
    }
    release_resources();
}

// Runs `capture_present` with the reentrancy guard and the exception guard. A
// fault here disables capture; it never reaches the game.
void guarded_capture(IDXGISwapChain* swap, const char* where) {
    if (g_disabled.load(std::memory_order_relaxed)) return;
    if (g_in_capture.exchange(true, std::memory_order_acquire)) return;
    __try {
        capture_present(swap);
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        note_fault(where);
    }
    g_in_capture.store(false, std::memory_order_release);
}

HRESULT STDMETHODCALLTYPE hooked_present(IDXGISwapChain* swap, UINT sync, UINT flags) {
    guarded_capture(swap, "Present");
    return g_real_present(swap, sync, flags);
}

HRESULT STDMETHODCALLTYPE hooked_present1(IDXGISwapChain1* swap, UINT sync, UINT flags,
                                          const DXGI_PRESENT_PARAMETERS* params) {
    guarded_capture(swap, "Present1");
    return g_real_present1(swap, sync, flags, params);
}

HRESULT STDMETHODCALLTYPE hooked_resize_buffers(IDXGISwapChain* swap, UINT count, UINT width,
                                                UINT height, DXGI_FORMAT format, UINT flags) {
    // Our textures are our own, so they never block the resize. They do become
    // the wrong size, so drop them and let the next present build new ones. The
    // guard keeps this away from a present on another thread.
    if (!g_disabled.load(std::memory_order_relaxed) &&
        !g_in_capture.exchange(true, std::memory_order_acquire)) {
        __try {
            if (g_res.swap == swap) release_resources();
        } __except (EXCEPTION_EXECUTE_HANDLER) {
            note_fault("ResizeBuffers");
        }
        g_in_capture.store(false, std::memory_order_release);
    }
    return g_real_resize_buffers(swap, count, width, height, format, flags);
}

template <typename Fn>
bool resolve(const char* module, uint64_t offset, Fn* out) {
    if (offset == 0) return false;
    const HMODULE base = GetModuleHandleA(module);
    if (!base) return false;
    *out = reinterpret_cast<Fn>(reinterpret_cast<uint8_t*>(base) + offset);
    return true;
}

}  // namespace

bool install_dxgi_hooks(const MelloHookInfo& info) {
    if (!info.offsets_valid) {
        HookState::instance().set_error(MELLO_HOOK_ERR_NO_OFFSETS);
        log_line("no present offsets from the client; nothing is hooked");
        return false;
    }

    const bool has_present = resolve("dxgi.dll", info.off_dxgi_present, &g_real_present);
    if (!has_present) {
        HookState::instance().set_error(MELLO_HOOK_ERR_NO_OFFSETS);
        log_line("dxgi.dll is not loaded in this process, or Present has no offset");
        return false;
    }
    resolve("dxgi.dll", info.off_dxgi_present1, &g_real_present1);
    resolve("dxgi.dll", info.off_dxgi_resize_buffers, &g_real_resize_buffers);

    DetourTransactionBegin();
    DetourUpdateThread(GetCurrentThread());
    DetourAttach(reinterpret_cast<PVOID*>(&g_real_present),
                 reinterpret_cast<PVOID>(hooked_present));
    if (g_real_present1) {
        DetourAttach(reinterpret_cast<PVOID*>(&g_real_present1),
                     reinterpret_cast<PVOID>(hooked_present1));
    }
    if (g_real_resize_buffers) {
        DetourAttach(reinterpret_cast<PVOID*>(&g_real_resize_buffers),
                     reinterpret_cast<PVOID>(hooked_resize_buffers));
    }
    const LONG result = DetourTransactionCommit();
    if (result != NO_ERROR) {
        HookState::instance().set_error(MELLO_HOOK_ERR_DETOUR);
        log_line("Detours refused the transaction: %ld", result);
        return false;
    }

    log_line("hooked Present%s%s in dxgi.dll",
             g_real_present1 ? ", Present1" : "",
             g_real_resize_buffers ? ", ResizeBuffers" : "");
    return true;
}

void stop_dxgi_capture() {
    // The resources belong to the present thread. Releasing them from the hook
    // thread would pull a texture out from under a copy in progress, so this
    // only records the intent: `capture_present` sees that the client stopped
    // asking and releases them on the next present. A game that never presents
    // again keeps them until it exits, which costs one texture pair.
    HookState::instance().set_error(MELLO_HOOK_OK);
}

}  // namespace mello_hook
