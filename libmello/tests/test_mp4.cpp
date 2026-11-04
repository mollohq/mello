// m3llo's own MP4 writer and demux (CLIPS.md 15.5 and 15.9), on every
// platform: the box tree of a clip, Opus in ISOBMFF (dOps, the pre-skip as the
// edit, the roll group), the sound on the timeline, a file from another
// writer (ffmpeg, fixtures/replay/ffmpeg_opus_160x96.mp4), and files that are
// cut or damaged, which a clip from another user can be.
#include <gtest/gtest.h>
#include "replay/h264_nal.hpp"
#include "replay/mp4_box.hpp"
#include "replay/mp4_demux.hpp"
#include "replay/mp4_read.hpp"
#include "replay/replay_audio_mix.hpp"
#include "replay/replay_mux.hpp"

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cmath>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <random>
#include <string>
#include <vector>

using namespace mello::replay;
namespace fs = std::filesystem;

namespace {

class TempDir {
public:
    TempDir() {
        const auto stamp = std::chrono::steady_clock::now().time_since_epoch().count();
        path_ = fs::temp_directory_path() / ("mello_mp4_test_" + std::to_string(stamp));
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

std::vector<uint8_t> read_file(const fs::path& p) {
    std::ifstream f(p, std::ios::binary);
    return {std::istreambuf_iterator<char>(f), std::istreambuf_iterator<char>()};
}

void write_file(const fs::path& p, const std::vector<uint8_t>& bytes) {
    std::ofstream f(p, std::ios::binary | std::ios::trunc);
    f.write(reinterpret_cast<const char*>(bytes.data()), static_cast<std::streamsize>(bytes.size()));
}

// The H.264 fixture's access units (160x96, 30 fps, an IDR every 30 frames).
std::vector<std::vector<uint8_t>> fixture_units() {
    const auto all = read_file(fs::path(MELLO_REPLAY_FIXTURE_DIR) / "gop30_160x96.h264");
    std::vector<std::vector<uint8_t>> units;
    h264::for_each_nal(all.data(), all.size(), [&](uint8_t type, const uint8_t* nal, size_t size) {
        if (type == h264::kNalAud || units.empty()) units.emplace_back();
        static const uint8_t kStart[] = {0, 0, 0, 1};
        units.back().insert(units.back().end(), kStart, kStart + 4);
        units.back().insert(units.back().end(), nal, nal + size);
    });
    return units;
}

constexpr double kPi = 3.14159265358979323846;
constexpr uint64_t kT0 = 50'000'000;
constexpr uint64_t kFrameUs = 33'333;

/// A clip of the 60 fixture units with `audio` (48 kHz stereo from T0).
struct Clip {
    std::vector<std::vector<uint8_t>> units;
    std::vector<AccessUnitRef> refs;
    ClipMuxInput in;
};

Clip make_clip(std::vector<int16_t> audio) {
    Clip c;
    c.units = fixture_units();
    for (size_t i = 0; i < c.units.size(); ++i) {
        AccessUnitRef r;
        r.offset = i;  // the index into `units`
        r.size = static_cast<uint32_t>(c.units[i].size());
        r.capture_us = kT0 + i * kFrameUs;
        r.keyframe = h264::has_idr(c.units[i].data(), c.units[i].size());
        c.refs.push_back(r);
    }
    c.in.units = c.refs;
    c.in.width = 160;
    c.in.height = 96;
    c.in.fps = 30;
    c.in.end_us = kT0 + c.units.size() * kFrameUs;
    c.in.audio = std::move(audio);
    return c;
}

bool write(Clip& c, const fs::path& mp4, ClipMuxResult* result = nullptr) {
    c.in.read_unit = [&c](const AccessUnitRef& u, std::vector<uint8_t>& out) {
        out = c.units[static_cast<size_t>(u.offset)];
        return true;
    };
    return write_clip_mp4(c.in, mp4, result);
}

/// 2 s of a 440 Hz tone on the left and a 660 Hz tone on the right, silent
/// before 0.5 s.
std::vector<int16_t> two_tones() {
    std::vector<int16_t> s(2 * 96000);
    for (size_t i = 24000; i < 96000; ++i) {
        const double t = static_cast<double>(i) / 48000.0;
        s[2 * i] = static_cast<int16_t>(12000 * std::sin(2 * kPi * 440 * t));
        s[2 * i + 1] = static_cast<int16_t>(12000 * std::sin(2 * kPi * 660 * t));
    }
    return s;
}

double goertzel(const std::vector<int16_t>& stereo, int channel, double hz, size_t from, size_t n) {
    const double k = 2 * std::cos(2 * kPi * hz / 48000.0);
    double s1 = 0, s2 = 0;
    for (size_t i = from; i < from + n && 2 * i + 1 < stereo.size(); ++i) {
        const double s = stereo[2 * i + channel] / 32768.0 + k * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    return std::sqrt(std::max(0.0, s1 * s1 + s2 * s2 - k * s1 * s2)) / static_cast<double>(n);
}

bool open_file(const fs::path& p, Mp4Movie& movie, std::string* error = nullptr) {
    uint64_t size = 0;
    const Mp4ReadAt read = file_reader(p, &size);
    return open_mp4(read, size, movie, error);
}

/// The offset and size of the first box of `type` in the file, at any depth
/// of the container boxes on the way to it.
bool find_box(const std::vector<uint8_t>& file, const char* type, size_t& at, size_t& size) {
    const uint32_t want = mp4::fourcc(type);
    for (size_t i = 4; i + 4 <= file.size(); ++i) {
        if (mp4::be32(file.data() + i) == want) {
            at = i - 4;
            size = mp4::be32(file.data() + at);
            return size >= 8 && at + size <= file.size();
        }
    }
    return false;
}

}  // namespace

TEST(Mp4Write, TheClipHasOpusInIsobmffAndTheMoovFirst) {
    TempDir dir;
    Clip clip = make_clip(two_tones());
    const fs::path mp4 = dir.path() / "clip.mp4";
    ClipMuxResult result;
    ASSERT_TRUE(write(clip, mp4, &result));
    EXPECT_EQ(result.video_frames, 60u);
    EXPECT_NEAR(static_cast<double>(result.duration_us), 2'000'000.0, 1000.0);
    EXPECT_FALSE(fs::exists(dir.path() / "clip.mp4.part")) << "no part file is left";

    const auto file = read_file(mp4);
    EXPECT_EQ(result.bytes, file.size());
    // The top level: ftyp, moov, mdat, in that order.
    std::vector<uint32_t> top;
    ASSERT_TRUE(mp4::for_each_box(file.data(), file.size(), [&](const mp4::BoxRef& b) { top.push_back(b.type); }));
    ASSERT_EQ(top.size(), 3u);
    EXPECT_EQ(top[0], mp4::fourcc("ftyp"));
    EXPECT_EQ(top[1], mp4::fourcc("moov"));
    EXPECT_EQ(top[2], mp4::fourcc("mdat"));
    // The brands of the file that every tested player played (CLIPS.md 15.5).
    EXPECT_EQ(std::memcmp(file.data() + 8, "isom\0\0\x02\0isomiso2avc1mp41", 24), 0);

    Mp4Movie movie;
    std::string error;
    ASSERT_TRUE(open_file(mp4, movie, &error)) << error;
    const Mp4Track* v = movie.video();
    const Mp4Track* a = movie.audio();
    ASSERT_NE(v, nullptr);
    ASSERT_NE(a, nullptr);
    EXPECT_EQ(v->codec, mp4::fourcc("avc1"));
    EXPECT_EQ(v->width, 160u);
    EXPECT_EQ(v->height, 96u);
    ASSERT_EQ(v->samples.size(), 60u);
    EXPECT_TRUE(v->samples[0].sync);
    EXPECT_TRUE(v->samples[30].sync);
    EXPECT_FALSE(v->samples[1].sync);
    ASSERT_EQ(v->sps.size(), 1u);
    ASSERT_EQ(v->pps.size(), 1u);
    EXPECT_EQ(v->nal_length_size, 4);

    EXPECT_EQ(a->codec, mp4::fourcc("Opus"));
    EXPECT_EQ(a->channels, 2u);
    EXPECT_EQ(a->timescale, 48000u);
    EXPECT_GT(a->pre_skip, 0);
    EXPECT_EQ(a->edit_media_time, a->pre_skip) << "the edit skips the pre-skip";
    EXPECT_EQ(a->roll_distance, -4) << "80 ms of pre-roll";
    EXPECT_EQ(a->samples.size(), (96000u + a->pre_skip + 959u) / 960u);
    for (const auto& s : a->samples) EXPECT_EQ(s.duration, 960u);

    // The SPS in avcC is the one in the first IDR.
    const auto& first = clip.units[0];
    bool sps_matches = false;
    h264::for_each_nal(first.data(), first.size(), [&](uint8_t type, const uint8_t* nal, size_t size) {
        if (type == h264::kNalSps) sps_matches = std::vector<uint8_t>(nal, nal + size) == v->sps[0];
    });
    EXPECT_TRUE(sps_matches);

    // Every sample lies inside the mdat box.
    const size_t mdat_at = file.size() - (mp4::be32(file.data() + 32 + mp4::be32(file.data() + 32)));
    for (const auto* t : {v, a}) {
        for (const auto& s : t->samples) {
            EXPECT_GE(s.offset, mdat_at + 8);
            EXPECT_LE(s.offset + s.size, file.size());
        }
    }
}

TEST(Mp4Write, EachVideoUnitIsAtItsCaptureTime) {
    TempDir dir;
    Clip clip = make_clip({});
    // The game stalls 100 ms after frame 40.
    for (size_t i = 41; i < clip.refs.size(); ++i) clip.refs[i].capture_us += 100'000;
    clip.in.units = clip.refs;
    clip.in.end_us = clip.refs.back().capture_us + kFrameUs;
    const fs::path mp4 = dir.path() / "clip.mp4";
    ASSERT_TRUE(write(clip, mp4));

    std::vector<Mp4VideoUnit> units;
    uint32_t w = 0, h = 0;
    ASSERT_TRUE(read_mp4_video(
        mp4, [&](const Mp4VideoUnit& u) { units.push_back(u); return true; }, &w, &h));
    EXPECT_EQ(w, 160u);
    ASSERT_EQ(units.size(), 60u);
    for (size_t i = 0; i < units.size(); ++i) {
        EXPECT_NEAR(static_cast<double>(units[i].time_us), static_cast<double>(clip.refs[i].capture_us - kT0), 12.0)
            << "frame " << i;  // 90 kHz ticks: 11 us
        // The read-back is Annex B with the bytes of the unit as it went in.
        std::vector<uint8_t> want;
        h264::for_each_nal(clip.units[i].data(), clip.units[i].size(), [&](uint8_t, const uint8_t* nal, size_t n) {
            static const uint8_t kStart[] = {0, 0, 0, 1};
            want.insert(want.end(), kStart, kStart + 4);
            want.insert(want.end(), nal, nal + n);
        });
        if (i == 0) {
            // The track's SPS and PPS go before the first key frame.
            ASSERT_GE(units[i].annex_b.size(), want.size());
            EXPECT_TRUE(std::equal(want.begin(), want.end(), units[i].annex_b.end() - static_cast<std::ptrdiff_t>(want.size())));
        } else {
            EXPECT_EQ(units[i].annex_b, want) << "frame " << i;
        }
    }
    Mp4Movie movie;
    ASSERT_TRUE(open_file(mp4, movie));
    EXPECT_EQ(movie.audio(), nullptr) << "no sound, no sound track";
    EXPECT_NEAR(static_cast<double>(movie.video()->samples[40].duration) / 90.0, 133.3, 0.1) << "the stall";
}

TEST(Mp4Write, TheSoundStartsAtTZeroOnItsOwnChannels) {
    TempDir dir;
    Clip clip = make_clip(two_tones());
    const fs::path mp4 = dir.path() / "clip.mp4";
    ASSERT_TRUE(write(clip, mp4));

    std::vector<int16_t> pcm;
    int64_t first_time = -1;
    ASSERT_TRUE(read_mp4_audio(mp4, [&](const int16_t* s, size_t frames, int64_t time_us) {
        if (first_time < 0) first_time = time_us;
        pcm.insert(pcm.end(), s, s + frames * 2);
        return true;
    }));
    EXPECT_EQ(first_time, 0);
    // The pre-skip is gone: the sound is as long as the mix, plus at most the
    // padding of the last packet.
    EXPECT_GE(pcm.size() / 2, 96000u);
    EXPECT_LT(pcm.size() / 2, 96000u + 960u);
    // The tone starts at 0.5 s, within 2 ms.
    size_t onset = 0;
    for (size_t i = 0; i < pcm.size() / 2; ++i) {
        if (std::abs(pcm[2 * i]) > 3000) {
            onset = i;
            break;
        }
    }
    EXPECT_NEAR(static_cast<double>(onset) / 48.0, 500.0, 2.0) << "the onset in ms";
    // 440 Hz on the left, 660 Hz on the right.
    const double l440 = goertzel(pcm, 0, 440, 48000, 24000), l660 = goertzel(pcm, 0, 660, 48000, 24000);
    const double r440 = goertzel(pcm, 1, 440, 48000, 24000), r660 = goertzel(pcm, 1, 660, 48000, 24000);
    EXPECT_GT(l440, 20 * l660);
    EXPECT_GT(r660, 20 * r440);
    EXPECT_NEAR(l440, 0.183, 0.03) << "the level is kept (12000 / 32768 / 2)";
}

TEST(Mp4Demux, ReadsAFileFromAnotherWriter) {
    // ffmpeg 8 (mov muxer, libopus): the H.264 fixture and 2 s of two tones.
    const fs::path mp4 = fs::path(MELLO_REPLAY_FIXTURE_DIR) / "ffmpeg_opus_160x96.mp4";
    Mp4Movie movie;
    std::string error;
    ASSERT_TRUE(open_file(mp4, movie, &error)) << error;
    ASSERT_NE(movie.video(), nullptr);
    ASSERT_NE(movie.audio(), nullptr);
    EXPECT_EQ(movie.video()->samples.size(), 60u);
    EXPECT_EQ(movie.audio()->pre_skip, 312);
    EXPECT_EQ(movie.audio()->edit_media_time, 312);

    std::vector<int16_t> pcm;
    ASSERT_TRUE(read_mp4_audio(mp4, [&](const int16_t* s, size_t frames, int64_t) {
        pcm.insert(pcm.end(), s, s + frames * 2);
        return true;
    }));
    EXPECT_GE(pcm.size() / 2, 96000u);
    EXPECT_GT(goertzel(pcm, 0, 440, 24000, 24000), 20 * goertzel(pcm, 0, 660, 24000, 24000));
    EXPECT_GT(goertzel(pcm, 1, 660, 24000, 24000), 20 * goertzel(pcm, 1, 440, 24000, 24000));
    size_t keys = 0;
    ASSERT_TRUE(read_mp4_video(
        mp4, [&](const Mp4VideoUnit& u) { keys += u.keyframe; return true; }, nullptr, nullptr));
    EXPECT_EQ(keys, 2u);
}

TEST(Mp4Demux, ACutFileDoesNotOpen) {
    TempDir dir;
    Clip clip = make_clip(two_tones());
    const fs::path mp4 = dir.path() / "clip.mp4";
    ASSERT_TRUE(write(clip, mp4));
    const auto file = read_file(mp4);
    // Cut in the middle of the moov box, and in the middle of the mdat box.
    size_t moov_at = 0, moov_size = 0;
    ASSERT_TRUE(find_box(file, "moov", moov_at, moov_size));
    for (size_t cut : {size_t{20}, moov_at + moov_size / 2, moov_at + moov_size + 100}) {
        const fs::path part = dir.path() / ("cut_" + std::to_string(cut) + ".mp4");
        write_file(part, std::vector<uint8_t>(file.begin(), file.begin() + static_cast<std::ptrdiff_t>(cut)));
        Mp4Movie movie;
        std::string error;
        EXPECT_FALSE(open_file(part, movie, &error)) << "a file cut at " << cut;
        EXPECT_FALSE(error.empty());
    }
}

TEST(Mp4Demux, TablesThatDoNotFitAreRefused) {
    TempDir dir;
    Clip clip = make_clip(two_tones());
    const fs::path mp4 = dir.path() / "clip.mp4";
    ASSERT_TRUE(write(clip, mp4));
    const auto file = read_file(mp4);

    // A sample count of 2^31 in stsz.
    {
        auto bad = file;
        size_t at = 0, size = 0;
        ASSERT_TRUE(find_box(bad, "stsz", at, size));
        bad[at + 16] = 0x80;
        const fs::path p = dir.path() / "big_stsz.mp4";
        write_file(p, bad);
        Mp4Movie movie;
        EXPECT_FALSE(open_file(p, movie));
    }
    // A chunk offset past the end of the file.
    {
        auto bad = file;
        size_t at = 0, size = 0;
        ASSERT_TRUE(find_box(bad, "stco", at, size));
        bad[at + 16] = 0x7F;
        const fs::path p = dir.path() / "far_chunk.mp4";
        write_file(p, bad);
        Mp4Movie movie;
        std::string error;
        EXPECT_FALSE(open_file(p, movie, &error));
        EXPECT_NE(error.find("outside the file"), std::string::npos) << error;
    }
}

TEST(Mp4Demux, DamagedMoovBytesNeverCrash) {
    // Not a proof, but each run flips bytes in the moov of a real clip, and
    // the demux must either refuse the file or give samples inside it.
    TempDir dir;
    Clip clip = make_clip(two_tones());
    const fs::path mp4 = dir.path() / "clip.mp4";
    ASSERT_TRUE(write(clip, mp4));
    const auto file = read_file(mp4);
    size_t moov_at = 0, moov_size = 0;
    ASSERT_TRUE(find_box(file, "moov", moov_at, moov_size));
    std::mt19937 rng(7);
    for (int run = 0; run < 300; ++run) {
        std::vector<uint8_t> moov(file.begin() + static_cast<std::ptrdiff_t>(moov_at + 8),
                                  file.begin() + static_cast<std::ptrdiff_t>(moov_at + moov_size));
        for (int k = 0; k < 4; ++k) moov[rng() % moov.size()] = static_cast<uint8_t>(rng());
        Mp4Movie movie;
        if (!parse_moov(moov.data(), moov.size(), file.size(), movie, nullptr)) continue;
        for (const auto& t : movie.tracks) {
            for (const auto& s : t.samples) ASSERT_LE(s.offset + s.size, file.size()) << "run " << run;
        }
    }
}

TEST(Mp4Demux, AvccToAnnexBChecksEachLength) {
    const std::vector<uint8_t> good = {0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 1, 0x06};
    std::vector<uint8_t> out;
    ASSERT_TRUE(avcc_to_annex_b(good.data(), good.size(), 4, out));
    EXPECT_EQ(out, (std::vector<uint8_t>{0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x06}));
    const std::vector<uint8_t> long_length = {0, 0, 0, 9, 0x65, 0x88};
    EXPECT_FALSE(avcc_to_annex_b(long_length.data(), long_length.size(), 4, out));
    const std::vector<uint8_t> short_header = {0, 0};
    EXPECT_FALSE(avcc_to_annex_b(short_header.data(), short_header.size(), 4, out));
}

// A clip from this writer for a check by hand in other players (Chromium,
// AVPlayer, ffmpeg). Run on request only:
//   MELLO_MP4_IN=<annex-b .h264 with an AUD before each unit> MELLO_MP4_FPS=60
//   MELLO_MP4_W=1280 MELLO_MP4_H=720 MELLO_MP4_OUT=<file.mp4>
//   mello_tests --gtest_also_run_disabled_tests --gtest_filter=Mp4Write.DISABLED_WriteAClipForOtherPlayers
TEST(Mp4Write, DISABLED_WriteAClipForOtherPlayers) {
    const char* in_path = std::getenv("MELLO_MP4_IN");
    const char* out_path = std::getenv("MELLO_MP4_OUT");
    ASSERT_TRUE(in_path && out_path) << "set MELLO_MP4_IN and MELLO_MP4_OUT";
    const uint32_t fps = std::getenv("MELLO_MP4_FPS") ? static_cast<uint32_t>(std::atoi(std::getenv("MELLO_MP4_FPS"))) : 60;
    const auto all = read_file(in_path);
    Clip c;
    h264::for_each_nal(all.data(), all.size(), [&](uint8_t type, const uint8_t* nal, size_t size) {
        if (type == h264::kNalAud || c.units.empty()) c.units.emplace_back();
        static const uint8_t kStart[] = {0, 0, 0, 1};
        c.units.back().insert(c.units.back().end(), kStart, kStart + 4);
        c.units.back().insert(c.units.back().end(), nal, nal + size);
    });
    const uint64_t frame_us = 1'000'000 / fps;
    for (size_t i = 0; i < c.units.size(); ++i) {
        AccessUnitRef r;
        r.offset = i;
        r.size = static_cast<uint32_t>(c.units[i].size());
        r.capture_us = kT0 + i * frame_us;
        r.keyframe = h264::has_idr(c.units[i].data(), c.units[i].size());
        c.refs.push_back(r);
    }
    c.in.units = c.refs;
    c.in.width = static_cast<uint32_t>(std::atoi(std::getenv("MELLO_MP4_W") ? std::getenv("MELLO_MP4_W") : "1280"));
    c.in.height = static_cast<uint32_t>(std::atoi(std::getenv("MELLO_MP4_H") ? std::getenv("MELLO_MP4_H") : "720"));
    c.in.fps = fps;
    c.in.end_us = kT0 + c.units.size() * frame_us;
    // Bursts of 440 Hz left and 660 Hz right, 0.2 s at the start of each second.
    const size_t frames = static_cast<size_t>((c.in.end_us - kT0) * 48 / 1000);
    c.in.audio.assign(frames * 2, 0);
    for (size_t i = 0; i < frames; ++i) {
        if (i % 48000 >= 9600) continue;
        const double t = static_cast<double>(i) / 48000.0;
        c.in.audio[2 * i] = static_cast<int16_t>(13000 * std::sin(2 * kPi * 440 * t));
        c.in.audio[2 * i + 1] = static_cast<int16_t>(13000 * std::sin(2 * kPi * 660 * t));
    }
    ClipMuxResult result;
    ASSERT_TRUE(write(c, out_path, &result));
    std::printf("[ Mp4Write ] %s: %u frames, %.2f s, %llu bytes\n", out_path, result.video_frames,
                static_cast<double>(result.duration_us) / 1e6, static_cast<unsigned long long>(result.bytes));
}
