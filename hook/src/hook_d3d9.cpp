#include "hook_d3d9.hpp"

#include <windows.h>

#include <d3d9.h>

#include <atomic>
#include <cstring>

#include <detours/detours.h>

#include "hook_log.hpp"
#include "hook_state.hpp"

namespace mello_hook {
namespace {

using PresentFn        = HRESULT(STDMETHODCALLTYPE*)(IDirect3DDevice9*, const RECT*, const RECT*,
                                                     HWND, const RGNDATA*);
using PresentExFn      = HRESULT(STDMETHODCALLTYPE*)(IDirect3DDevice9Ex*, const RECT*, const RECT*,
                                                     HWND, const RGNDATA*, DWORD);
using SwapPresentFn    = HRESULT(STDMETHODCALLTYPE*)(IDirect3DSwapChain9*, const RECT*,
                                                     const RECT*, HWND, const RGNDATA*, DWORD);
using ResetFn          = HRESULT(STDMETHODCALLTYPE*)(IDirect3DDevice9*, D3DPRESENT_PARAMETERS*);
using ResetExFn        = HRESULT(STDMETHODCALLTYPE*)(IDirect3DDevice9Ex*, D3DPRESENT_PARAMETERS*,
                                                     D3DDISPLAYMODEEX*);

PresentFn     g_real_present      = nullptr;
PresentExFn   g_real_present_ex   = nullptr;
SwapPresentFn g_real_swap_present = nullptr;
ResetFn       g_real_reset        = nullptr;
ResetExFn     g_real_reset_ex     = nullptr;

std::atomic<bool> g_disabled{false};
std::atomic<bool> g_in_capture{false};

// Everything below belongs to the game's present thread.
struct Resources {
    IDirect3DDevice9*  device      = nullptr;   // not owned; compared only
    IDirect3DSurface9* readback    = nullptr;   // system memory, one per slot
    IDirect3DSurface9* readback2   = nullptr;
    HANDLE             mapping     = nullptr;   // the frame block
    uint8_t*           frames      = nullptr;   // MELLO_HOOK_TEXTURE_COUNT slots
    uint32_t           frame_bytes = 0;
    uint32_t           pitch       = 0;
    UINT               width       = 0;
    UINT               height      = 0;
    D3DFORMAT          format      = D3DFMT_UNKNOWN;
    uint32_t           next        = 0;
    bool               ready       = false;
};

Resources g_res;

void release_resources() {
    if (g_res.readback) {
        g_res.readback->Release();
        g_res.readback = nullptr;
    }
    if (g_res.readback2) {
        g_res.readback2->Release();
        g_res.readback2 = nullptr;
    }
    if (g_res.frames) {
        UnmapViewOfFile(g_res.frames);
        g_res.frames = nullptr;
    }
    if (g_res.mapping) {
        CloseHandle(g_res.mapping);
        g_res.mapping = nullptr;
    }
    g_res.device = nullptr;
    g_res.frame_bytes = 0;
    g_res.pitch = 0;
    g_res.width = 0;
    g_res.height = 0;
    g_res.format = D3DFMT_UNKNOWN;
    g_res.next = 0;
    g_res.ready = false;
}

// The client works in DXGI formats. These two are what a game's back buffer
// carries; anything else the hook refuses rather than sending wrong colours.
uint32_t dxgi_format_of(D3DFORMAT format) {
    switch (format) {
        case D3DFMT_A8R8G8B8:
        case D3DFMT_X8R8G8B8:
            return 87;  // DXGI_FORMAT_B8G8R8A8_UNORM
        default:
            return 0;
    }
}

// Builds the read-back surfaces and the shared frame block for this device.
bool build_resources(IDirect3DDevice9* device, const D3DSURFACE_DESC& desc) {
    release_resources();

    const uint32_t dxgi_format = dxgi_format_of(desc.Format);
    if (dxgi_format == 0) {
        log_line("back buffer format %u is not one the client can show", desc.Format);
        HookState::instance().set_error(MELLO_HOOK_ERR_FORMAT);
        return false;
    }

    // A plain offscreen surface in system memory is what GetRenderTargetData
    // fills. Two of them, so a read-back can start while the client still holds
    // the other frame.
    if (FAILED(device->CreateOffscreenPlainSurface(desc.Width, desc.Height, desc.Format,
                                                   D3DPOOL_SYSTEMMEM, &g_res.readback, nullptr)) ||
        FAILED(device->CreateOffscreenPlainSurface(desc.Width, desc.Height, desc.Format,
                                                   D3DPOOL_SYSTEMMEM, &g_res.readback2,
                                                   nullptr))) {
        log_line("read-back surfaces %ux%u failed", desc.Width, desc.Height);
        HookState::instance().set_error(MELLO_HOOK_ERR_SHARED_TEXTURE);
        release_resources();
        return false;
    }

    g_res.pitch = desc.Width * 4;
    g_res.frame_bytes = g_res.pitch * desc.Height;

    char name[64];
    object_name(name, sizeof(name), MELLO_HOOK_NAME_FRAMES, GetCurrentProcessId());
    const uint64_t total = static_cast<uint64_t>(g_res.frame_bytes) * MELLO_HOOK_TEXTURE_COUNT;
    g_res.mapping = CreateFileMappingA(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE,
                                       static_cast<DWORD>(total >> 32),
                                       static_cast<DWORD>(total & 0xFFFFFFFF), name);
    if (!g_res.mapping) {
        log_line("frame block of %llu bytes failed: %lu", total, GetLastError());
        HookState::instance().set_error(MELLO_HOOK_ERR_SHARED_TEXTURE);
        release_resources();
        return false;
    }
    g_res.frames = static_cast<uint8_t*>(
        MapViewOfFile(g_res.mapping, FILE_MAP_ALL_ACCESS, 0, 0, static_cast<SIZE_T>(total)));
    if (!g_res.frames) {
        log_line("frame block view failed: %lu", GetLastError());
        HookState::instance().set_error(MELLO_HOOK_ERR_SHARED_TEXTURE);
        release_resources();
        return false;
    }

    g_res.device = device;
    g_res.width = desc.Width;
    g_res.height = desc.Height;
    g_res.format = desc.Format;
    g_res.next = 0;
    g_res.ready = true;

    MelloHookInfo* info = HookState::instance().info();
    if (info) {
        info->cpu_frame_bytes = g_res.frame_bytes;
        info->cpu_pitch = g_res.pitch;
    }
    const uint32_t handles[MELLO_HOOK_TEXTURE_COUNT] = {0, 0};
    HookState::instance().publish_description(MELLO_HOOK_API_D3D9, desc.Width, desc.Height,
                                              dxgi_format, 0, handles, MELLO_HOOK_FLAG_CPU_COPY);
    HookState::instance().set_error(MELLO_HOOK_OK);
    log_line("capturing D3D9 %ux%u fmt=%u through memory, %u bytes a frame", desc.Width,
             desc.Height, desc.Format, g_res.frame_bytes);
    return true;
}

// The body of the present hooks. Runs on the game's render thread.
void capture_present(IDirect3DDevice9* device) {
    HookState& state = HookState::instance();
    if (!state.capture_wanted()) {
        if (g_res.ready) {
            log_line("client stopped asking for frames; releasing the D3D9 resources");
            release_resources();
        }
        return;
    }

    IDirect3DSurface9* back = nullptr;
    if (FAILED(device->GetBackBuffer(0, 0, D3DBACKBUFFER_TYPE_MONO, &back)) || !back) {
        state.count_drop();
        return;
    }

    D3DSURFACE_DESC desc{};
    if (FAILED(back->GetDesc(&desc))) {
        back->Release();
        return;
    }

    const bool changed = !g_res.ready || g_res.device != device || g_res.width != desc.Width ||
                         g_res.height != desc.Height || g_res.format != desc.Format;
    if (changed && !build_resources(device, desc)) {
        back->Release();
        return;
    }

    const uint32_t slot = g_res.next;
    IDirect3DSurface9* readback = slot == 0 ? g_res.readback : g_res.readback2;

    // The read back costs a stall on the render thread. It is what plan 3.2
    // asks for first: it works on every D3D9 device, including the plain ones
    // that cannot share a surface at all.
    const HRESULT hr = device->GetRenderTargetData(back, readback);
    back->Release();
    if (FAILED(hr)) {
        state.count_drop();
        // A multisampled back buffer cannot be read back. Say so once: the
        // client turns it into a ladder move, not an error for the user.
        state.set_error(hr == D3DERR_INVALIDCALL ? MELLO_HOOK_ERR_MULTISAMPLED
                                                 : MELLO_HOOK_ERR_COPY);
        return;
    }

    D3DLOCKED_RECT locked{};
    if (FAILED(readback->LockRect(&locked, nullptr, D3DLOCK_READONLY))) {
        state.count_drop();
        return;
    }
    uint8_t* target = g_res.frames + static_cast<size_t>(slot) * g_res.frame_bytes;
    const auto* source = static_cast<const uint8_t*>(locked.pBits);
    if (static_cast<uint32_t>(locked.Pitch) == g_res.pitch) {
        std::memcpy(target, source, g_res.frame_bytes);
    } else {
        for (UINT row = 0; row < g_res.height; ++row) {
            std::memcpy(target + static_cast<size_t>(row) * g_res.pitch,
                        source + static_cast<size_t>(row) * locked.Pitch, g_res.pitch);
        }
    }
    readback->UnlockRect();

    g_res.next = (slot + 1) % MELLO_HOOK_TEXTURE_COUNT;
    state.publish_frame(slot, static_cast<uint64_t>(qpc_now()));
    state.signal_frame();
}

void note_fault(const char* where) {
    if (!g_disabled.exchange(true)) {
        HookState::instance().count_fault();
        HookState::instance().set_error(MELLO_HOOK_ERR_EXCEPTION);
        log_line("fault in %s; D3D9 capture is off and the game keeps running", where);
    }
    release_resources();
}

void guarded_capture(IDirect3DDevice9* device, const char* where) {
    if (g_disabled.load(std::memory_order_relaxed)) return;
    if (g_in_capture.exchange(true, std::memory_order_acquire)) return;
    __try {
        capture_present(device);
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        note_fault(where);
    }
    g_in_capture.store(false, std::memory_order_release);
}

// A reset throws away every resource in the default pool and can change the
// back buffer size. Ours live in system memory, but they are the wrong size
// afterwards, so drop them and let the next present rebuild.
void guarded_release(const char* where) {
    if (g_disabled.load(std::memory_order_relaxed)) return;
    if (g_in_capture.exchange(true, std::memory_order_acquire)) return;
    __try {
        release_resources();
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        note_fault(where);
    }
    g_in_capture.store(false, std::memory_order_release);
}

HRESULT STDMETHODCALLTYPE hooked_present(IDirect3DDevice9* device, const RECT* source,
                                         const RECT* dest, HWND window, const RGNDATA* dirty) {
    guarded_capture(device, "D3D9 Present");
    return g_real_present(device, source, dest, window, dirty);
}

HRESULT STDMETHODCALLTYPE hooked_present_ex(IDirect3DDevice9Ex* device, const RECT* source,
                                            const RECT* dest, HWND window, const RGNDATA* dirty,
                                            DWORD flags) {
    guarded_capture(device, "D3D9 PresentEx");
    return g_real_present_ex(device, source, dest, window, dirty, flags);
}

HRESULT STDMETHODCALLTYPE hooked_swap_present(IDirect3DSwapChain9* swap, const RECT* source,
                                              const RECT* dest, HWND window, const RGNDATA* dirty,
                                              DWORD flags) {
    if (!g_disabled.load(std::memory_order_relaxed)) {
        IDirect3DDevice9* device = nullptr;
        if (SUCCEEDED(swap->GetDevice(&device)) && device) {
            guarded_capture(device, "D3D9 SwapChain::Present");
            device->Release();
        }
    }
    return g_real_swap_present(swap, source, dest, window, dirty, flags);
}

HRESULT STDMETHODCALLTYPE hooked_reset(IDirect3DDevice9* device, D3DPRESENT_PARAMETERS* params) {
    guarded_release("D3D9 Reset");
    return g_real_reset(device, params);
}

HRESULT STDMETHODCALLTYPE hooked_reset_ex(IDirect3DDevice9Ex* device,
                                          D3DPRESENT_PARAMETERS* params, D3DDISPLAYMODEEX* mode) {
    guarded_release("D3D9 ResetEx");
    return g_real_reset_ex(device, params, mode);
}

template <typename Fn>
bool resolve(uint64_t offset, Fn* out) {
    if (offset == 0) return false;
    const HMODULE base = GetModuleHandleA("d3d9.dll");
    if (!base) return false;
    *out = reinterpret_cast<Fn>(reinterpret_cast<uint8_t*>(base) + offset);
    return true;
}

}  // namespace

bool install_d3d9_hooks(const MelloHookInfo& info) {
    if (!info.offsets_valid || !GetModuleHandleA("d3d9.dll")) return false;
    if (!resolve(info.off_d3d9_present, &g_real_present)) return false;

    resolve(info.off_d3d9_present_ex, &g_real_present_ex);
    resolve(info.off_d3d9_swapchain_present, &g_real_swap_present);
    resolve(info.off_d3d9_reset, &g_real_reset);
    resolve(info.off_d3d9_reset_ex, &g_real_reset_ex);

    DetourTransactionBegin();
    DetourUpdateThread(GetCurrentThread());
    DetourAttach(reinterpret_cast<PVOID*>(&g_real_present),
                 reinterpret_cast<PVOID>(hooked_present));
    if (g_real_present_ex) {
        DetourAttach(reinterpret_cast<PVOID*>(&g_real_present_ex),
                     reinterpret_cast<PVOID>(hooked_present_ex));
    }
    if (g_real_swap_present) {
        DetourAttach(reinterpret_cast<PVOID*>(&g_real_swap_present),
                     reinterpret_cast<PVOID>(hooked_swap_present));
    }
    if (g_real_reset) {
        DetourAttach(reinterpret_cast<PVOID*>(&g_real_reset), reinterpret_cast<PVOID>(hooked_reset));
    }
    if (g_real_reset_ex) {
        DetourAttach(reinterpret_cast<PVOID*>(&g_real_reset_ex),
                     reinterpret_cast<PVOID>(hooked_reset_ex));
    }
    const LONG result = DetourTransactionCommit();
    if (result != NO_ERROR) {
        HookState::instance().set_error(MELLO_HOOK_ERR_DETOUR);
        log_line("Detours refused the D3D9 transaction: %ld", result);
        return false;
    }

    log_line("hooked Present%s%s in d3d9.dll", g_real_present_ex ? ", PresentEx" : "",
             g_real_swap_present ? ", SwapChain::Present" : "");
    return true;
}

void stop_d3d9_capture() {
    // As with DXGI: the resources belong to the present thread, and it releases
    // them when it sees that the client stopped asking.
    HookState::instance().set_error(MELLO_HOOK_OK);
}

}  // namespace mello_hook
