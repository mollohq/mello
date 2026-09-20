// Capture a game through the hook, encode it, decode it, and look at the
// picture.
//
// Every other hook test stops at the captured texture. This one goes the whole
// way a viewer does: NVENC, H.264, the decoder, and the pixels that come out.
// It exists because a host can report 48 fps and 53 MB sent while the person
// watching sees black, and nothing short of decoding the stream tells the two
// apart (2026-09-18).
//
// It writes the decoded frame to `hook_loopback.bmp` in the working directory,
// so the picture can be looked at as well as measured.

#include <gtest/gtest.h>

#ifdef _WIN32

#include <windows.h>

#include <chrono>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include "video/video_pipeline.hpp"

using namespace mello::video;

namespace {

bool under_ci() {
    const char* ci = std::getenv("CI");
    return ci && *ci && std::string(ci) != "0" && std::string(ci) != "false";
}

std::string hook_directory() {
    const char* dir = std::getenv("MELLO_HOOK_DIR");
    return dir ? dir : "";
}

struct Packet {
    std::vector<uint8_t> data;
    bool                 keyframe;
};

// The test program that presents like a game. `--d3d9` picks the path a
// 2012-era game uses; the colour it draws is the same either way.
struct FakeGame {
    PROCESS_INFORMATION process{};

    bool start(const std::string& directory, bool d3d9) {
        std::string command = "\"" + directory + "\\mello-fakegame64.exe\" --seconds 30" +
                              (d3d9 ? " --d3d9" : "");
        std::wstring wide(command.begin(), command.end());
        wide.push_back(L'\0');
        STARTUPINFOW si{};
        si.cb = sizeof(si);
        if (!CreateProcessW(nullptr, wide.data(), nullptr, nullptr, FALSE, 0, nullptr, nullptr,
                            &si, &process)) {
            return false;
        }
        for (int i = 0; i < 100; ++i) {
            if (FindWindowW(L"mello_fake_game", nullptr)) return true;
            std::this_thread::sleep_for(std::chrono::milliseconds(50));
        }
        return false;
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

void write_bmp(const char* path, const std::vector<uint8_t>& rgba, uint32_t width,
               uint32_t height) {
    std::ofstream file(path, std::ios::binary);
    if (!file) return;

    const uint32_t row_padded = (width * 3 + 3) & ~3u;
    const uint32_t pixels = row_padded * height;
    const uint32_t size = 54 + pixels;

    uint8_t header[54]{};
    header[0] = 'B';
    header[1] = 'M';
    std::memcpy(header + 2, &size, 4);
    const uint32_t offset = 54;
    std::memcpy(header + 10, &offset, 4);
    const uint32_t dib = 40;
    std::memcpy(header + 14, &dib, 4);
    const int32_t w = static_cast<int32_t>(width);
    const int32_t h = static_cast<int32_t>(height);
    std::memcpy(header + 18, &w, 4);
    std::memcpy(header + 22, &h, 4);
    const uint16_t planes = 1;
    std::memcpy(header + 26, &planes, 2);
    const uint16_t bpp = 24;
    std::memcpy(header + 28, &bpp, 2);
    std::memcpy(header + 34, &pixels, 4);
    file.write(reinterpret_cast<char*>(header), 54);

    std::vector<uint8_t> row(row_padded, 0);
    for (int32_t y = static_cast<int32_t>(height) - 1; y >= 0; --y) {
        for (uint32_t x = 0; x < width; ++x) {
            const size_t src = (static_cast<size_t>(y) * width + x) * 4;
            row[x * 3 + 0] = rgba[src + 2];
            row[x * 3 + 1] = rgba[src + 1];
            row[x * 3 + 2] = rgba[src + 0];
        }
        file.write(reinterpret_cast<char*>(row.data()), row_padded);
    }
}

// Runs the whole chain for one graphics API and returns the decoded middle
// pixel. Empty result means no frame came out the far end.
struct Decoded {
    bool     ok = false;
    uint32_t width = 0;
    uint32_t height = 0;
    uint8_t  r = 0, g = 0, b = 0;
    size_t   packets = 0;
    size_t   keyframes = 0;
};

Decoded run_loopback(const std::string& directory, bool d3d9, const char* bmp_path) {
    Decoded out;

    FakeGame game;
    if (!game.start(directory, d3d9)) return out;

    VideoPipeline pipeline;
    if (!pipeline.init_device() || !pipeline.encoder_available()) return out;

    CaptureSourceDesc source{};
    source.mode = CaptureMode::Process;
    source.pid = game.pid();
    source.allow_hook = true;

    PipelineConfig config{};
    config.width = 640;
    config.height = 360;
    config.fps = 30;
    config.bitrate_kbps = 4000;
    config.low_latency = true;

    std::vector<Packet> packets;
    std::mutex mutex;
    if (!pipeline.start_host(source, config,
                             [&](const uint8_t* data, size_t size, bool keyframe, uint64_t) {
                                 std::lock_guard<std::mutex> lock(mutex);
                                 packets.push_back({std::vector<uint8_t>(data, data + size),
                                                    keyframe});
                             })) {
        return out;
    }
    std::this_thread::sleep_for(std::chrono::seconds(2));
    pipeline.stop_host();

    {
        std::lock_guard<std::mutex> lock(mutex);
        out.packets = packets.size();
        for (const Packet& packet : packets) {
            if (packet.keyframe) ++out.keyframes;
        }
    }
    if (out.packets == 0) return out;

    std::vector<uint8_t> frame;
    uint32_t frame_width = 0;
    uint32_t frame_height = 0;
    if (!pipeline.start_viewer(config, [&](const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t) {
            if (!frame.empty()) return;
            frame.assign(rgba, rgba + static_cast<size_t>(w) * h * 4);
            frame_width = w;
            frame_height = h;
        })) {
        return out;
    }

    bool seen_keyframe = false;
    for (const Packet& packet : packets) {
        if (!seen_keyframe) {
            if (!packet.keyframe) continue;
            seen_keyframe = true;
        }
        pipeline.feed_packet(packet.data.data(), packet.data.size(), packet.keyframe);
        if (!frame.empty()) break;
    }

    // The viewer does not push frames: the client pulls one per tick, and the
    // jitter buffer decides when. A test that only feeds packets sees nothing,
    // however well the decode went.
    for (int i = 0; i < 200 && frame.empty(); ++i) {
        pipeline.present_frame();
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    pipeline.stop_viewer();

    if (frame.empty()) return out;

    write_bmp(bmp_path, frame, frame_width, frame_height);

    const size_t middle = (static_cast<size_t>(frame_height / 2) * frame_width + frame_width / 2) * 4;
    out.ok = true;
    out.width = frame_width;
    out.height = frame_height;
    out.r = frame[middle + 0];
    out.g = frame[middle + 1];
    out.b = frame[middle + 2];
    return out;
}

} // namespace

// The whole path a viewer sees, for a Direct3D 11 game.
TEST(HookLoopback, ADirect3D11GameArrivesAtTheViewer) {
    if (under_ci()) GTEST_SKIP() << "needs a GPU and a desktop session";
    const std::string directory = hook_directory();
    if (directory.empty()) GTEST_SKIP() << "set MELLO_HOOK_DIR to the hook build folder";

    const Decoded decoded = run_loopback(directory, /*d3d9=*/false, "hook_loopback_d3d11.bmp");
    ASSERT_GT(decoded.packets, 0u) << "the host encoded nothing";
    ASSERT_GT(decoded.keyframes, 0u) << "the host sent no keyframe";
    ASSERT_TRUE(decoded.ok) << "nothing decoded: a viewer would see black";

    // The test program clears to (64, 128, 191). H.264 at 4 Mbps moves those a
    // little; a wrong path moves them a lot, and black moves them to zero.
    EXPECT_NEAR(decoded.r, 64, 12);
    EXPECT_NEAR(decoded.g, 128, 12);
    EXPECT_NEAR(decoded.b, 191, 12);
}

// The same, for the Direct3D 9 path, whose frames travel through memory.
TEST(HookLoopback, ADirect3D9GameArrivesAtTheViewer) {
    if (under_ci()) GTEST_SKIP() << "needs a GPU and a desktop session";
    const std::string directory = hook_directory();
    if (directory.empty()) GTEST_SKIP() << "set MELLO_HOOK_DIR to the hook build folder";

    const Decoded decoded = run_loopback(directory, /*d3d9=*/true, "hook_loopback_d3d9.bmp");
    ASSERT_GT(decoded.packets, 0u) << "the host encoded nothing";
    ASSERT_GT(decoded.keyframes, 0u) << "the host sent no keyframe";
    ASSERT_TRUE(decoded.ok) << "nothing decoded: a viewer would see black";

    EXPECT_NEAR(decoded.r, 64, 12);
    EXPECT_NEAR(decoded.g, 128, 12);
    EXPECT_NEAR(decoded.b, 191, 12);
}

#endif // _WIN32
