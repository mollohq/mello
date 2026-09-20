// mello-offsets — prints the present-function offsets for this machine.
//
// The hook must never build a probe device inside a game (plan 3.3). This
// helper builds one in a process of its own, reads the addresses out of the
// COM virtual function tables, and prints them as offsets from the module that
// contains them. The client caches the result against the file version of
// dxgi.dll and passes the offsets to the hook through shared memory.
//
// Output is one `key=value` line per fact, on stdout:
//
//   protocol=1
//   dxgi_file_version=10.0.26100.1
//   dxgi_present=0x00012345
//   dxgi_present1=0x00012999
//   dxgi_resize_buffers=0x000124aa
//
// Exit code 0 means every required offset is there. 1 means it failed, and the
// reason is on stderr.

#include <windows.h>

#include <d3d11.h>
#include <d3d9.h>
#include <dxgi1_2.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>

#include "mello_hook_protocol.h"

#pragma comment(lib, "version.lib")
#pragma comment(lib, "d3d9.lib")

namespace {

// Indexes into the IDXGISwapChain virtual function table. The COM ABI fixes
// them: IUnknown first, then IDXGIObject, IDXGIDeviceSubObject, IDXGISwapChain,
// then IDXGISwapChain1. They cannot change without breaking every program on
// Windows, which is why reading them this way is safe.
constexpr int kVtPresent       = 8;   // IDXGISwapChain::Present
constexpr int kVtResizeBuffers = 13;  // IDXGISwapChain::ResizeBuffers
constexpr int kVtPresent1      = 22;  // IDXGISwapChain1::Present1

// The same for Direct3D 9. IDirect3DDevice9 has 119 methods, and
// IDirect3DDevice9Ex adds its own after them, so PresentEx and ResetEx sit
// past the end of the older interface.
constexpr int kVtD3d9Present    = 17;   // IDirect3DDevice9::Present
constexpr int kVtD3d9Reset      = 16;   // IDirect3DDevice9::Reset
constexpr int kVtD3d9PresentEx  = 121;  // IDirect3DDevice9Ex::PresentEx
constexpr int kVtD3d9ResetEx    = 132;  // IDirect3DDevice9Ex::ResetEx
constexpr int kVtD3d9SwapPresent = 3;   // IDirect3DSwapChain9::Present

void* vtable_entry(void* com_object, int index) {
    if (!com_object) return nullptr;
    void** vtable = *reinterpret_cast<void***>(com_object);
    return vtable ? vtable[index] : nullptr;
}

// Finds the module that contains `address` and returns the offset from its
// base. `module_name` gets the file name, so the caller can check that the
// function lives where the hook expects it.
bool offset_in_module(void* address, uint64_t* out_offset, char* module_name, size_t name_size) {
    if (!address) return false;
    HMODULE module = nullptr;
    if (!GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                            static_cast<LPCSTR>(address), &module) ||
        !module) {
        return false;
    }
    char path[MAX_PATH]{};
    if (GetModuleFileNameA(module, path, MAX_PATH) == 0) return false;

    const char* name = std::strrchr(path, '\\');
    std::snprintf(module_name, name_size, "%s", name ? name + 1 : path);

    *out_offset = static_cast<uint64_t>(reinterpret_cast<uint8_t*>(address) -
                                        reinterpret_cast<uint8_t*>(module));
    return true;
}

bool file_version_of(const char* module, char* out, size_t out_size) {
    char path[MAX_PATH]{};
    const HMODULE handle = GetModuleHandleA(module);
    if (!handle || GetModuleFileNameA(handle, path, MAX_PATH) == 0) return false;

    DWORD ignored = 0;
    const DWORD size = GetFileVersionInfoSizeA(path, &ignored);
    if (size == 0) return false;

    void* data = std::malloc(size);
    if (!data) return false;
    bool ok = false;
    if (GetFileVersionInfoA(path, 0, size, data)) {
        VS_FIXEDFILEINFO* info = nullptr;
        UINT len = 0;
        if (VerQueryValueA(data, "\\", reinterpret_cast<void**>(&info), &len) && info) {
            std::snprintf(out, out_size, "%u.%u.%u.%u", HIWORD(info->dwFileVersionMS),
                          LOWORD(info->dwFileVersionMS), HIWORD(info->dwFileVersionLS),
                          LOWORD(info->dwFileVersionLS));
            ok = true;
        }
    }
    std::free(data);
    return ok;
}

HWND create_probe_window() {
    WNDCLASSEXA wc{};
    wc.cbSize = sizeof(wc);
    wc.lpfnWndProc = DefWindowProcA;
    wc.hInstance = GetModuleHandleA(nullptr);
    wc.lpszClassName = "mello_offsets_probe";
    RegisterClassExA(&wc);
    // Never shown. A swap chain needs a real window; a message-only window
    // cannot carry one.
    return CreateWindowExA(0, wc.lpszClassName, "mello", WS_OVERLAPPEDWINDOW, 0, 0, 16, 16,
                           nullptr, nullptr, wc.hInstance, nullptr);
}

void print_offset(const char* key, void* address, const char* expected_module, bool* all_ok) {
    uint64_t offset = 0;
    char module[MAX_PATH]{};
    if (!offset_in_module(address, &offset, module, sizeof(module))) {
        std::fprintf(stderr, "%s: no module contains %p\n", key, address);
        *all_ok = false;
        return;
    }
    if (_stricmp(module, expected_module) != 0) {
        // The hook adds the offset to the module it expects. A function that
        // moved to another module needs a protocol change, not a guess.
        std::fprintf(stderr, "%s lives in %s, not %s\n", key, module, expected_module);
        *all_ok = false;
        return;
    }
    std::printf("%s=0x%llx\n", key, static_cast<unsigned long long>(offset));
}

}  // namespace

int main() {
    const HWND window = create_probe_window();
    if (!window) {
        std::fprintf(stderr, "probe window failed: %lu\n", GetLastError());
        return 1;
    }

    DXGI_SWAP_CHAIN_DESC desc{};
    desc.BufferCount = 2;
    desc.BufferDesc.Width = 16;
    desc.BufferDesc.Height = 16;
    desc.BufferDesc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    desc.OutputWindow = window;
    desc.SampleDesc.Count = 1;
    desc.Windowed = TRUE;
    desc.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;

    IDXGISwapChain* swap = nullptr;
    ID3D11Device* device = nullptr;
    ID3D11DeviceContext* context = nullptr;
    const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1,
                                        D3D_FEATURE_LEVEL_10_0};
    D3D_FEATURE_LEVEL level{};
    HRESULT hr = D3D11CreateDeviceAndSwapChain(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0,
                                               levels, ARRAYSIZE(levels), D3D11_SDK_VERSION,
                                               &desc, &swap, &device, &level, &context);
    if (FAILED(hr)) {
        // A machine with no hardware device still needs offsets: the software
        // renderer builds the same DXGI objects.
        hr = D3D11CreateDeviceAndSwapChain(nullptr, D3D_DRIVER_TYPE_WARP, nullptr, 0, levels,
                                           ARRAYSIZE(levels), D3D11_SDK_VERSION, &desc, &swap,
                                           &device, &level, &context);
    }
    if (FAILED(hr) || !swap) {
        std::fprintf(stderr, "no D3D11 swap chain: hr=0x%08lx\n", static_cast<unsigned long>(hr));
        DestroyWindow(window);
        return 1;
    }

    bool all_ok = true;
    std::printf("protocol=%u\n", MELLO_HOOK_PROTOCOL_VERSION);

    char version[64]{};
    if (file_version_of("dxgi.dll", version, sizeof(version))) {
        std::printf("dxgi_file_version=%s\n", version);
    }

    print_offset("dxgi_present", vtable_entry(swap, kVtPresent), "dxgi.dll", &all_ok);
    print_offset("dxgi_resize_buffers", vtable_entry(swap, kVtResizeBuffers), "dxgi.dll", &all_ok);

    IDXGISwapChain1* swap1 = nullptr;
    if (SUCCEEDED(swap->QueryInterface(__uuidof(IDXGISwapChain1),
                                       reinterpret_cast<void**>(&swap1))) &&
        swap1) {
        bool present1_ok = true;
        print_offset("dxgi_present1", vtable_entry(swap1, kVtPresent1), "dxgi.dll", &present1_ok);
        // Present1 is not on every Windows version this runs on. Its absence
        // costs nothing: games that use it also present through Present.
        swap1->Release();
    }

    if (context) context->Release();
    if (device) device->Release();
    swap->Release();

    // --- Direct3D 9 -----------------------------------------------------------
    // A D3D9Ex device carries both interfaces, so one device gives every offset.
    // A machine with no D3D9Ex still runs D3D9 games, and then only the older
    // offsets come out.
    IDirect3D9Ex* d3d9ex = nullptr;
    if (SUCCEEDED(Direct3DCreate9Ex(D3D_SDK_VERSION, &d3d9ex)) && d3d9ex) {
        D3DPRESENT_PARAMETERS pp{};
        pp.Windowed = TRUE;
        pp.SwapEffect = D3DSWAPEFFECT_DISCARD;
        pp.BackBufferFormat = D3DFMT_UNKNOWN;
        pp.BackBufferWidth = 16;
        pp.BackBufferHeight = 16;
        pp.hDeviceWindow = window;

        IDirect3DDevice9Ex* device9 = nullptr;
        HRESULT hr9 = d3d9ex->CreateDeviceEx(
            D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, window,
            D3DCREATE_SOFTWARE_VERTEXPROCESSING | D3DCREATE_NOWINDOWCHANGES, &pp, nullptr,
            &device9);
        if (SUCCEEDED(hr9) && device9) {
            bool d3d9_ok = true;
            print_offset("d3d9_present", vtable_entry(device9, kVtD3d9Present), "d3d9.dll",
                         &d3d9_ok);
            print_offset("d3d9_reset", vtable_entry(device9, kVtD3d9Reset), "d3d9.dll", &d3d9_ok);
            print_offset("d3d9_present_ex", vtable_entry(device9, kVtD3d9PresentEx), "d3d9.dll",
                         &d3d9_ok);
            print_offset("d3d9_reset_ex", vtable_entry(device9, kVtD3d9ResetEx), "d3d9.dll",
                         &d3d9_ok);

            IDirect3DSwapChain9* swap9 = nullptr;
            if (SUCCEEDED(device9->GetSwapChain(0, &swap9)) && swap9) {
                print_offset("d3d9_swapchain_present", vtable_entry(swap9, kVtD3d9SwapPresent),
                             "d3d9.dll", &d3d9_ok);
                swap9->Release();
            }
            // A failure here is not fatal for the whole run: a machine without
            // D3D9 still hooks DXGI games.
            if (!d3d9_ok) {
                std::fprintf(stderr, "d3d9 offsets are incomplete on this machine\n");
            }
            char version9[64]{};
            if (file_version_of("d3d9.dll", version9, sizeof(version9))) {
                std::printf("d3d9_file_version=%s\n", version9);
            }
            device9->Release();
        } else {
            std::fprintf(stderr, "no D3D9Ex device: hr=0x%08lx\n",
                         static_cast<unsigned long>(hr9));
        }
        d3d9ex->Release();
    }

    DestroyWindow(window);

    return all_ok ? 0 : 1;
}
