// A windowed game came with its title bar and borders: WGC captures the
// window's visible frame, and every game clip and every stream on the WGC
// window rung showed the bar with its minimize and close buttons (a
// Hearthstone clip, 2026-11-02). The capture now cuts the client area out.

#include <gtest/gtest.h>

#include "video/window_crop.hpp"

using mello::video::client_crop;

// Windows 11: a 1-pixel visible border and a 31-pixel title bar around a
// 1280x720 client area.
TEST(WindowCrop, AWindowedGameLosesItsTitleBarAndBorders) {
    const auto crop = client_crop(100, 50, 101, 81, 1280, 720, 1282, 752);
    ASSERT_TRUE(crop.has_value());
    EXPECT_EQ(crop->x, 1u);
    EXPECT_EQ(crop->y, 31u);
    EXPECT_EQ(crop->width, 1280u);
    EXPECT_EQ(crop->height, 720u);
}

TEST(WindowCrop, ABorderlessGameStaysWhole) {
    EXPECT_FALSE(client_crop(0, 0, 0, 0, 1920, 1080, 1920, 1080).has_value());
}

TEST(WindowCrop, AClientAreaOutsideThePictureStaysWhole) {
    // A minimized window has no client area.
    EXPECT_FALSE(client_crop(100, 50, 101, 81, 0, 0, 1282, 752).has_value());
    // The window grew between the two reads: the client area is larger.
    EXPECT_FALSE(client_crop(100, 50, 101, 81, 1600, 900, 1282, 752).has_value());
    // A client origin left of or above the frame.
    EXPECT_FALSE(client_crop(100, 50, 90, 81, 1280, 720, 1282, 752).has_value());
    EXPECT_FALSE(client_crop(100, 50, 101, 40, 1280, 720, 1282, 752).has_value());
}

TEST(WindowCrop, NegativeScreenCoordinatesWork) {
    // A window on a monitor left of the primary one.
    const auto crop = client_crop(-1920, -10, -1919, 21, 800, 600, 802, 632);
    ASSERT_TRUE(crop.has_value());
    EXPECT_EQ(crop->x, 1u);
    EXPECT_EQ(crop->y, 31u);
}

#ifdef _WIN32

#include <windows.h>

#include <chrono>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>

#include <d3d11.h>
#include <wrl/client.h>

#include "video/capture_source.hpp"
#include "video/capture_wgc.hpp"
#include "video/graphics_device.hpp"

using namespace mello::video;
using Microsoft::WRL::ComPtr;

namespace {

bool running_under_ci() {
    const char* ci = std::getenv("CI");
    return ci && *ci && std::string(ci) != "0";
}

// A window painted magenta, with the given style.
class PaintedWindow {
public:
    explicit PaintedWindow(DWORD style) {
        brush_ = CreateSolidBrush(RGB(255, 0, 255));
        WNDCLASSEXW wc{};
        wc.cbSize = sizeof(wc);
        wc.lpfnWndProc = DefWindowProcW;
        wc.hInstance = GetModuleHandleW(nullptr);
        wc.lpszClassName = L"MelloWindowCropTestWindow";
        wc.hbrBackground = brush_;
        RegisterClassExW(&wc);
        hwnd_ = CreateWindowExW(0, wc.lpszClassName, L"Mello window crop test", style, 120, 120,
                                640, 480, nullptr, nullptr, wc.hInstance, nullptr);
        if (hwnd_) {
            ShowWindow(hwnd_, SW_SHOWNOACTIVATE);
            pump();
        }
    }
    ~PaintedWindow() {
        if (hwnd_) DestroyWindow(hwnd_);
        UnregisterClassW(L"MelloWindowCropTestWindow", GetModuleHandleW(nullptr));
        DeleteObject(brush_);
    }
    void pump() {
        MSG msg;
        while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    HWND get() const { return hwnd_; }

    /// The client size in physical pixels.
    SIZE client_size() const {
        const DPI_AWARENESS_CONTEXT previous =
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        RECT rc{};
        GetClientRect(hwnd_, &rc);
        if (previous) SetThreadDpiAwarenessContext(previous);
        return {rc.right - rc.left, rc.bottom - rc.top};
    }

private:
    HWND hwnd_ = nullptr;
    HBRUSH brush_ = nullptr;
};

struct Delivered {
    uint32_t width = 0;
    uint32_t height = 0;
    uint32_t top_left = 0;  ///< BGRA of the pixel at (2, 2)
    int frames = 0;
};

// Runs a WGC capture of `hwnd` for a second and reads back the last frame.
Delivered capture_one_second(PaintedWindow& window, uint32_t* reported_w, uint32_t* reported_h) {
    Delivered out;
    const GraphicsDevice device = create_d3d11_device();
    if (!device.handle) return out;

    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Window;
    desc.hwnd = window.get();
    WgcCapture capture;
    if (!capture.initialize(device, desc)) return out;

    ComPtr<ID3D11DeviceContext> context;
    device.d3d11()->GetImmediateContext(&context);
    ComPtr<ID3D11Texture2D> staging;
    std::mutex mutex;
    capture.start(60, [&](ID3D11Texture2D* texture, uint64_t) {
        D3D11_TEXTURE2D_DESC d{};
        texture->GetDesc(&d);
        std::lock_guard<std::mutex> lock(mutex);
        if (!staging || out.width != d.Width || out.height != d.Height) {
            D3D11_TEXTURE2D_DESC s = d;
            s.Usage = D3D11_USAGE_STAGING;
            s.BindFlags = 0;
            s.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
            s.MiscFlags = 0;
            staging.Reset();
            device.d3d11()->CreateTexture2D(&s, nullptr, &staging);
        }
        if (staging) context->CopyResource(staging.Get(), texture);
        out.width = d.Width;
        out.height = d.Height;
        ++out.frames;
    });
    const auto until = std::chrono::steady_clock::now() + std::chrono::milliseconds(1000);
    while (std::chrono::steady_clock::now() < until) {
        InvalidateRect(window.get(), nullptr, TRUE);
        window.pump();
        std::this_thread::sleep_for(std::chrono::milliseconds(16));
    }
    capture.stop();
    *reported_w = capture.width();
    *reported_h = capture.height();

    std::lock_guard<std::mutex> lock(mutex);
    D3D11_MAPPED_SUBRESOURCE mapped{};
    if (staging && SUCCEEDED(context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped))) {
        const auto* row = static_cast<const uint8_t*>(mapped.pData) + 2 * mapped.RowPitch;
        std::memcpy(&out.top_left, row + 2 * 4, 4);
        context->Unmap(staging.Get(), 0);
    }
    return out;
}

constexpr uint32_t kMagentaBgra = 0xFFFF00FF;  // B=FF G=00 R=FF A=FF

}  // namespace

// The frames of a window with a title bar are its client area: the size of
// the client rectangle, and the top-left pixel is the client's paint, not the
// title bar.
TEST(WindowCrop, WgcDeliversTheClientAreaOfAWindowWithATitleBar) {
    if (running_under_ci()) GTEST_SKIP() << "needs a GPU and a desktop session";
    PaintedWindow window(WS_OVERLAPPEDWINDOW);
    ASSERT_NE(window.get(), nullptr);
    const SIZE client = window.client_size();

    uint32_t reported_w = 0, reported_h = 0;
    const Delivered got = capture_one_second(window, &reported_w, &reported_h);
    if (got.frames == 0) GTEST_SKIP() << "WGC delivered no frame: the desktop is locked or asleep";

    EXPECT_EQ(got.width, static_cast<uint32_t>(client.cx));
    EXPECT_EQ(got.height, static_cast<uint32_t>(client.cy));
    EXPECT_EQ(reported_w, got.width) << "width() names the delivered size";
    EXPECT_EQ(reported_h, got.height);
    EXPECT_EQ(got.top_left, kMagentaBgra) << "the top-left pixel is not the client area";
}

// A window with no frame is its client area: the frames stay whole.
TEST(WindowCrop, WgcKeepsAWindowWithNoFrameWhole) {
    if (running_under_ci()) GTEST_SKIP() << "needs a GPU and a desktop session";
    PaintedWindow window(WS_POPUP);
    ASSERT_NE(window.get(), nullptr);
    const SIZE client = window.client_size();

    uint32_t reported_w = 0, reported_h = 0;
    const Delivered got = capture_one_second(window, &reported_w, &reported_h);
    if (got.frames == 0) GTEST_SKIP() << "WGC delivered no frame: the desktop is locked or asleep";

    EXPECT_EQ(got.width, static_cast<uint32_t>(client.cx));
    EXPECT_EQ(got.height, static_cast<uint32_t>(client.cy));
    EXPECT_EQ(got.top_left, kMagentaBgra);
}

#endif  // _WIN32
