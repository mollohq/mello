// The game clip MP4 (plans/GAME-CLIPS.md 5.3, 5.4, 10.2): H.264 passthrough
// from the replay ring, AAC from the clip mix, the moov box first, the poster.
// The video is a fixture made once with ffmpeg (libx264, 160x96, 30 fps, an
// IDR every 30 frames with SPS and PPS, no B-frames, an AUD before each
// access unit):
//   ffmpeg -f lavfi -i testsrc=size=160x96:rate=30 -t 2 -pix_fmt yuv420p
//     -c:v libx264 -profile:v high -bf 0 -g 30 -keyint_min 30 -sc_threshold 0
//     -x264-params repeat-headers=1:aud=1 -f h264 gop30_160x96.h264
#ifdef _WIN32
#include <gtest/gtest.h>
#include "replay/h264_nal.hpp"
#include "replay/replay_audio_mix.hpp"
#include "replay/replay_mux.hpp"
#include "replay/replay_video_ring.hpp"

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <wrl/client.h>

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <string>
#include <vector>

using namespace mello::replay;
using Microsoft::WRL::ComPtr;
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

struct Mp4Video {
    std::vector<LONGLONG> times;
    std::vector<LONGLONG> durations;
    std::vector<bool> clean_points;
};

struct MfScope {
    MfScope() {
        CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
        MFStartup(MF_VERSION);
    }
    ~MfScope() {
        MFShutdown();
        CoUninitialize();
    }
};

// The H.264 samples of an MP4 as they are stored, with no decode.
Mp4Video read_video(const fs::path& mp4) {
    MfScope mf;
    Mp4Video v;
    ComPtr<IMFSourceReader> reader;
    if (FAILED(MFCreateSourceReaderFromURL(mp4.wstring().c_str(), nullptr, &reader))) return v;
    const DWORD video = static_cast<DWORD>(MF_SOURCE_READER_FIRST_VIDEO_STREAM);
    reader->SetStreamSelection(static_cast<DWORD>(MF_SOURCE_READER_ALL_STREAMS), FALSE);
    reader->SetStreamSelection(video, TRUE);
    for (;;) {
        DWORD flags = 0;
        LONGLONG time = 0;
        ComPtr<IMFSample> sample;
        if (FAILED(reader->ReadSample(video, 0, nullptr, &flags, &time, &sample))) break;
        if (flags & MF_SOURCE_READERF_ENDOFSTREAM) break;
        if (!sample) continue;
        LONGLONG duration = 0;
        sample->GetSampleDuration(&duration);
        v.times.push_back(time);
        v.durations.push_back(duration);
        v.clean_points.push_back(MFGetAttributeUINT32(sample.Get(), MFSampleExtension_CleanPoint, FALSE) != 0);
    }
    return v;
}

// The audio of an MP4, decoded to 48 kHz stereo int16.
std::vector<int16_t> read_audio(const fs::path& mp4) {
    MfScope mf;
    std::vector<int16_t> pcm;
    ComPtr<IMFSourceReader> reader;
    if (FAILED(MFCreateSourceReaderFromURL(mp4.wstring().c_str(), nullptr, &reader))) return pcm;
    const DWORD audio = static_cast<DWORD>(MF_SOURCE_READER_FIRST_AUDIO_STREAM);
    ComPtr<IMFMediaType> type;
    MFCreateMediaType(&type);
    type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Audio);
    type->SetGUID(MF_MT_SUBTYPE, MFAudioFormat_PCM);
    type->SetUINT32(MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
    type->SetUINT32(MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000);
    type->SetUINT32(MF_MT_AUDIO_NUM_CHANNELS, 2);
    if (FAILED(reader->SetCurrentMediaType(audio, nullptr, type.Get()))) return pcm;
    for (;;) {
        DWORD flags = 0;
        ComPtr<IMFSample> sample;
        if (FAILED(reader->ReadSample(audio, 0, nullptr, &flags, nullptr, &sample))) break;
        if (flags & MF_SOURCE_READERF_ENDOFSTREAM) break;
        if (!sample) continue;
        ComPtr<IMFMediaBuffer> buffer;
        sample->ConvertToContiguousBuffer(&buffer);
        BYTE* data = nullptr;
        DWORD length = 0;
        buffer->Lock(&data, nullptr, &length);
        const size_t at = pcm.size();
        pcm.resize(at + length / 2);
        std::memcpy(pcm.data() + at, data, length);
        buffer->Unlock();
    }
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

TEST(ReplayMux, APosterFromTheMp4IsAJpeg) {
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
    EXPECT_LT(fs::file_size(jpeg), 1024u * 1024u) << "under 1 MB (plan G1)";
}

// Game clips plan 5.2 and 10.2 (ReplayMuxTime): a 30 s clip at the default
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
#endif // _WIN32
