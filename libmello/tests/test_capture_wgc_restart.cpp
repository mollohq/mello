// A capture backend must survive being stopped and started again.
//
// The capture ladder does exactly that. When it tries a better method and that
// method turns out to be unavailable, it puts the previous backend back:
// `ProcessCapture::activate` stops the running backend, fails to build the new
// one, then calls start() on the old one again.
//
// WGC did not survive it. stop() closes the capture item and sets it to null,
// and start() then called CreateCaptureSession on a null item, which throws.
// The caller is a bare std::thread, so the throw reached std::terminate and
// ended the process: exit code 0xC0000409, no log line, and the voice call
// went with it. Seen with Counter-Strike 2 entering exclusive fullscreen on
// 2026-09-18, where the hook was refused (not on the safe list) and the ladder
// put WGC back.
//
// Needs a GPU and a desktop session, so it skips under CI like the other GPU
// tests in this suite.

#include <gtest/gtest.h>

#ifdef _WIN32

#include <windows.h>

#include <cstdlib>
#include <string>

#include <d3d11.h>

#include "video/capture_source.hpp"
#include "video/capture_wgc.hpp"
#include "video/graphics_device.hpp"

using namespace mello::video;

namespace {

bool running_under_ci() {
    const char* ci = std::getenv("CI");
    return ci && *ci && std::string(ci) != "0";
}

// A plain visible window, which is all WGC needs to make a capture item.
class TestWindow {
public:
    TestWindow() {
        WNDCLASSEXW wc{};
        wc.cbSize = sizeof(wc);
        wc.lpfnWndProc = DefWindowProcW;
        wc.hInstance = GetModuleHandleW(nullptr);
        wc.lpszClassName = L"MelloWgcRestartTestWindow";
        RegisterClassExW(&wc);
        hwnd_ = CreateWindowExW(0, wc.lpszClassName, L"Mello WGC restart test",
                                WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT, 640, 480,
                                nullptr, nullptr, wc.hInstance, nullptr);
        if (hwnd_) {
            ShowWindow(hwnd_, SW_SHOWNOACTIVATE);
            pump();
        }
    }
    ~TestWindow() {
        if (hwnd_) DestroyWindow(hwnd_);
    }
    void pump() {
        MSG msg;
        while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    HWND get() const { return hwnd_; }

private:
    HWND hwnd_ = nullptr;
};

}  // namespace

// The ladder's "put the old backend back" path, in one object. Before the fix
// the second start() threw and the process died, so this test cannot merely
// fail — it takes the test binary with it.
TEST(WgcCaptureRestart, StartsAgainAfterStop) {
    if (running_under_ci()) GTEST_SKIP() << "needs a GPU and a desktop session";

    TestWindow window;
    ASSERT_NE(window.get(), nullptr) << "could not create a test window";

    const GraphicsDevice device = create_d3d11_device();
    ASSERT_NE(device.handle, nullptr);

    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Window;
    desc.hwnd = window.get();

    WgcCapture capture;
    ASSERT_TRUE(capture.initialize(device, desc));

    auto no_frames = [](ID3D11Texture2D*, uint64_t) {};

    ASSERT_TRUE(capture.start(60, no_frames)) << "first start";
    capture.stop();

    // The ladder calls this after a better method refused to build.
    EXPECT_TRUE(capture.start(60, no_frames)) << "restart after stop";
    capture.stop();

    // And again, because the ladder retries the best method on a timer.
    EXPECT_TRUE(capture.start(60, no_frames)) << "second restart";
    capture.stop();
}

// A backend whose target is gone reports a failed start. It must not throw:
// the ladder treats a failed start as an ordinary outcome and moves on.
TEST(WgcCaptureRestart, RestartWithoutATargetFailsQuietly) {
    if (running_under_ci()) GTEST_SKIP() << "needs a GPU and a desktop session";

    const GraphicsDevice device = create_d3d11_device();
    ASSERT_NE(device.handle, nullptr);

    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Window;

    WgcCapture capture;
    {
        TestWindow window;
        ASSERT_NE(window.get(), nullptr);
        desc.hwnd = window.get();
        ASSERT_TRUE(capture.initialize(device, desc));
        ASSERT_TRUE(capture.start(60, [](ID3D11Texture2D*, uint64_t) {}));
        capture.stop();
    }
    // The window is destroyed now, which is what a quit game looks like.
    EXPECT_NO_THROW({
        EXPECT_FALSE(capture.start(60, [](ID3D11Texture2D*, uint64_t) {}));
    });
    capture.stop();
}

#endif  // _WIN32
