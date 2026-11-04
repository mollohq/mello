// The recording intent on VideoToolbox (14-VIDEO-PIPELINE.md 6.8): a 1 s GOP
// in frames and in time, no reordering (the ring segments and the MP4
// passthrough need every frame in decode order), and a hardware session.
#include <gtest/gtest.h>

#ifdef __APPLE__
#include "replay/h264_nal.hpp"
#include "video/encoder_videotoolbox.hpp"
#include "video/graphics_device.hpp"
#include "video/video_pipeline.hpp"

#include <CoreFoundation/CoreFoundation.h>
#include <CoreVideo/CoreVideo.h>

#include <atomic>
#include <cstdio>
#include <cstring>
#include <thread>
#include <vector>

using namespace mello::video;

namespace {

/// A moving BGRA pattern, so each frame differs from the last.
CVPixelBufferRef make_frame(uint32_t w, uint32_t h, uint32_t n) {
    CVPixelBufferRef pb = nullptr;
    if (CVPixelBufferCreate(kCFAllocatorDefault, w, h, kCVPixelFormatType_32BGRA, nullptr, &pb) != kCVReturnSuccess) {
        return nullptr;
    }
    CVPixelBufferLockBaseAddress(pb, 0);
    auto* base = static_cast<uint8_t*>(CVPixelBufferGetBaseAddress(pb));
    const size_t stride = CVPixelBufferGetBytesPerRow(pb);
    for (uint32_t y = 0; y < h; ++y) {
        for (uint32_t x = 0; x < w; ++x) {
            uint8_t* p = base + y * stride + x * 4;
            p[0] = static_cast<uint8_t>((x + n * 4) & 0xFF);
            p[1] = static_cast<uint8_t>((y * 2 + n) & 0xFF);
            p[2] = static_cast<uint8_t>((x + y + n * 2) & 0xFF);
            p[3] = 255;
        }
    }
    CVPixelBufferUnlockBaseAddress(pb, 0);
    return pb;
}

/// The slice type of the first slice of an access unit: 0 P, 1 B, 2 I (the
/// first bits of the slice header, ue(v) first_mb then ue(v) slice_type).
int slice_type(const std::vector<uint8_t>& au) {
    int type = -1;
    mello::replay::h264::for_each_nal(au.data(), au.size(), [&](uint8_t nal_type, const uint8_t* nal, size_t size) {
        if (type >= 0 || (nal_type != 1 && nal_type != 5) || size < 2) return;
        // Exp-Golomb reads over the bytes after the NAL header.
        size_t bit = 8;
        auto read_bit = [&]() -> int {
            if (bit / 8 >= size) return 0;
            const int b = (nal[bit / 8] >> (7 - bit % 8)) & 1;
            ++bit;
            return b;
        };
        auto ue = [&]() {
            int zeros = 0;
            while (read_bit() == 0 && zeros < 31) ++zeros;
            unsigned v = 1;
            for (int i = 0; i < zeros; ++i) v = (v << 1) | static_cast<unsigned>(read_bit());
            return static_cast<int>(v - 1);
        };
        ue();  // first_mb_in_slice
        type = ue() % 5;
    });
    return type;
}

}  // namespace

TEST(EncoderIntent, VideoToolboxRecordingHasAOneSecondGopAndNoBFrames) {
    EncoderConfig config{};
    config.width = 640;
    config.height = 360;
    config.fps = 60;
    config.bitrate_kbps = 3000;
    config.keyframe_interval = 60;
    config.intent = EncoderIntent::Recording;
    VTEncoder encoder;
    if (!encoder.initialize(GraphicsDevice{}, config)) GTEST_SKIP() << "no hardware H.264 encoder";

    std::vector<int> keyframes;
    size_t b_frames = 0, frames = 0;
    for (uint32_t n = 0; n < 150; ++n) {
        CVPixelBufferRef pb = make_frame(config.width, config.height, n);
        ASSERT_NE(pb, nullptr);
        EncodedPacket packet{};
        ASSERT_TRUE(encoder.encode(pb, 1'000'000ull + n * 16'667ull, packet)) << "frame " << n;
        CVPixelBufferRelease(pb);
        if (packet.data.empty()) continue;
        ++frames;
        if (packet.is_keyframe) keyframes.push_back(static_cast<int>(n));
        if (slice_type(packet.data) == 1) ++b_frames;
    }
    encoder.shutdown();
    EXPECT_EQ(frames, 150u) << "one access unit per frame, in order";
    EXPECT_EQ(b_frames, 0u) << "no B-frames";
    ASSERT_GE(keyframes.size(), 3u);
    EXPECT_EQ(keyframes[0], 0);
    EXPECT_EQ(keyframes[1], 60) << "a 1 s GOP at 60 fps";
    EXPECT_EQ(keyframes[2], 120);
}

// The macOS stream host on its capture manager, with real ScreenCaptureKit
// capture of the main display. ScreenCaptureKit's source calls into the main
// queue, so the host runs on a worker thread while the main thread runs its
// run loop, as in the client. Needs the Screen Recording permission. Run on
// request only:
//   mello_tests --gtest_also_run_disabled_tests --gtest_filter=VideoPipelineMac.*
TEST(VideoPipelineMac, DISABLED_TheHostEncodesTheDisplayOnItsCaptureManager) {
    std::atomic<bool> done{false};
    std::atomic<int> packets{0}, keyframes{0};
    bool started = false, had_manager = false;
    uint64_t captured = 0;
    std::thread worker([&] {
        VideoPipeline host;
        CaptureSourceDesc desc{};
        desc.mode = CaptureMode::Monitor;
        desc.monitor_index = 0;
        PipelineConfig pc{};
        pc.height = 720;
        pc.fps = 60;
        pc.bitrate_kbps = 4000;
        started = host.start_host(desc, pc, [&](const uint8_t*, size_t, bool key, uint64_t) {
            ++packets;
            if (key) ++keyframes;
        });
        had_manager = host.capture_manager() != nullptr;
        if (started) {
            host.request_keyframe();
            std::this_thread::sleep_for(std::chrono::seconds(2));
            VideoPipeline::HostTelemetry t;
            host.get_host_telemetry(t);
            captured = t.frames_captured;
            host.stop_host();
        }
        done = true;
    });
    while (!done.load()) CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, true);
    worker.join();
    ASSERT_TRUE(started) << "the host did not start (Screen Recording permission?)";
    EXPECT_TRUE(had_manager) << "the macOS host takes its capture from a capture manager";
    EXPECT_GT(packets.load(), 10) << "frames from the encode thread";
    EXPECT_GE(keyframes.load(), 1);
    std::printf("[ VideoPipelineMac ] %d packets, %d keyframes, %llu frames captured in 2 s\n", packets.load(),
                keyframes.load(), static_cast<unsigned long long>(captured));
}
#endif
