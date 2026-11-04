// The game clip MP4 (CLIPS.md 15.4, 15.5 and 15.13): H.264 passthrough
// from the replay ring, Opus from the clip mix, the moov box first, the poster.
// The MP4 reads back through m3llo's own demux (mp4_read.hpp), so the mux
// tests run on every platform. The replay session tests are Windows only
// until the macOS replay.
// The video is a fixture that libmello's own recording encoder made once
// (160x96, 30 fps, an IDR every 30 frames with SPS and PPS, no B-frames, an
// AUD before each access unit). No GPL encoder: m3llo is Apache 2.0
// (14-VIDEO-PIPELINE.md 6.7). ReplayFixture.DISABLED_MakeTheH264Fixture
// makes it again:
//   mello_tests --gtest_also_run_disabled_tests --gtest_filter=ReplayFixture.*
#include <gtest/gtest.h>
#include "replay/h264_nal.hpp"
#include "replay/mp4_read.hpp"
#include "replay/replay_audio_mix.hpp"
#include "replay/replay_mux.hpp"
#include "replay/replay_video_ring.hpp"

#ifdef _WIN32
#include "replay/replay_session.hpp"
#include "video/capture_process.hpp"
#include "video/encoder_factory.hpp"
#include "video/graphics_device.hpp"
#include "video/video_pipeline.hpp"
#include "video/video_preprocessor.hpp"

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <wrl/client.h>
using Microsoft::WRL::ComPtr;
#endif

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <string>
#include <thread>
#include <vector>

using namespace mello::replay;
namespace fs = std::filesystem;

namespace {

class TempDir {
public:
    TempDir() {
        const auto stamp = std::chrono::steady_clock::now().time_since_epoch().count();
        path_ = fs::temp_directory_path() / ("mello_replay_mux_test_" + std::to_string(stamp));
        fs::create_directories(path_);
    }
    ~TempDir() {
        std::error_code ec;
        fs::remove_all(path_, ec);
    }
    const fs::path& path() const { return path_; }

private:
    fs::path path_;
};

// The fixture's access units: each starts at an AUD.
std::vector<std::vector<uint8_t>> fixture_units() {
    const fs::path file = fs::path(MELLO_REPLAY_FIXTURE_DIR) / "gop30_160x96.h264";
    std::ifstream f(file, std::ios::binary);
    std::vector<uint8_t> all((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
    std::vector<std::vector<uint8_t>> units;
    h264::for_each_nal(all.data(), all.size(), [&](uint8_t type, const uint8_t* nal, size_t size) {
        if (type == h264::kNalAud || units.empty()) units.emplace_back();
        static const uint8_t kStart[] = {0, 0, 0, 1};
        units.back().insert(units.back().end(), kStart, kStart + 4);
        units.back().insert(units.back().end(), nal, nal + size);
    });
    return units;
}

// A filler NAL that pads a unit to about `bytes`, as an encoder at a real
// bitrate would fill it.
void pad_to(std::vector<uint8_t>& unit, size_t bytes) {
    if (unit.size() + 8 >= bytes) return;
    static const uint8_t kStart[] = {0, 0, 0, 1};
    unit.insert(unit.end(), kStart, kStart + 4);
    unit.push_back(h264::kNalFiller);
    unit.insert(unit.end(), bytes - unit.size() - 1, 0xFF);
    unit.push_back(0x80);
}

// The video samples of an MP4 as the demux reads them. Times in 100 ns.
struct Mp4Video {
    std::vector<int64_t> times;
    std::vector<int64_t> durations;
    std::vector<bool> clean_points;
};

Mp4Video read_video(const fs::path& mp4) {
    Mp4Video v;
    read_mp4_video(
        mp4,
        [&](const Mp4VideoUnit& u) {
            v.times.push_back(u.time_us * 10);
            v.clean_points.push_back(u.keyframe);
            return true;
        },
        nullptr, nullptr);
    for (size_t i = 0; i < v.times.size(); ++i) {
        v.durations.push_back(i + 1 < v.times.size() ? v.times[i + 1] - v.times[i] : 0);
    }
    return v;
}

// The sound of an MP4, decoded to 48 kHz stereo int16 from the start of play.
std::vector<int16_t> read_audio(const fs::path& mp4) {
    std::vector<int16_t> pcm;
    read_mp4_audio(mp4, [&](const int16_t* s, size_t frames, int64_t) {
        pcm.insert(pcm.end(), s, s + frames * 2);
        return true;
    });
    return pcm;
}

// The top-level boxes of an MP4, in file order.
std::vector<std::string> top_level_boxes(const fs::path& mp4) {
    std::vector<std::string> boxes;
    std::ifstream f(mp4, std::ios::binary);
    uint64_t at = 0;
    const uint64_t size = fs::file_size(mp4);
    while (at + 8 <= size) {
        uint8_t h[16];
        f.seekg(static_cast<std::streamoff>(at));
        f.read(reinterpret_cast<char*>(h), 8);
        uint64_t box = (uint64_t{h[0]} << 24) | (uint64_t{h[1]} << 16) | (uint64_t{h[2]} << 8) | h[3];
        boxes.emplace_back(reinterpret_cast<const char*>(h + 4), 4);
        if (box == 1) {
            f.read(reinterpret_cast<char*>(h + 8), 8);
            box = 0;
            for (int i = 8; i < 16; ++i) box = (box << 8) | h[i];
        }
        if (box < 8) break;
        at += box;
    }
    return boxes;
}

ReplayVideoRing::Config ring_config(const TempDir& dir, uint64_t segment_bytes) {
    ReplayVideoRing::Config c;
    c.dir = dir.path();
    c.segment_bytes = segment_bytes;
    c.writer_thread = false;
    return c;
}

constexpr uint64_t kT0 = 50'000'000;
constexpr uint64_t kFrameUs = 33'333;

} // namespace

TEST(ReplayMux, TheMp4HasTheUnitsAtTheirCaptureTimesWithTheMoovFirst) {
    TempDir dir;
    const auto units = fixture_units();
    ASSERT_EQ(units.size(), 60u);

    // The game stalls for 100 ms after frame 40: the frame before the stall
    // lasts 133 ms.
    ReplayVideoRing ring;
    ASSERT_TRUE(ring.start(ring_config(dir, 64 * 1024)));
    std::vector<uint64_t> capture(units.size());
    for (size_t i = 0; i < units.size(); ++i) {
        capture[i] = kT0 + i * kFrameUs + (i > 40 ? 100'000 : 0);
        auto copy = units[i];
        ring.push(std::move(copy), capture[i], h264::has_idr(units[i].data(), units[i].size()));
    }
    ring.flush();
    const uint64_t end = capture.back() + kFrameUs;
    auto window = ring.pin_window(kT0, end);
    ASSERT_EQ(window.units().size(), 60u);

    ClipMuxInput in;
    in.units = window.units();
    in.read_unit = [&](const AccessUnitRef& u, std::vector<uint8_t>& out) { return window.read(u, out); };
    in.width = 160;
    in.height = 96;
    in.fps = 30;
    in.end_us = end;
    const fs::path mp4 = dir.path() / "clip.mp4";
    ClipMuxResult result;
    ASSERT_TRUE(write_clip_mp4(in, mp4, &result));
    EXPECT_EQ(result.video_frames, 60u);

    const auto boxes = top_level_boxes(mp4);
    const auto moov = std::find(boxes.begin(), boxes.end(), "moov");
    const auto mdat = std::find(boxes.begin(), boxes.end(), "mdat");
    ASSERT_NE(moov, boxes.end());
    ASSERT_NE(mdat, boxes.end());
    EXPECT_LT(moov - boxes.begin(), mdat - boxes.begin()) << "moov before mdat";

    const auto v = read_video(mp4);
    ASSERT_EQ(v.times.size(), 60u);
    EXPECT_TRUE(v.clean_points[0]) << "the first sample is a keyframe";
    EXPECT_TRUE(v.clean_points[30]);
    EXPECT_FALSE(v.clean_points[1]);
    for (size_t i = 0; i < v.times.size(); ++i) {
        const double expected_ms = static_cast<double>(capture[i] - kT0) / 1000.0;
        EXPECT_NEAR(static_cast<double>(v.times[i]) / 1e4, expected_ms, 1.0) << "frame " << i;
    }
    EXPECT_NEAR(static_cast<double>(v.durations[40]) / 1e4, 133.3, 1.0) << "the stall";
}

// The audio of the clip lands on the video timeline: a click in the mix at
// 1.0 s of the clip is at 1.0 s in the decoded MP4.
TEST(ReplayMux, TheAudioIsOnTheVideoTimeline) {
    TempDir dir;
    const auto units = fixture_units();
    ReplayVideoRing ring;
    ASSERT_TRUE(ring.start(ring_config(dir, 64 * 1024)));
    for (size_t i = 0; i < units.size(); ++i) {
        auto copy = units[i];
        ring.push(std::move(copy), kT0 + i * kFrameUs, h264::has_idr(units[i].data(), units[i].size()));
    }
    ring.flush();
    const uint64_t end = kT0 + units.size() * kFrameUs;
    auto window = ring.pin_window(kT0, end);

    ClipAudioSource game;
    game.channels = 2;
    for (uint64_t f = 0; f < 96000; f += 480) {
        TimedPcm c;
        c.capture_us = kT0 + f * 1'000'000 / 48000;
        c.samples.assign(960, 0);
        if (f == 48000) {
            c.samples[0] = 20000;
            c.samples[1] = 20000;
        }
        game.chunks.push_back(std::move(c));
    }

    ClipMuxInput in;
    in.units = window.units();
    in.read_unit = [&](const AccessUnitRef& u, std::vector<uint8_t>& out) { return window.read(u, out); };
    in.width = 160;
    in.height = 96;
    in.fps = 30;
    in.end_us = end;
    in.audio = mix_clip_audio(kT0, end, {game});
    const fs::path mp4 = dir.path() / "clip.mp4";
    ASSERT_TRUE(write_clip_mp4(in, mp4));

    const auto pcm = read_audio(mp4);
    ASSERT_GT(pcm.size(), 2u * 48000);
    size_t peak = 0;
    for (size_t i = 0; i < pcm.size() / 2; ++i) {
        if (std::abs(pcm[2 * i]) > std::abs(pcm[2 * peak])) peak = i;
    }
    const double off_ms = (static_cast<double>(peak) - 48000.0) / 48.0;
    EXPECT_LE(std::abs(off_ms), 1.0) << "the click is " << off_ms << " ms off";
}

// The poster decodes through the viewer's decoder. Linux has no JPEG writer:
// it records no game clips.
#ifndef __linux__
TEST(ReplayMux, APosterFromTheMp4IsAJpeg) {
#ifdef _WIN32
    // The decode is NVDEC, AMF or OpenH264 through the viewer. The Windows CI
    // runner is a service: in session 0 the CUDA interop of NVDEC fails.
    if (std::getenv("CI")) GTEST_SKIP() << "hardware decode skipped under CI on Windows";
#endif
    TempDir dir;
    const auto units = fixture_units();
    ReplayVideoRing ring;
    ASSERT_TRUE(ring.start(ring_config(dir, 64 * 1024)));
    for (size_t i = 0; i < units.size(); ++i) {
        auto copy = units[i];
        ring.push(std::move(copy), kT0 + i * kFrameUs, h264::has_idr(units[i].data(), units[i].size()));
    }
    ring.flush();
    auto window = ring.pin_window(kT0, UINT64_MAX);
    ClipMuxInput in;
    in.units = window.units();
    in.read_unit = [&](const AccessUnitRef& u, std::vector<uint8_t>& out) { return window.read(u, out); };
    in.width = 160;
    in.height = 96;
    in.fps = 30;
    in.end_us = kT0 + 60 * kFrameUs;
    const fs::path mp4 = dir.path() / "clip.mp4";
    ASSERT_TRUE(write_clip_mp4(in, mp4));

    // The last IDR of the clip: frame 30, at 1.0 s.
    const fs::path jpeg = dir.path() / "poster.jpg";
    ASSERT_TRUE(write_poster_from_mp4(mp4, 30 * kFrameUs, jpeg));
    std::ifstream f(jpeg, std::ios::binary);
    uint8_t soi[2] = {};
    f.read(reinterpret_cast<char*>(soi), 2);
    EXPECT_EQ(soi[0], 0xFF);
    EXPECT_EQ(soi[1], 0xD8) << "a JPEG starts with SOI";
    EXPECT_GT(fs::file_size(jpeg), 500u);
    EXPECT_LT(fs::file_size(jpeg), 1024u * 1024u) << "under 1 MB (12-STREAMING.md 3.7, G1)";
}

#endif  // __linux__

// CLIPS.md 15.4 and 15.13 (ReplayMuxTime): a 30 s clip at the default
// quality (720p60 at 5 Mbps: about 10 KB a frame) muxes in under 3 s. The
// fixture's units are padded with filler NAL units to that size.
TEST(ReplayMuxTime, ThirtySecondsAtTheDefaultQualityMuxInUnderThreeSeconds) {
    TempDir dir;
    const auto units = fixture_units();
    ReplayVideoRing ring;
    ASSERT_TRUE(ring.start(ring_config(dir, ReplayVideoRing::segment_bytes_for(5'000))));
    constexpr uint64_t kFrame60 = 16'667;
    constexpr size_t kFrames = 30 * 60 + 60;
    const size_t frame_bytes = 5'000'000 / 8 / 60;
    for (size_t i = 0; i < kFrames; ++i) {
        // The 60 fixture units over and over: one segment a second at 60 fps,
        // as the replay encodes. The fixture's second IDR (unit 30) sits
        // inside the segment, which a decoder takes as it is.
        auto unit = units[i % units.size()];
        const bool idr = h264::has_idr(unit.data(), unit.size());
        pad_to(unit, idr ? frame_bytes * 4 : frame_bytes);
        ring.push(std::move(unit), kT0 + i * kFrame60, i % 60 == 0);
        if (i % 60 == 59) ring.flush();
    }
    ring.flush();
    const uint64_t end = kT0 + kFrames * kFrame60;
    const uint64_t start = end - 30'000'000;

    ClipAudioSource game;
    game.channels = 2;
    for (uint64_t f = 0; f < 31 * 48000; f += 480) {
        TimedPcm c;
        c.capture_us = start - 500'000 + f * 1'000'000 / 48000;
        c.samples.resize(960);
        for (size_t i = 0; i < c.samples.size(); ++i) c.samples[i] = static_cast<int16_t>((f + i) * 37);
        game.chunks.push_back(std::move(c));
    }

    const auto t_start = std::chrono::steady_clock::now();
    auto window = ring.pin_window(start, end);
    ASSERT_FALSE(window.empty());
    ClipMuxInput in;
    in.units = window.units();
    in.read_unit = [&](const AccessUnitRef& u, std::vector<uint8_t>& out) { return window.read(u, out); };
    in.width = 160;
    in.height = 96;
    in.fps = 60;
    in.end_us = end;
    in.audio = mix_clip_audio(window.units().front().capture_us, end, {game});
    const fs::path mp4 = dir.path() / "clip.mp4";
    ClipMuxResult result;
    ASSERT_TRUE(write_clip_mp4(in, mp4, &result));
    const double seconds =
        std::chrono::duration<double>(std::chrono::steady_clock::now() - t_start).count();

    EXPECT_GE(result.duration_us, 30'000'000u);
    EXPECT_LE(result.duration_us, 31'000'000u);
    EXPECT_GT(result.bytes, 15'000'000u) << "the default quality's size";
    EXPECT_LT(seconds, 3.0) << "mux of 30 s took " << seconds << " s";
    std::printf("[ ReplayMuxTime ] 30 s clip, %.1f MB, muxed in %.2f s\n",
                static_cast<double>(result.bytes) / 1e6, seconds);
}

#ifdef _WIN32
// Makes the fixture of the tests above with the recording encoder of this
// machine: 60 frames of a moving pattern, an IDR every 30 frames, each access
// unit after an AUD. Run on request only (see the top of this file).
TEST(ReplayFixture, DISABLED_MakeTheH264Fixture) {
    using namespace mello::video;
    constexpr uint32_t w = 160, h = 96, frames = 60;
    GraphicsDevice device = create_d3d11_device();
    ASSERT_TRUE(device.d3d11()) << "no D3D11 device";
    ComPtr<ID3D11DeviceContext> context;
    device.d3d11()->GetImmediateContext(&context);

    EncoderConfig enc{w, h, 30, 150};
    enc.keyframe_interval = 30;
    enc.intent = EncoderIntent::Recording;
    auto encoder = create_best_encoder(device, enc);
    ASSERT_TRUE(encoder) << "no encoder";
    VideoPreprocessor pre;
    ASSERT_TRUE(pre.initialize(device, w, h, w, h));

    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = w;
    desc.Height = h;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    ComPtr<ID3D11Texture2D> source;
    ASSERT_TRUE(SUCCEEDED(device.d3d11()->CreateTexture2D(&desc, nullptr, &source)));

    std::vector<uint8_t> bgra(static_cast<size_t>(w) * h * 4);
    std::vector<uint8_t> out;
    static const uint8_t kAud[] = {0, 0, 0, 1, h264::kNalAud, 0xF0};
    for (uint32_t n = 0; n < frames; ++n) {
        // A diagonal gradient that moves, and a white square that crosses it.
        for (uint32_t y = 0; y < h; ++y) {
            for (uint32_t x = 0; x < w; ++x) {
                uint8_t* p = &bgra[(static_cast<size_t>(y) * w + x) * 4];
                const bool square = x >= n * 2 && x < n * 2 + 24 && y >= 36 && y < 60;
                p[0] = square ? 255 : static_cast<uint8_t>((x + n * 3) & 0xFF);
                p[1] = square ? 255 : static_cast<uint8_t>((y * 2 + n) & 0xFF);
                p[2] = square ? 255 : static_cast<uint8_t>((x + y) & 0xFF);
                p[3] = 255;
            }
        }
        context->UpdateSubresource(source.Get(), 0, nullptr, bgra.data(), w * 4, 0);
        const ConvertResult nv12 = pre.convert(source.Get());
        ASSERT_TRUE(nv12.texture);
        EncodedPacket packet;
        ASSERT_TRUE(encoder->encode(nv12.texture, packet));
        ASSERT_FALSE(packet.data.empty());
        EXPECT_EQ(packet.is_keyframe, n % 30 == 0) << "frame " << n;
        bool has_aud = false;
        h264::for_each_nal(packet.data.data(), packet.data.size(),
                           [&](uint8_t type, const uint8_t*, size_t) { has_aud |= type == h264::kNalAud; });
        if (!has_aud) out.insert(out.end(), kAud, kAud + sizeof(kAud));
        out.insert(out.end(), packet.data.begin(), packet.data.end());
    }
    encoder->shutdown();
    encoder.reset();
    pre.shutdown();
    source.Reset();
    context.Reset();
    device.d3d11()->Release();

    const fs::path file = fs::path(MELLO_REPLAY_FIXTURE_DIR) / "gop30_160x96.h264";
    std::ofstream f(file, std::ios::binary | std::ios::trunc);
    f.write(reinterpret_cast<const char*>(out.data()), static_cast<std::streamsize>(out.size()));
    ASSERT_TRUE(f.good());
    std::printf("[ ReplayFixture ] %s: %zu bytes\n", file.string().c_str(), out.size());
}
#endif  // _WIN32

// The replay session: Windows and macOS.
#if defined(_WIN32) || defined(__APPLE__)
#include "replay/replay_session.hpp"
#include "video/capture_reference.hpp"

namespace {

// A reference clip of 120 frames of 320x240 at 60 fps: a luma ramp that
// moves one pixel a frame.
mello::video::CaptureSourceDesc write_reference_clip(const fs::path& yuv) {
    const uint32_t w = 320, h = 240, frames = 120;
    std::vector<uint8_t> frame(static_cast<size_t>(w) * h * 3 / 2, 128);
    std::ofstream f(yuv, std::ios::binary);
    for (uint32_t n = 0; n < frames; ++n) {
        for (uint32_t y = 0; y < h; ++y)
            for (uint32_t x = 0; x < w; ++x)
                frame[y * w + x] = static_cast<uint8_t>(16 + ((x + n) * 219 / (w + frames)));
        f.write(reinterpret_cast<const char*>(frame.data()), static_cast<std::streamsize>(frame.size()));
    }
    mello::video::CaptureSourceDesc desc{};
    desc.mode = mello::video::CaptureMode::Reference;
    desc.reference.path = yuv.string();
    desc.reference.width = w;
    desc.reference.height = h;
    desc.reference.fps = 60;
    return desc;
}

}  // namespace

// The whole replay on this PC's encoder: a reference clip through the
// capture manager, the recording encoder and the rings, then a capture.
// Needs a graphics device and a hardware encoder, so it skips under CI like
// the other pipeline tests.
TEST(ReplaySession, AClipOfTheReferenceSourceHasItsFramesAndALivePoster) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    TempDir dir;
    const uint32_t w = 320, h = 240;
    const mello::video::CaptureSourceDesc desc = write_reference_clip(dir.path() / "reference.yuv");

    ReplayConfig config;
    config.source = desc;
    config.height = h;
    config.fps = 60;
    config.bitrate_kbps = 2000;
    config.dir = dir.path();
    std::string error;
    auto session = ReplaySession::start(config, &error);
    if (!session && error == "no hardware encoder") GTEST_SKIP() << "no encoder on this machine";
    ASSERT_TRUE(session) << error;

    std::this_thread::sleep_for(std::chrono::seconds(3));
    EXPECT_EQ(session->state(), ReplayState::Recording);
    const fs::path mp4 = dir.path() / "clip.mp4";
    const fs::path poster = dir.path() / "poster.jpg";
    ReplayClip clip;
    ASSERT_TRUE(session->capture(2.0f, nullptr, mp4, poster, &clip));
    const auto stats = session->stats();
    session->stop();

    // 2 s, plus up to one GOP (1 s) back to the keyframe before the start.
    EXPECT_GE(clip.duration_us, 1'950'000u);
    EXPECT_LE(clip.duration_us, 3'100'000u);
    EXPECT_EQ(clip.width, w);
    EXPECT_EQ(clip.height, h);
    EXPECT_TRUE(clip.poster_live) << "the reference source delivers a frame after the press";
    const auto v = read_video(mp4);
    ASSERT_FALSE(v.times.empty());
    EXPECT_TRUE(v.clean_points.front());
    EXPECT_GE(v.times.size(), 110u) << "about 60 frames a second";
    EXPECT_EQ(stats.video_ring.queue_drops, 0u);
    EXPECT_EQ(stats.video_ring.overflow_drops, 0u);
    std::ifstream f(poster, std::ios::binary);
    uint8_t soi[2] = {};
    f.read(reinterpret_cast<char*>(soi), 2);
    EXPECT_EQ(soi[0], 0xFF);
    EXPECT_EQ(soi[1], 0xD8);
    std::printf("[ ReplaySession ] %.2f s, %u frames, %llu bytes, %llu captured, %llu encoded\n",
                static_cast<double>(clip.duration_us) / 1e6, static_cast<unsigned>(v.times.size()),
                static_cast<unsigned long long>(clip.bytes),
                static_cast<unsigned long long>(stats.frames_captured),
                static_cast<unsigned long long>(stats.frames_encoded));
}

// The stream gate's replay lane (CLIPS.md 15.13): the reference source's game
// audio goes into the clip in place of a process loopback, and the read-back
// gives the clip's video units and its sound. The reference audio loops with
// the 2 s clip and clicks once a loop, so the decoded clicks are 2 s apart.
TEST(ReplaySession, TheReferenceGameAudioIsInTheClipAndReadsBack) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    TempDir dir;
    mello::video::CaptureSourceDesc desc = write_reference_clip(dir.path() / "reference.yuv");
    const fs::path pcm = dir.path() / "reference.s16";
    {
        std::vector<int16_t> loop(96000 * 2, 0);  // 2 s of 48 kHz stereo
        for (size_t i = 24000; i < 24000 + 240; ++i) {  // 5 ms at 0.5 s
            loop[2 * i] = 20000;
            loop[2 * i + 1] = 20000;
        }
        std::ofstream f(pcm, std::ios::binary);
        f.write(reinterpret_cast<const char*>(loop.data()), static_cast<std::streamsize>(loop.size() * 2));
    }
    desc.reference.audio_path = pcm.string();

    ReplayConfig config;
    config.source = desc;
    config.height = 240;
    config.fps = 60;
    config.bitrate_kbps = 2000;
    config.dir = dir.path();
    std::string error;
    auto session = ReplaySession::start(config, &error);
    if (!session && error == "no hardware encoder") GTEST_SKIP() << "no encoder on this machine";
    ASSERT_TRUE(session) << error;
    std::this_thread::sleep_for(std::chrono::seconds(6));
    mello::video::ReferenceInfo info;
    EXPECT_TRUE(session->reference_info(info));
    EXPECT_GT(info.start_us, 0u) << "the clock of the source once it delivers";
    const fs::path mp4 = dir.path() / "clip.mp4";
    ReplayClip clip;
    ASSERT_TRUE(session->capture(4.0f, nullptr, mp4, dir.path() / "poster.jpg", &clip));
    session->stop();
    EXPECT_TRUE(clip.has_game_audio);

    size_t units = 0;
    bool first_key = false;
    uint32_t w = 0, h = 0;
    ASSERT_TRUE(read_mp4_video(
        mp4,
        [&](const Mp4VideoUnit& u) {
            if (units == 0) first_key = u.keyframe && h264::has_idr(u.annex_b.data(), u.annex_b.size());
            ++units;
            return true;
        },
        &w, &h));
    EXPECT_TRUE(first_key) << "the read-back starts at a key frame with its parameter sets";
    EXPECT_EQ(w, 320u);
    EXPECT_EQ(h, 240u);
    EXPECT_GE(units, 230u) << "4 s at 60 fps";

    std::vector<int64_t> clicks;  // clip time of each click, µs
    int64_t last = -1'000'000;
    ASSERT_TRUE(read_mp4_audio(mp4, [&](const int16_t* s, size_t frames, int64_t time_us) {
        for (size_t i = 0; i < frames; ++i) {
            const int64_t t = time_us + static_cast<int64_t>(i) * 1'000'000 / 48000;
            if (std::abs(s[2 * i]) > 8000 && t - last > 500'000) {
                clicks.push_back(t);
                last = t;
            }
        }
        return true;
    }));
    ASSERT_GE(clicks.size(), 2u) << "a click every 2 s of the reference audio";
    for (size_t i = 1; i < clicks.size(); ++i) {
        EXPECT_NEAR(static_cast<double>(clicks[i] - clicks[i - 1]) / 1000.0, 2000.0, 2.0)
            << "the reference audio is at its capture times";
    }
}
#endif  // _WIN32 || __APPLE__

#ifdef _WIN32

// CLIPS.md 15.3; 14-VIDEO-PIPELINE.md 4.8: a stream of the same game starts, and the
// replay moves onto the stream's capture and keeps recording while the
// stream keeps its frames. Before the stream stops, the replay goes back to
// its own capture and keeps recording. A clip over both switches plays.
TEST(ReplaySession, FollowsAStreamOfTheSameGameAndComesBack) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    TempDir dir;
    const mello::video::CaptureSourceDesc desc = write_reference_clip(dir.path() / "reference.yuv");
    ReplayConfig config;
    config.source = desc;
    config.height = 240;
    config.fps = 60;
    config.bitrate_kbps = 2000;
    config.dir = dir.path();
    std::string error;
    auto session = ReplaySession::start(config, &error);
    if (!session && error == "no hardware encoder") GTEST_SKIP() << "no encoder on this machine";
    ASSERT_TRUE(session) << error;
    EXPECT_EQ(session->game_key(), ReplaySession::kReferenceKey);

    std::this_thread::sleep_for(std::chrono::seconds(1));
    const uint64_t alone = session->stats().frames_encoded;

    mello::video::VideoPipeline stream;
    mello::video::PipelineConfig pc{};
    pc.fps = 60;
    pc.bitrate_kbps = 2000;
    std::atomic<int> packets{0};
    ASSERT_TRUE(stream.start_host(desc, pc, [&](const uint8_t*, size_t, bool, uint64_t) { ++packets; }));
    // Another game: the replay stays where it is.
    session->follow_stream(stream.shared_capture_manager(), 1234, 60);
    EXPECT_FALSE(session->following_stream());
    session->follow_stream(stream.shared_capture_manager(), ReplaySession::kReferenceKey, 60);
    EXPECT_TRUE(session->follows(stream.capture_manager()));

    std::this_thread::sleep_for(std::chrono::seconds(1));
    const uint64_t following = session->stats().frames_encoded;
    EXPECT_GT(following, alone + 30) << "the replay records from the stream's capture";
    EXPECT_GT(packets.load(), 30) << "the stream keeps its frames";
    EXPECT_EQ(session->state(), ReplayState::Recording);

    session->leave_stream();
    EXPECT_FALSE(session->following_stream());
    const int packets_at_leave = packets.load();
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    EXPECT_GT(packets.load(), packets_at_leave) << "the stream does not notice the replay leave";
    stream.stop_host();

    std::this_thread::sleep_for(std::chrono::seconds(1));
    EXPECT_GT(session->stats().frames_encoded, following + 30) << "back on its own capture";
    EXPECT_EQ(session->state(), ReplayState::Recording);

    const fs::path mp4 = dir.path() / "clip.mp4";
    const fs::path poster = dir.path() / "poster.jpg";
    ReplayClip clip;
    ASSERT_TRUE(session->capture(3.0f, nullptr, mp4, poster, &clip));
    session->stop();
    const auto v = read_video(mp4);
    ASSERT_FALSE(v.times.empty());
    EXPECT_TRUE(v.clean_points.front());
    EXPECT_EQ(clip.width, 320u) << "one picture size over the switches";
    EXPECT_GE(v.times.size(), 150u) << "about 60 frames a second, a short gap at each switch";
    LONGLONG longest_gap = 0;
    for (const LONGLONG d : v.durations) longest_gap = (std::max)(longest_gap, d);
    std::printf("[ ReplaySession ] follow and back: %u frames in %.2f s, longest frame %.0f ms\n",
                static_cast<unsigned>(v.times.size()), static_cast<double>(clip.duration_us) / 1e6,
                static_cast<double>(longest_gap) / 1e4);
}

// On request only: the replay on a running game, as the client starts it.
//   set MELLO_REPLAY_PID=<pid>
//   mello_tests --gtest_also_run_disabled_tests --gtest_filter=ReplaySession.DISABLED_*
TEST(ReplaySession, DISABLED_RecordsTheGameOfMelloReplayPid) {
    const char* pid_text = std::getenv("MELLO_REPLAY_PID");
    ASSERT_TRUE(pid_text) << "set MELLO_REPLAY_PID";
    const uint32_t pid = static_cast<uint32_t>(std::strtoul(pid_text, nullptr, 10));
    HWND hwnd = mello::video::find_main_window(pid);
    char cls[128] = {};
    RECT rc{};
    if (hwnd) {
        GetClassNameA(hwnd, cls, sizeof(cls));
        GetClientRect(hwnd, &rc);
    }
    std::printf("[ ReplayPid ] pid %u: window %p class \"%s\" client %ldx%ld visible %d iconic %d\n",
                pid, static_cast<void*>(hwnd), cls, rc.right - rc.left, rc.bottom - rc.top,
                hwnd ? IsWindowVisible(hwnd) : 0, hwnd ? IsIconic(hwnd) : 0);
    ASSERT_TRUE(hwnd);

    TempDir dir;
    ReplayConfig config;
    config.window = hwnd;
    config.pid = pid;
    config.dir = dir.path();
    std::string error;
    auto session = ReplaySession::start(config, &error);
    ASSERT_TRUE(session) << error;
    std::this_thread::sleep_for(std::chrono::seconds(4));
    ReplayClip clip;
    const bool ok = session->capture(3.0f, nullptr, dir.path() / "clip.mp4", dir.path() / "poster.jpg", &clip);
    const auto stats = session->stats();
    session->stop();
    std::printf("[ ReplayPid ] clip %d: %.2f s %ux%u, %llu captured, %llu encoded, game audio %d\n", ok,
                static_cast<double>(clip.duration_us) / 1e6, clip.width, clip.height,
                static_cast<unsigned long long>(stats.frames_captured),
                static_cast<unsigned long long>(stats.frames_encoded), clip.has_game_audio);
    EXPECT_TRUE(ok);
}

// 12-STREAMING.md 3.7, F3: the stream that the replay follows is overloaded
// twice. The replay drops to 30 fps, then pauses, while the stream keeps its
// frames. A third time the replay has nothing left, and the stream moves its
// own cost tier. A clip over the pause shows the last frame for the gap.
TEST(ReplaySession, UnderOverloadTheReplayShedsBeforeTheStream) {
    if (std::getenv("CI")) GTEST_SKIP() << "hardware-dependent test skipped under CI";
    TempDir dir;
    const mello::video::CaptureSourceDesc desc = write_reference_clip(dir.path() / "reference.yuv");
    ReplayConfig config;
    config.source = desc;
    config.height = 240;
    config.fps = 60;
    config.bitrate_kbps = 2000;
    config.dir = dir.path();
    std::string error;
    auto session = ReplaySession::start(config, &error);
    if (!session && error == "no hardware encoder") GTEST_SKIP() << "no encoder on this machine";
    ASSERT_TRUE(session) << error;

    mello::video::VideoPipeline stream;
    mello::video::PipelineConfig pc{};
    pc.fps = 60;
    pc.bitrate_kbps = 2000;
    std::atomic<int> packets{0};
    ASSERT_TRUE(stream.start_host(desc, pc, [&](const uint8_t*, size_t, bool, uint64_t) { ++packets; }));
    session->follow_stream(stream.shared_capture_manager(), ReplaySession::kReferenceKey, 60);
    ASSERT_TRUE(session->follows(stream.capture_manager()));
    std::this_thread::sleep_for(std::chrono::seconds(1));

    EXPECT_TRUE(session->shed_for_stream());
    EXPECT_EQ(session->load_level(), ReplayLoad::Level::Reduced);
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    const uint64_t r0 = session->stats().frames_encoded;
    std::this_thread::sleep_for(std::chrono::seconds(1));
    const uint64_t reduced = session->stats().frames_encoded - r0;
    EXPECT_GE(reduced, 25u) << "about 30 frames a second";
    EXPECT_LE(reduced, 35u) << "about 30 frames a second";

    EXPECT_TRUE(session->shed_for_stream());
    EXPECT_EQ(session->state(), ReplayState::Paused);
    std::this_thread::sleep_for(std::chrono::milliseconds(200));
    const uint64_t p0 = session->stats().frames_encoded;
    const int s0 = packets.load();
    std::this_thread::sleep_for(std::chrono::milliseconds(500));
    EXPECT_EQ(session->stats().frames_encoded, p0) << "paused: no frame is encoded";
    EXPECT_GT(packets.load(), s0 + 20) << "the stream keeps its frames";
    EXPECT_FALSE(session->shed_for_stream()) << "nothing left: the stream moves its tier";

    const fs::path mp4 = dir.path() / "clip.mp4";
    ReplayClip clip;
    ASSERT_TRUE(session->capture(2.0f, nullptr, mp4, dir.path() / "poster.jpg", &clip));
    session->leave_stream();
    stream.stop_host();
    session->stop();
    const auto v = read_video(mp4);
    ASSERT_FALSE(v.times.empty());
    LONGLONG longest = 0;
    for (const LONGLONG d : v.durations) longest = (std::max)(longest, d);
    EXPECT_GE(longest, 5'000'000) << "the last frame lasts over the pause (at least 0.5 s)";
}
#endif // _WIN32
