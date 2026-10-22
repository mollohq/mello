// mello-fakegame — a D3D11 program that presents, for testing the hook.
//
// It is the smallest thing that looks like a game to the hook: a window, a DXGI
// swap chain, and a present every frame. It clears the back buffer to one known
// colour, so a test can check that the pixels the hook delivered are the pixels
// this program drew, and not a black frame or somebody else's window.
//
// Usage: mello-fakegame64.exe [--seconds N] [--width W] [--height H] [--d3d9]
//                             [--fullscreen] [--start-delay N]
//                             [--stop-drawing-after N --stop-drawing-for M]
//
// `--stop-drawing-after` stops presenting for M seconds, which is what a
// fullscreen game does the moment somebody alt-tabs away from it.
//
// `--start-delay` shows the window but builds no device until N seconds have
// passed, the way a game looks while it loads. A stream that starts in that
// window finds nothing to capture anywhere, which is what a real game did on
// 2026-09-16.
//
// `--d3d9` presents through Direct3D 9 instead, which is the other path the
// hook covers and the one a 2012-era game uses.
//
// `--d3d12` presents through a Direct3D 12 swap chain on its command queue,
// the API of most new games.
//
// `--fps N` presents N times a second with no vertical sync, paced by the
// clock, so a test can show a fast game (144 fps) on any display.
//
// It prints `ready pid=<pid>` when the swap chain is up, so a test can wait for
// that line instead of sleeping.

#include <windows.h>
#include <timeapi.h>

#include <d3d9.h>
#include <d3d11.h>
#include <d3d12.h>
#include <dxgi1_4.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>

#pragma comment(lib, "d3d11.lib")
#pragma comment(lib, "d3d9.lib")
#pragma comment(lib, "winmm.lib")
#pragma comment(lib, "d3d12.lib")
#pragma comment(lib, "dxgi.lib")

namespace {

// Presents at a fixed rate when `--fps` is given: sleeps in 1 ms steps, then
// waits the last part, so the rate holds within a few frames a second.
class Pacer {
public:
    explicit Pacer(int fps) : fps_(fps) {
        if (fps_ <= 0) return;
        timeBeginPeriod(1);
        QueryPerformanceFrequency(&freq_);
        QueryPerformanceCounter(&next_);
    }
    ~Pacer() {
        if (fps_ > 0) timeEndPeriod(1);
    }
    bool active() const { return fps_ > 0; }
    void wait() {
        if (fps_ <= 0) return;
        next_.QuadPart += freq_.QuadPart / fps_;
        LARGE_INTEGER now{};
        QueryPerformanceCounter(&now);
        if (now.QuadPart > next_.QuadPart + freq_.QuadPart / 10) {
            next_ = now;  // far behind (the window was dragged): start again
            return;
        }
        for (;;) {
            QueryPerformanceCounter(&now);
            const int64_t left = next_.QuadPart - now.QuadPart;
            if (left <= 0) return;
            if (left * 1000 > freq_.QuadPart * 2) {
                Sleep(1);
            } else {
                YieldProcessor();
            }
        }
    }

private:
    int           fps_ = 0;
    LARGE_INTEGER freq_{};
    LARGE_INTEGER next_{};
};

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
// True while the program is deliberately not drawing, so a test can watch what
// a stream does when the game stops.
bool drawing_paused(DWORD started_at, int after_seconds, int for_seconds) {
    if (after_seconds <= 0 || for_seconds <= 0) return false;
    const DWORD elapsed = GetTickCount() - started_at;
    const DWORD from = static_cast<DWORD>(after_seconds) * 1000;
    return elapsed >= from && elapsed < from + static_cast<DWORD>(for_seconds) * 1000;
}

// A second device on its own window, presenting slowly: a launcher, an overlay
// or a login window next to the one that draws the game. Heaven has one.
struct SecondDevice {
    HWND                window = nullptr;
    IDirect3D9Ex*       d3d9   = nullptr;
    IDirect3DDevice9Ex* device = nullptr;

    bool create() {
        WNDCLASSEXW wc{};
        wc.cbSize = sizeof(wc);
        wc.lpfnWndProc = DefWindowProcW;
        wc.hInstance = GetModuleHandleW(nullptr);
        wc.lpszClassName = L"mello_fake_game_second";
        RegisterClassExW(&wc);
        window = CreateWindowExW(0, wc.lpszClassName, L"m3llo fake game overlay",
                                 WS_OVERLAPPEDWINDOW, 0, 0, 320, 200, nullptr, nullptr,
                                 wc.hInstance, nullptr);
        if (!window) return false;

        if (FAILED(Direct3DCreate9Ex(D3D_SDK_VERSION, &d3d9)) || !d3d9) return false;
        D3DPRESENT_PARAMETERS pp{};
        pp.Windowed = TRUE;
        pp.SwapEffect = D3DSWAPEFFECT_DISCARD;
        pp.BackBufferFormat = D3DFMT_X8R8G8B8;
        pp.BackBufferWidth = 320;
        pp.BackBufferHeight = 200;
        pp.hDeviceWindow = window;
        pp.PresentationInterval = D3DPRESENT_INTERVAL_IMMEDIATE;
        return SUCCEEDED(d3d9->CreateDeviceEx(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, window,
                                              D3DCREATE_SOFTWARE_VERTEXPROCESSING |
                                                  D3DCREATE_NOWINDOWCHANGES,
                                              &pp, nullptr, &device)) &&
               device != nullptr;
    }

    void present() {
        if (!device) return;
        device->Clear(0, nullptr, D3DCLEAR_TARGET, D3DCOLOR_XRGB(255, 0, 0), 1.0f, 0);
        device->Present(nullptr, nullptr, nullptr, nullptr);
    }

    ~SecondDevice() {
        if (device) device->Release();
        if (d3d9) d3d9->Release();
        if (window) DestroyWindow(window);
    }
};

int run_d3d9(HWND window, int width, int height, int seconds, bool fullscreen,
             int start_delay_seconds, int stop_after, int stop_for, bool second_device,
             int fps) {
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
    pp.PresentationInterval = fps > 0 ? D3DPRESENT_INTERVAL_IMMEDIATE : D3DPRESENT_INTERVAL_ONE;

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

    SecondDevice overlay;
    if (second_device && !overlay.create()) {
        std::fprintf(stderr, "second device failed\n");
    }

    uint64_t frames = 0;
    Pacer pacer(fps);
    DWORD last_overlay_present = 0;
    const DWORD started_at = GetTickCount();
    const DWORD deadline = started_at + static_cast<DWORD>(seconds) * 1000;
    bool was_paused = false;
    while (g_running && GetTickCount() < deadline) {
        MSG message;
        while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        const bool paused = drawing_paused(started_at, stop_after, stop_for);
        if (paused != was_paused) {
            was_paused = paused;
            std::printf("%s drawing\n", paused ? "stopped" : "resumed");
            std::fflush(stdout);
        }
        if (paused) {
            Sleep(16);
            continue;
        }
        device->Clear(0, nullptr, D3DCLEAR_TARGET, colour, 1.0f, 0);
        // Alternate the two present calls a D3D9Ex game has. They reach the
        // hook as two different interface pointers to one device, which is what
        // Unigine Heaven does and what made the hook rebuild its capture on
        // almost every frame until 2026-09-18.
        if ((frames++ & 1) == 0) {
            device->Present(nullptr, nullptr, nullptr, nullptr);
        } else {
            device->PresentEx(nullptr, nullptr, nullptr, nullptr, 0);
        }
        pacer.wait();
        // The small device presents about nine times a second, which is what
        // Heaven's second window did.
        if (second_device && GetTickCount() - last_overlay_present >= 110) {
            last_overlay_present = GetTickCount();
            overlay.present();
        }
    }

    device->Release();
    d3d9->Release();
    return 0;
}

// The Direct3D 12 build: a command queue, a flip-model swap chain made on it,
// a clear to the same colour, and a present every frame. It waits for the GPU
// after every frame, which a game does not, and which a test does not mind.
int run_d3d12(HWND window, int width, int height, int seconds, int fps) {
    ID3D12Device* device = nullptr;
    if (FAILED(D3D12CreateDevice(nullptr, D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&device)))) {
        std::fprintf(stderr, "no D3D12 device\n");
        return 1;
    }
    D3D12_COMMAND_QUEUE_DESC qd{};
    qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    ID3D12CommandQueue* queue = nullptr;
    IDXGIFactory2* factory = nullptr;
    IDXGISwapChain1* swap1 = nullptr;
    IDXGISwapChain3* swap = nullptr;
    if (FAILED(device->CreateCommandQueue(&qd, IID_PPV_ARGS(&queue))) ||
        FAILED(CreateDXGIFactory2(0, IID_PPV_ARGS(&factory)))) {
        std::fprintf(stderr, "no D3D12 queue or DXGI factory\n");
        return 1;
    }
    DXGI_SWAP_CHAIN_DESC1 sd{};
    sd.Width = static_cast<UINT>(width);
    sd.Height = static_cast<UINT>(height);
    sd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    sd.SampleDesc.Count = 1;
    sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.BufferCount = 2;
    sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    if (FAILED(factory->CreateSwapChainForHwnd(queue, window, &sd, nullptr, nullptr, &swap1)) ||
        FAILED(swap1->QueryInterface(IID_PPV_ARGS(&swap)))) {
        std::fprintf(stderr, "no D3D12 swap chain\n");
        return 1;
    }

    D3D12_DESCRIPTOR_HEAP_DESC hd{};
    hd.Type = D3D12_DESCRIPTOR_HEAP_TYPE_RTV;
    hd.NumDescriptors = 2;
    ID3D12DescriptorHeap* heap = nullptr;
    device->CreateDescriptorHeap(&hd, IID_PPV_ARGS(&heap));
    const UINT rtv_size = device->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
    ID3D12Resource* buffers[2] = {};
    for (UINT i = 0; i < 2; ++i) {
        swap->GetBuffer(i, IID_PPV_ARGS(&buffers[i]));
        D3D12_CPU_DESCRIPTOR_HANDLE h = heap->GetCPUDescriptorHandleForHeapStart();
        h.ptr += static_cast<SIZE_T>(i) * rtv_size;
        device->CreateRenderTargetView(buffers[i], nullptr, h);
    }
    ID3D12CommandAllocator* allocator = nullptr;
    ID3D12GraphicsCommandList* list = nullptr;
    ID3D12Fence* fence = nullptr;
    device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&allocator));
    device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, allocator, nullptr,
                              IID_PPV_ARGS(&list));
    list->Close();
    device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence));
    HANDLE idle = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    uint64_t fence_value = 0;

    std::printf("ready pid=%lu api=d3d12\n", GetCurrentProcessId());
    std::fflush(stdout);

    Pacer pacer(fps);
    const DWORD deadline = GetTickCount() + static_cast<DWORD>(seconds) * 1000;
    while (g_running && GetTickCount() < deadline) {
        MSG message;
        while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        const UINT index = swap->GetCurrentBackBufferIndex();
        allocator->Reset();
        list->Reset(allocator, nullptr);
        D3D12_RESOURCE_BARRIER barrier{};
        barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
        barrier.Transition.pResource = buffers[index];
        barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
        barrier.Transition.StateBefore = D3D12_RESOURCE_STATE_PRESENT;
        barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_RENDER_TARGET;
        list->ResourceBarrier(1, &barrier);
        D3D12_CPU_DESCRIPTOR_HANDLE h = heap->GetCPUDescriptorHandleForHeapStart();
        h.ptr += static_cast<SIZE_T>(index) * rtv_size;
        list->ClearRenderTargetView(h, kClearColour, 0, nullptr);
        barrier.Transition.StateBefore = D3D12_RESOURCE_STATE_RENDER_TARGET;
        barrier.Transition.StateAfter = D3D12_RESOURCE_STATE_PRESENT;
        list->ResourceBarrier(1, &barrier);
        list->Close();
        ID3D12CommandList* lists[] = {list};
        queue->ExecuteCommandLists(1, lists);
        swap->Present(pacer.active() ? 0 : 1, 0);
        queue->Signal(fence, ++fence_value);
        if (fence->GetCompletedValue() < fence_value) {
            fence->SetEventOnCompletion(fence_value, idle);
            WaitForSingleObject(idle, 1000);
        }
        pacer.wait();
    }

    CloseHandle(idle);
    fence->Release();
    list->Release();
    allocator->Release();
    for (ID3D12Resource* b : buffers) {
        if (b) b->Release();
    }
    heap->Release();
    swap->Release();
    swap1->Release();
    factory->Release();
    queue->Release();
    device->Release();
    return 0;
}

}  // namespace

int wmain(int argc, wchar_t** argv) {
    const int seconds = argument(argc, argv, L"--seconds", 30);
    const int width = argument(argc, argv, L"--width", 640);
    const int height = argument(argc, argv, L"--height", 360);
    const bool use_d3d9 = flag(argc, argv, L"--d3d9");
    const bool use_d3d12 = flag(argc, argv, L"--d3d12");
    const bool fullscreen = flag(argc, argv, L"--fullscreen");
    const int start_delay = argument(argc, argv, L"--start-delay", 0);
    const int stop_after = argument(argc, argv, L"--stop-drawing-after", 0);
    const int stop_for = argument(argc, argv, L"--stop-drawing-for", 0);
    const bool second_device = flag(argc, argv, L"--second-device");
    const int fps = argument(argc, argv, L"--fps", 0);

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
        const int result = run_d3d9(window, width, height, seconds, fullscreen, start_delay,
                                    stop_after, stop_for, second_device, fps);
        DestroyWindow(window);
        return result;
    }

    if (use_d3d12) {
        const int result = run_d3d12(window, width, height, seconds, fps);
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

    Pacer pacer(fps);
    const DWORD deadline = GetTickCount() + static_cast<DWORD>(seconds) * 1000;
    while (g_running && GetTickCount() < deadline) {
        MSG message;
        while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        context->ClearRenderTargetView(view, kClearColour);
        // Vertical sync: presents at the display rate, like a game does. With
        // --fps, no sync and the clock sets the rate.
        swap->Present(pacer.active() ? 0 : 1, 0);
        pacer.wait();
    }

    if (view) view->Release();
    if (back) back->Release();
    if (context) context->Release();
    if (device) device->Release();
    if (swap) swap->Release();
    DestroyWindow(window);
    return 0;
}
