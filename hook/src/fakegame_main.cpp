// mello-fakegame — a D3D11 program that presents, for testing the hook.
//
// It is the smallest thing that looks like a game to the hook: a window, a DXGI
// swap chain, and a present every frame. It clears the back buffer to one known
// colour, so a test can check that the pixels the hook delivered are the pixels
// this program drew, and not a black frame or somebody else's window.
//
// Usage: mello-fakegame64.exe [--seconds N] [--width W] [--height H] [--d3d9]
//                             [--fullscreen] [--start-delay N]
//
// `--start-delay` shows the window but builds no device until N seconds have
// passed, the way a game looks while it loads. A stream that starts in that
// window finds nothing to capture anywhere, which is what a real game did on
// 2026-09-16.
//
// `--d3d9` presents through Direct3D 9 instead, which is the other path the
// hook covers and the one a 2012-era game uses.
//
// It prints `ready pid=<pid>` when the swap chain is up, so a test can wait for
// that line instead of sleeping.

#include <windows.h>

#include <d3d9.h>
#include <d3d11.h>
#include <dxgi1_2.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>

#pragma comment(lib, "d3d11.lib")
#pragma comment(lib, "d3d9.lib")

namespace {

// The colour the test looks for. Chosen so each channel is different: a test
// that passes cannot have channels swapped.
constexpr float kClearColour[4] = {0.25f, 0.50f, 0.75f, 1.0f};  // R, G, B, A

bool g_running = true;

LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
    if (message == WM_CLOSE || message == WM_DESTROY) {
        g_running = false;
        return 0;
    }
    return DefWindowProcW(window, message, wparam, lparam);
}

int argument(int argc, wchar_t** argv, const wchar_t* name, int fallback) {
    for (int i = 1; i + 1 < argc; ++i) {
        if (wcscmp(argv[i], name) == 0) return _wtoi(argv[i + 1]);
    }
    return fallback;
}

bool flag(int argc, wchar_t** argv, const wchar_t* name) {
    for (int i = 1; i < argc; ++i) {
        if (wcscmp(argv[i], name) == 0) return true;
    }
    return false;
}

// The Direct3D 9 build of the same program: one window, one device, a clear to
// the same colour, and a present every frame.
int run_d3d9(HWND window, int width, int height, int seconds, bool fullscreen,
             int start_delay_seconds) {
    // Nothing is rendered and no device exists yet: the window is up and the
    // process is alive, and that is all a capture method can find.
    const DWORD start_at = GetTickCount() + static_cast<DWORD>(start_delay_seconds) * 1000;
    while (g_running && GetTickCount() < start_at) {
        MSG message;
        while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        Sleep(16);
    }

    IDirect3D9Ex* d3d9 = nullptr;
    if (FAILED(Direct3DCreate9Ex(D3D_SDK_VERSION, &d3d9)) || !d3d9) {
        std::fprintf(stderr, "no Direct3D 9Ex on this machine\n");
        return 1;
    }

    D3DPRESENT_PARAMETERS pp{};
    // Exclusive fullscreen is the case the hook exists for: no screen capture
    // method can see it.
    pp.Windowed = fullscreen ? FALSE : TRUE;
    pp.SwapEffect = D3DSWAPEFFECT_DISCARD;
    pp.BackBufferFormat = D3DFMT_X8R8G8B8;
    if (fullscreen) {
        pp.FullScreen_RefreshRateInHz = D3DPRESENT_RATE_DEFAULT;
    }
    pp.BackBufferWidth = static_cast<UINT>(width);
    pp.BackBufferHeight = static_cast<UINT>(height);
    pp.hDeviceWindow = window;
    pp.PresentationInterval = D3DPRESENT_INTERVAL_ONE;

    // D3D9Ex takes the display mode for a fullscreen device. It refuses the
    // call with D3DERR_INVALIDCALL when this is missing.
    D3DDISPLAYMODEEX mode{};
    mode.Size = sizeof(mode);
    mode.Width = pp.BackBufferWidth;
    mode.Height = pp.BackBufferHeight;
    mode.RefreshRate = 0;
    mode.Format = pp.BackBufferFormat;
    mode.ScanLineOrdering = D3DSCANLINEORDERING_PROGRESSIVE;

    IDirect3DDevice9Ex* device = nullptr;
    const HRESULT hr = d3d9->CreateDeviceEx(
        D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, window,
        D3DCREATE_HARDWARE_VERTEXPROCESSING | D3DCREATE_NOWINDOWCHANGES, &pp,
        fullscreen ? &mode : nullptr, &device);
    if (FAILED(hr) || !device) {
        std::fprintf(stderr, "no D3D9 device: hr=0x%08lx\n", static_cast<unsigned long>(hr));
        d3d9->Release();
        return 1;
    }

    // The same colour the D3D11 path clears to, so one test covers both.
    const D3DCOLOR colour = D3DCOLOR_XRGB(64, 128, 191);

    std::printf("ready pid=%lu api=d3d9\n", GetCurrentProcessId());
    std::fflush(stdout);

    const DWORD deadline = GetTickCount() + static_cast<DWORD>(seconds) * 1000;
    while (g_running && GetTickCount() < deadline) {
        MSG message;
        while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        device->Clear(0, nullptr, D3DCLEAR_TARGET, colour, 1.0f, 0);
        device->Present(nullptr, nullptr, nullptr, nullptr);
    }

    device->Release();
    d3d9->Release();
    return 0;
}

}  // namespace

int wmain(int argc, wchar_t** argv) {
    const int seconds = argument(argc, argv, L"--seconds", 30);
    const int width = argument(argc, argv, L"--width", 640);
    const int height = argument(argc, argv, L"--height", 360);
    const bool use_d3d9 = flag(argc, argv, L"--d3d9");
    const bool fullscreen = flag(argc, argv, L"--fullscreen");
    const int start_delay = argument(argc, argv, L"--start-delay", 0);

    WNDCLASSEXW wc{};
    wc.cbSize = sizeof(wc);
    wc.lpfnWndProc = window_proc;
    wc.hInstance = GetModuleHandleW(nullptr);
    wc.lpszClassName = L"mello_fake_game";
    RegisterClassExW(&wc);

    RECT rect{0, 0, width, height};
    AdjustWindowRect(&rect, WS_OVERLAPPEDWINDOW, FALSE);
    HWND window = CreateWindowExW(0, wc.lpszClassName, L"m3llo fake game", WS_OVERLAPPEDWINDOW,
                                  CW_USEDEFAULT, CW_USEDEFAULT, rect.right - rect.left,
                                  rect.bottom - rect.top, nullptr, nullptr, wc.hInstance, nullptr);
    if (!window) {
        std::fprintf(stderr, "window failed: %lu\n", GetLastError());
        return 1;
    }
    ShowWindow(window, SW_SHOW);

    if (use_d3d9) {
        if (fullscreen) {
            SetWindowLongW(window, GWL_STYLE, WS_POPUP | WS_VISIBLE);
            SetWindowPos(window, HWND_TOP, 0, 0, width, height, SWP_FRAMECHANGED | SWP_SHOWWINDOW);
        }
        const int result = run_d3d9(window, width, height, seconds, fullscreen, start_delay);
        DestroyWindow(window);
        return result;
    }

    DXGI_SWAP_CHAIN_DESC desc{};
    desc.BufferCount = 2;
    desc.BufferDesc.Width = static_cast<UINT>(width);
    desc.BufferDesc.Height = static_cast<UINT>(height);
    desc.BufferDesc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    desc.OutputWindow = window;
    desc.SampleDesc.Count = 1;
    desc.Windowed = TRUE;
    desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;

    IDXGISwapChain* swap = nullptr;
    ID3D11Device* device = nullptr;
    ID3D11DeviceContext* context = nullptr;
    const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_0};
    HRESULT hr = D3D11CreateDeviceAndSwapChain(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0,
                                               levels, 1, D3D11_SDK_VERSION, &desc, &swap, &device,
                                               nullptr, &context);
    if (FAILED(hr)) {
        std::fprintf(stderr, "swap chain failed: hr=0x%08lx\n", static_cast<unsigned long>(hr));
        return 1;
    }

    ID3D11Texture2D* back = nullptr;
    ID3D11RenderTargetView* view = nullptr;
    swap->GetBuffer(0, __uuidof(ID3D11Texture2D), reinterpret_cast<void**>(&back));
    device->CreateRenderTargetView(back, nullptr, &view);

    std::printf("ready pid=%lu\n", GetCurrentProcessId());
    std::fflush(stdout);

    const DWORD deadline = GetTickCount() + static_cast<DWORD>(seconds) * 1000;
    while (g_running && GetTickCount() < deadline) {
        MSG message;
        while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        context->ClearRenderTargetView(view, kClearColour);
        // Vertical sync: presents at the display rate, like a game does.
        swap->Present(1, 0);
    }

    if (view) view->Release();
    if (back) back->Release();
    if (context) context->Release();
    if (device) device->Release();
    if (swap) swap->Release();
    DestroyWindow(window);
    return 0;
}
