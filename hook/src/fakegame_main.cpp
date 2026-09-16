// mello-fakegame — a D3D11 program that presents, for testing the hook.
//
// It is the smallest thing that looks like a game to the hook: a window, a DXGI
// swap chain, and a present every frame. It clears the back buffer to one known
// colour, so a test can check that the pixels the hook delivered are the pixels
// this program drew, and not a black frame or somebody else's window.
//
// Usage: mello-fakegame64.exe [--seconds N] [--width W] [--height H]
//
// It prints `ready pid=<pid>` when the swap chain is up, so a test can wait for
// that line instead of sleeping.

#include <windows.h>

#include <d3d11.h>
#include <dxgi1_2.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>

#pragma comment(lib, "d3d11.lib")

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

}  // namespace

int wmain(int argc, wchar_t** argv) {
    const int seconds = argument(argc, argv, L"--seconds", 30);
    const int width = argument(argc, argv, L"--width", 640);
    const int height = argument(argc, argv, L"--height", 360);

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
