// End-to-end test for the game capture hook.
//
// It starts a small D3D11 program that presents, hooks it, and checks that the
// pixels that come back are the pixels that program drew. That is the whole
// chain: offsets helper, injection helper, the detour on the game's present
// path, the shared texture, and the client end in `HookCapture`.
//
// It needs a GPU and a desktop session, so it skips under CI like the other
// GPU tests in this suite.

#include <gtest/gtest.h>

#ifdef _WIN32

#include <windows.h>

#include <d3d11.h>
#include <wrl/client.h>

#include <atomic>
#include <chrono>
#include <cstdlib>
#include <mutex>
#include <string>
#include <thread>

#include "video/capture_hook.hpp"
#include "video/graphics_device.hpp"
#include "video/hook_policy.hpp"

using namespace mello::video;

namespace {

bool running_under_ci() {
    const char* ci = std::getenv("CI");
    return ci && *ci && std::string(ci) != "0" && std::string(ci) != "false";
}

std::string hook_directory() {
    const char* dir = std::getenv("MELLO_HOOK_DIR");
    return dir ? dir : "";
}

// Starts the test program and returns its process id and handle. The program
// prints "ready pid=..." once its swap chain is up; waiting for the window is
// the same thing and needs no pipe.
struct FakeGame {
    PROCESS_INFORMATION process{};
    HWND                window = nullptr;

    bool start(const std::string& directory, int seconds) {
        std::string command = "\"" + directory + "\\mello-fakegame64.exe\" --seconds " +
                              std::to_string(seconds);
        std::wstring wide(command.begin(), command.end());
        wide.push_back(L'\0');

        STARTUPINFOW si{};
        si.cb = sizeof(si);
        if (!CreateProcessW(nullptr, wide.data(), nullptr, nullptr, FALSE, 0, nullptr, nullptr,
                            &si, &process)) {
            return false;
        }
        // The window appears within a few hundred milliseconds. Without it the
        // hook has no thread to attach to.
        for (int i = 0; i < 100 && !window; ++i) {
            window = FindWindowW(L"mello_fake_game", nullptr);
            if (window) break;
            std::this_thread::sleep_for(std::chrono::milliseconds(50));
        }
        return window != nullptr;
    }

    uint32_t pid() const { return process.dwProcessId; }

    ~FakeGame() {
        if (process.hProcess) {
            TerminateProcess(process.hProcess, 0);
            WaitForSingleObject(process.hProcess, 2000);
            CloseHandle(process.hProcess);
            CloseHandle(process.hThread);
        }
    }
};

// Reads the middle pixel of a captured frame, through a staging copy.
struct Pixel {
    uint8_t b = 0, g = 0, r = 0, a = 0;
};

bool middle_pixel(ID3D11Device* device, ID3D11DeviceContext* context, ID3D11Texture2D* texture,
                  Pixel* out) {
    D3D11_TEXTURE2D_DESC desc{};
    texture->GetDesc(&desc);
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    desc.MiscFlags = 0;

    Microsoft::WRL::ComPtr<ID3D11Texture2D> staging;
    if (FAILED(device->CreateTexture2D(&desc, nullptr, &staging))) return false;
    context->CopyResource(staging.Get(), texture);

    D3D11_MAPPED_SUBRESOURCE mapped{};
    if (FAILED(context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped))) return false;
    const auto* rows = static_cast<const uint8_t*>(mapped.pData);
    const uint8_t* pixel = rows + (desc.Height / 2) * mapped.RowPitch + (desc.Width / 2) * 4;
    out->b = pixel[0];
    out->g = pixel[1];
    out->r = pixel[2];
    out->a = pixel[3];
    context->Unmap(staging.Get(), 0);
    return true;
}

} // namespace

// The whole hook path, against a program that presents like a game.
TEST(HookCapture, CapturesTheFramesTheGameDrew) {
    if (running_under_ci()) GTEST_SKIP() << "needs a GPU and a desktop session";
    const std::string directory = hook_directory();
    if (directory.empty()) GTEST_SKIP() << "set MELLO_HOOK_DIR to the hook build folder";

    FakeGame game;
    ASSERT_TRUE(game.start(directory, 30)) << "the test program did not start";

    const GraphicsDevice device = create_d3d11_device();
    ASSERT_NE(device.d3d11(), nullptr) << "no D3D11 device on this machine";
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
    device.d3d11()->GetImmediateContext(&context);

    CaptureSourceDesc desc{};
    desc.mode = CaptureMode::Process;
    desc.pid = game.pid();
    desc.allow_hook = true;

    HookCapture capture;
    ASSERT_TRUE(capture.initialize(device, desc)) << "the hook did not load into the test program";
    EXPECT_EQ(capture.width(), 640u);
    EXPECT_EQ(capture.height(), 360u);

    std::atomic<int> frames{0};
    Microsoft::WRL::ComPtr<ID3D11Texture2D> last;
    std::mutex last_mutex;
    ASSERT_TRUE(capture.start(60, [&](ID3D11Texture2D* texture, uint64_t) {
        std::lock_guard<std::mutex> lock(last_mutex);
        last = texture;
        frames.fetch_add(1);
    }));

    // 60 fps for two seconds is 120 frames. Ten is enough to prove delivery and
    // leaves room for a slow first frame.
    for (int i = 0; i < 40 && frames.load() < 10; ++i) {
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
    }
    EXPECT_GE(frames.load(), 10) << "the hook delivered no frames";

    Microsoft::WRL::ComPtr<ID3D11Texture2D> frame;
    {
        std::lock_guard<std::mutex> lock(last_mutex);
        frame = last;
    }
    ASSERT_TRUE(frame) << "no frame texture";

    Pixel pixel;
    ASSERT_TRUE(middle_pixel(device.d3d11(), context.Get(), frame.Get(), &pixel));
    // The test program clears to (0.25, 0.50, 0.75). Anything else means the
    // hook captured the wrong surface, or the channels are swapped.
    EXPECT_NEAR(pixel.r, 64, 2);
    EXPECT_NEAR(pixel.g, 128, 2);
    EXPECT_NEAR(pixel.b, 191, 2);

    capture.stop();
    EXPECT_FALSE(capture.stop_timed_out());
    EXPECT_FALSE(capture.failed());
}

// The hook must never go into a process the caller did not allow, whatever the
// process is.
TEST(HookCapture, RefusesAProcessTheCallerDidNotAllow) {
    if (running_under_ci()) GTEST_SKIP() << "needs a GPU and a desktop session";
    const std::string directory = hook_directory();
    if (directory.empty()) GTEST_SKIP() << "set MELLO_HOOK_DIR to the hook build folder";

    const hook::PolicyResult result = hook::check_process(GetCurrentProcessId(), false);
    EXPECT_FALSE(result.allowed());
    EXPECT_EQ(result.verdict, hook::PolicyVerdict::NotAllowedByCaller);
}

#endif // _WIN32
