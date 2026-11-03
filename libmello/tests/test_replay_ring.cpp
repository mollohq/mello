#include <gtest/gtest.h>
#include "replay/replay_audio_ring.hpp"
#include "replay/replay_video_ring.hpp"
#include <chrono>
#include <cstdint>
#include <filesystem>
#include <string>
#include <vector>

using namespace mello::replay;
namespace fs = std::filesystem;

namespace {

// A fresh, empty folder for one test, removed after it.
class TempDir {
public:
    TempDir() {
        const auto stamp = std::chrono::steady_clock::now().time_since_epoch().count();
        path_ = fs::temp_directory_path() / ("mello_replay_ring_test_" + std::to_string(stamp));
        fs::create_directories(path_);
    }
    ~TempDir() {
        std::error_code ec;
        fs::remove_all(path_, ec);
    }
    const fs::path& path() const { return path_; }
    size_t entries() const {
        size_t n = 0;
        for ([[maybe_unused]] const auto& e : fs::directory_iterator(path_)) ++n;
        return n;
    }

private:
    fs::path path_;
};

// The folder entries of an open ring file (util/ring_file.hpp). Windows keeps
// the name until the last handle closes (delete on close). POSIX unlinks the
// file at open, so the folder is empty from the start.
#ifdef _WIN32
constexpr size_t kOpenRingEntries = 1;
#else
constexpr size_t kOpenRingEntries = 0;
#endif

// A unit whose bytes name its GOP and its place in the GOP.
std::vector<uint8_t> unit_bytes(uint32_t gop, uint32_t index, size_t size) {
    std::vector<uint8_t> data(size);
    for (size_t i = 0; i < size; ++i) {
        data[i] = static_cast<uint8_t>((gop * 31 + index * 7 + i) & 0xFF);
    }
    return data;
}

constexpr uint64_t kFrameUs = 10'000;

// Pushes one GOP of `units` units (a keyframe, then P-frames), 100 bytes
// each, one frame time apart from `first_us`.
void push_gop(ReplayVideoRing& ring, uint32_t gop, uint32_t units, uint64_t first_us) {
    for (uint32_t i = 0; i < units; ++i) {
        ring.push(unit_bytes(gop, i, 100), first_us + i * kFrameUs, i == 0);
    }
}

uint64_t gop_start(uint32_t gop) { return 1'000'000 + gop * 4 * kFrameUs; }

// No writer thread: only flush() writes, so each test decides when the queue
// drains.
ReplayVideoRing::Config video_config(const TempDir& dir, uint32_t segments) {
    ReplayVideoRing::Config c;
    c.dir = dir.path();
    c.segments = segments;
    c.segment_bytes = 1000;
    c.writer_thread = false;
    return c;
}

} // namespace

TEST(ReplayVideoRing, AWindowStartsAtTheKeyframeBeforeItsStart) {
    TempDir dir;
    ReplayVideoRing ring;
    ASSERT_TRUE(ring.start(video_config(dir, 8)));
    for (uint32_t g = 0; g < 4; ++g) push_gop(ring, g, 4, gop_start(g));
    ring.flush();

    // The start falls on the third unit of GOP 1.
    auto window = ring.pin_window(gop_start(1) + 2 * kFrameUs, gop_start(3) + 3 * kFrameUs);
    ASSERT_EQ(window.units().size(), 12u);
    EXPECT_TRUE(window.units().front().keyframe);
    EXPECT_EQ(window.units().front().capture_us, gop_start(1));
    EXPECT_EQ(window.units().back().capture_us, gop_start(3) + 3 * kFrameUs);

    std::vector<uint8_t> bytes;
    for (size_t i = 0; i < window.units().size(); ++i) {
        ASSERT_TRUE(window.read(window.units()[i], bytes));
        EXPECT_EQ(bytes, unit_bytes(1 + static_cast<uint32_t>(i / 4), static_cast<uint32_t>(i % 4), 100))
            << "unit " << i;
    }
}

TEST(ReplayVideoRing, TheWindowEndsAtItsEndTime) {
    TempDir dir;
    ReplayVideoRing ring;
    ASSERT_TRUE(ring.start(video_config(dir, 8)));
    for (uint32_t g = 0; g < 3; ++g) push_gop(ring, g, 4, gop_start(g));
    ring.flush();

    auto window = ring.pin_window(gop_start(0), gop_start(1) + kFrameUs);
    ASSERT_EQ(window.units().size(), 6u);
    EXPECT_EQ(window.units().back().capture_us, gop_start(1) + kFrameUs);
}

TEST(ReplayVideoRing, TheRingRotatesAndKeepsTheNewestSegments) {
    TempDir dir;
    ReplayVideoRing ring;
    ASSERT_TRUE(ring.start(video_config(dir, 4)));
    for (uint32_t g = 0; g < 7; ++g) push_gop(ring, g, 4, gop_start(g));
    ring.flush();

    // The window asks for everything; the ring has the last 4 GOPs.
    auto window = ring.pin_window(0, UINT64_MAX);
    ASSERT_EQ(window.units().size(), 16u);
    EXPECT_EQ(window.units().front().capture_us, gop_start(3));
    std::vector<uint8_t> bytes;
    ASSERT_TRUE(window.read(window.units().front(), bytes));
    EXPECT_EQ(bytes, unit_bytes(3, 0, 100));
    EXPECT_EQ(ring.stats().segments_written, 7u);
}

TEST(ReplayVideoRing, APinnedSegmentIsNeverOverwritten) {
    TempDir dir;
    ReplayVideoRing ring;
    ASSERT_TRUE(ring.start(video_config(dir, 4)));
    for (uint32_t g = 0; g < 4; ++g) push_gop(ring, g, 4, gop_start(g));
    ring.flush();

    // Pin GOP 0, the oldest: the next GOP would take its slot.
    auto window = ring.pin_window(gop_start(0), gop_start(0) + 3 * kFrameUs);
    ASSERT_EQ(window.units().size(), 4u);
    for (uint32_t g = 4; g < 10; ++g) push_gop(ring, g, 4, gop_start(g));
    ring.flush();

    std::vector<uint8_t> bytes;
    for (uint32_t i = 0; i < 4; ++i) {
        ASSERT_TRUE(window.read(window.units()[i], bytes));
        EXPECT_EQ(bytes, unit_bytes(0, i, 100)) << "unit " << i;
    }
    EXPECT_GT(ring.stats().pinned_skips, 0u);

    // Released, the slot goes back to the writer.
    window.release();
    for (uint32_t g = 10; g < 14; ++g) push_gop(ring, g, 4, gop_start(g));
    ring.flush();
    auto all = ring.pin_window(0, UINT64_MAX);
    ASSERT_EQ(all.units().size(), 16u);
    EXPECT_EQ(all.units().front().capture_us, gop_start(10));
}

TEST(ReplayVideoRing, AFullQueueDropsUpToTheNextKeyframeAndAsksForOne) {
    TempDir dir;
    ReplayVideoRing ring;
    auto config = video_config(dir, 8);
    config.queue_bytes = 250;  // two and a half units
    ASSERT_TRUE(ring.start(config));

    // The disk stalls: nothing drains while GOP 0 arrives. Unit 2 does not
    // fit, and unit 3 has no reference without it.
    for (uint32_t i = 0; i < 4; ++i) {
        ring.push(unit_bytes(0, i, 100), gop_start(0) + i * kFrameUs, i == 0);
    }
    EXPECT_TRUE(ring.take_keyframe_request());
    EXPECT_FALSE(ring.take_keyframe_request()) << "the request is taken once";

    // The disk is back. A P-frame still waits for the keyframe.
    ring.flush();
    ring.push(unit_bytes(0, 4, 100), gop_start(0) + 4 * kFrameUs, false);
    push_gop(ring, 1, 2, gop_start(1));
    ring.flush();

    auto window = ring.pin_window(0, UINT64_MAX);
    ASSERT_EQ(window.units().size(), 4u);
    EXPECT_TRUE(window.units()[0].keyframe);
    EXPECT_EQ(window.units()[1].capture_us, gop_start(0) + kFrameUs);
    EXPECT_TRUE(window.units()[2].keyframe);
    EXPECT_EQ(window.units()[2].capture_us, gop_start(1));
    EXPECT_EQ(ring.stats().queue_drops, 3u);
}

TEST(ReplayVideoRing, AGopLargerThanItsSlotEndsTheSegmentAndAsksForAKeyframe) {
    TempDir dir;
    ReplayVideoRing ring;
    auto config = video_config(dir, 4);
    config.segment_bytes = 250;
    ASSERT_TRUE(ring.start(config));

    push_gop(ring, 0, 4, gop_start(0));  // 400 bytes into a 250 byte slot
    push_gop(ring, 1, 2, gop_start(1));
    ring.flush();

    EXPECT_TRUE(ring.take_keyframe_request());
    EXPECT_EQ(ring.stats().overflow_drops, 1u) << "counted where the gap began";
    auto window = ring.pin_window(0, UINT64_MAX);
    ASSERT_EQ(window.units().size(), 4u);
    EXPECT_EQ(window.units()[1].capture_us, gop_start(0) + kFrameUs);
    EXPECT_TRUE(window.units()[2].keyframe);
    EXPECT_EQ(window.units()[2].capture_us, gop_start(1));
}

TEST(ReplayVideoRing, PFramesBeforeTheFirstKeyframeAreNotWritten) {
    TempDir dir;
    ReplayVideoRing ring;
    ASSERT_TRUE(ring.start(video_config(dir, 4)));
    ring.push(unit_bytes(9, 1, 100), gop_start(0), false);
    push_gop(ring, 1, 2, gop_start(1));
    ring.flush();

    auto window = ring.pin_window(0, UINT64_MAX);
    ASSERT_EQ(window.units().size(), 2u);
    EXPECT_TRUE(window.units().front().keyframe);
}

TEST(ReplayVideoRing, TheFileIsGoneAfterStopAndAWindowKeepsItUntilReleased) {
    TempDir dir;
    ReplayVideoRing ring;
    ASSERT_TRUE(ring.start(video_config(dir, 4)));
    EXPECT_EQ(dir.entries(), kOpenRingEntries);
    push_gop(ring, 0, 4, gop_start(0));
    ring.flush();
    auto window = ring.pin_window(0, UINT64_MAX);
    ring.stop();

    // A mux that runs past the stop still reads its units.
    std::vector<uint8_t> bytes;
    ASSERT_TRUE(window.read(window.units().front(), bytes));
    EXPECT_EQ(bytes, unit_bytes(0, 0, 100));
    window.release();
    EXPECT_EQ(dir.entries(), 0u);
}

TEST(ReplayVideoRing, TheSlotHoldsTwoSecondsOfThePeakRate) {
    // 5 Mbps: the peak is 6.25 Mbps, 781250 bytes a second.
    EXPECT_EQ(ReplayVideoRing::segment_bytes_for(5'000), 1'562'500u);
}

// --- Game audio ---

namespace {

constexpr uint32_t kRate = ReplayAudioRing::kRate;

// Stereo frames whose left sample is the frame number (mod 32768) and whose
// right sample is its negative.
std::vector<int16_t> ramp(uint64_t first_frame, size_t frames) {
    std::vector<int16_t> s(frames * 2);
    for (size_t i = 0; i < frames; ++i) {
        const auto v = static_cast<int16_t>((first_frame + i) % 32768);
        s[2 * i] = v;
        s[2 * i + 1] = static_cast<int16_t>(-v);
    }
    return s;
}

uint64_t us_of(uint64_t frames) { return frames * 1'000'000 / kRate; }

ReplayAudioRing::Config audio_config(const TempDir& dir, uint32_t seconds) {
    ReplayAudioRing::Config c;
    c.dir = dir.path();
    c.seconds = seconds;
    c.writer_thread = false;
    return c;
}

} // namespace

TEST(ReplayAudioRing, ChunksKeepTheCaptureTimeOfTheirFirstFrame) {
    TempDir dir;
    ReplayAudioRing ring;
    ASSERT_TRUE(ring.start(audio_config(dir, 4)));
    const uint64_t t0 = 5'000'000;
    // 25 ms packets, as a loopback device can deliver them.
    for (uint64_t f = 0; f < 1200 * 8; f += 1200) {
        const auto pcm = ramp(f, 1200);
        ring.push(pcm.data(), 1200, t0 + us_of(f));
    }
    ring.flush();

    auto window = ring.window(t0, t0 + 1'000'000);
    ASSERT_EQ(window.chunks().size(), 20u);
    std::vector<int16_t> out(ReplayAudioRing::kChunkFrames * 2);
    for (size_t i = 0; i < window.chunks().size(); ++i) {
        const auto& c = window.chunks()[i];
        EXPECT_EQ(c.frames, ReplayAudioRing::kChunkFrames);
        EXPECT_EQ(c.capture_us, t0 + i * 10'000) << "chunk " << i;
        ASSERT_TRUE(window.read(c, out.data()));
        EXPECT_EQ(out, ramp(i * ReplayAudioRing::kChunkFrames, ReplayAudioRing::kChunkFrames))
            << "chunk " << i;
    }
}

TEST(ReplayAudioRing, AGapStartsANewChunkAtItsOwnTime) {
    TempDir dir;
    ReplayAudioRing ring;
    ASSERT_TRUE(ring.start(audio_config(dir, 4)));
    const uint64_t t0 = 5'000'000;
    const auto a = ramp(0, 720);  // 15 ms
    ring.push(a.data(), 720, t0);
    const auto b = ramp(720, 480);  // 10 ms, 100 ms later: the game was quiet
    ring.push(b.data(), 480, t0 + 100'000);
    ring.flush();

    auto window = ring.window(0, UINT64_MAX);
    ASSERT_EQ(window.chunks().size(), 3u);
    EXPECT_EQ(window.chunks()[0].capture_us, t0);
    EXPECT_EQ(window.chunks()[0].frames, 480u);
    EXPECT_EQ(window.chunks()[1].capture_us, t0 + 10'000);
    EXPECT_EQ(window.chunks()[1].frames, 240u) << "the part before the gap";
    EXPECT_EQ(window.chunks()[2].capture_us, t0 + 100'000);
    EXPECT_EQ(window.chunks()[2].frames, 480u);
}

TEST(ReplayAudioRing, ArrivalJitterDoesNotSplitAChunk) {
    TempDir dir;
    ReplayAudioRing ring;
    ASSERT_TRUE(ring.start(audio_config(dir, 4)));
    const int64_t t0 = 5'000'000;
    // 7.5 ms packets, so chunks span two packets, with arrival-time stamps
    // that wander by up to 4 ms.
    const int64_t jitter[] = {0, 3000, -2000, 4000, -1000, 0};
    for (int i = 0; i < 6; ++i) {
        const auto pcm = ramp(static_cast<uint64_t>(i) * 360, 360);
        ring.push(pcm.data(), 360, static_cast<uint64_t>(t0 + i * 7'500 + jitter[i]));
    }
    ring.flush();
    auto window = ring.window(0, UINT64_MAX);
    ASSERT_EQ(window.chunks().size(), 5u) << "4 whole chunks, then the flushed rest";
    for (size_t i = 0; i < 4; ++i) EXPECT_EQ(window.chunks()[i].frames, 480u);
    EXPECT_EQ(window.chunks()[4].frames, 240u);
}

TEST(ReplayAudioRing, AnOverwrittenChunkReadsAsMissing) {
    TempDir dir;
    ReplayAudioRing ring;
    ASSERT_TRUE(ring.start(audio_config(dir, 1)));  // 100 chunks
    const uint64_t t0 = 5'000'000;
    const auto first = ramp(0, 24'000);  // 0.5 s
    ring.push(first.data(), 24'000, t0);
    ring.flush();
    auto window = ring.window(t0, t0 + 500'000);
    ASSERT_EQ(window.chunks().size(), 50u);

    // One more second: the writer reuses every slot of the window.
    for (uint64_t f = 24'000; f < 72'000; f += 4800) {
        const auto pcm = ramp(f, 4800);
        ring.push(pcm.data(), 4800, t0 + us_of(f));
        ring.flush();
    }
    std::vector<int16_t> out(ReplayAudioRing::kChunkFrames * 2);
    EXPECT_FALSE(window.read(window.chunks().front(), out.data()));
}

TEST(ReplayAudioRing, AFullQueueDropsAndCounts) {
    TempDir dir;
    ReplayAudioRing ring;
    ASSERT_TRUE(ring.start(audio_config(dir, 4)));
    // 3 s at once, faster than any writer: the queue holds 2 s.
    const auto pcm = ramp(0, 3 * kRate);
    ring.push(pcm.data(), 3 * kRate, 5'000'000);
    ring.flush();
    EXPECT_EQ(ring.stats().frames_dropped, 3u * kRate);
}

TEST(ReplayAudioRing, TheFileIsGoneAfterStop) {
    TempDir dir;
    ReplayAudioRing ring;
    ASSERT_TRUE(ring.start(audio_config(dir, 1)));
    EXPECT_EQ(dir.entries(), kOpenRingEntries);
    ring.stop();
    EXPECT_EQ(dir.entries(), 0u);
}
