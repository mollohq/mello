#define _USE_MATH_DEFINES
#include <cmath>
#include <gtest/gtest.h>
#include "audio/clip_buffer.hpp"
#include <chrono>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <string>
#include <vector>

using namespace mello::audio;
namespace fs = std::filesystem;

namespace {

constexpr int kRate = 48000;
constexpr size_t kChunk = 480; // the render path writes 10 ms at a time

// A fresh, empty folder for one test, removed after it.
class TempDir {
public:
    TempDir() {
        const auto stamp = std::chrono::steady_clock::now().time_since_epoch().count();
        path_ = fs::temp_directory_path() / ("mello_clip_buffer_test_" + std::to_string(stamp));
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

// Writes `seconds` of a sine in render-sized chunks.
void write_tone(ClipBuffer& buffer, double hz, double seconds, double& phase) {
    const size_t total = static_cast<size_t>(seconds * kRate);
    std::vector<int16_t> chunk(kChunk);
    for (size_t done = 0; done < total; done += kChunk) {
        for (auto& s : chunk) {
            s = static_cast<int16_t>(12000.0 * std::sin(phase));
            phase += 2.0 * M_PI * hz / kRate;
        }
        buffer.write(chunk.data(), chunk.size());
    }
}

// Writes `seconds` of a sine one second at a time and flushes the queue after
// each second, as the writer thread keeps up with the real-time render
// thread. The RAM queue holds 2 s.
void write_tone_paced(ClipBuffer& buffer, double hz, int seconds, double& phase) {
    for (int i = 0; i < seconds; ++i) {
        write_tone(buffer, hz, 1.0, phase);
        buffer.flush();
    }
}

struct Wav {
    uint16_t channels = 0;
    uint32_t rate = 0;
    std::vector<int16_t> samples;
};

Wav read_wav(const fs::path& path) {
    Wav wav;
    std::ifstream f(path, std::ios::binary);
    std::vector<char> header(44);
    f.read(header.data(), 44);
    if (!f || std::memcmp(header.data(), "RIFF", 4) != 0) return wav;
    std::memcpy(&wav.channels, header.data() + 22, 2);
    std::memcpy(&wav.rate, header.data() + 24, 4);
    uint32_t bytes = 0;
    std::memcpy(&bytes, header.data() + 40, 4);
    wav.samples.resize(bytes / 2);
    f.read(reinterpret_cast<char*>(wav.samples.data()), bytes);
    return wav;
}

// The frequency of a tone from its zero crossings.
double tone_hz(const std::vector<int16_t>& s, size_t from, size_t count) {
    size_t crossings = 0;
    for (size_t i = from + 1; i < from + count; ++i) {
        if ((s[i - 1] < 0) != (s[i] < 0)) ++crossings;
    }
    return static_cast<double>(crossings) / 2.0 / (static_cast<double>(count) / kRate);
}

double rms(const std::vector<int16_t>& s, size_t from, size_t count) {
    double sum = 0.0;
    for (size_t i = from; i < from + count; ++i) sum += double(s[i]) * s[i];
    return std::sqrt(sum / static_cast<double>(count));
}

} // namespace

TEST(ClipBuffer, TheRingIsAFileOfUnderOneMegabyte) {
    // CLIPS.md 4.1: 60 seconds of voice is well under 1 MB on disk.
    ClipBuffer buffer(kRate, 60);
    EXPECT_EQ(buffer.ring_bytes(), 60u * 50u * ClipBuffer::kSlotBytes);
    EXPECT_LT(buffer.ring_bytes(), 1024u * 1024u);
}

TEST(ClipBuffer, CaptureSavesTheLastSecondsAsWav) {
    TempDir dir;
    ClipBuffer buffer(kRate, 60, dir.path().string());
    ASSERT_TRUE(buffer.start());

    double phase = 0.0;
    write_tone_paced(buffer, 300.0, 3, phase);
    write_tone_paced(buffer, 1000.0, 3, phase);

    const fs::path out = dir.path() / "clip.wav";
    ASSERT_TRUE(buffer.capture(2.0f, out.string()));
    buffer.stop();

    const Wav wav = read_wav(out);
    EXPECT_EQ(wav.channels, 1);
    EXPECT_EQ(wav.rate, static_cast<uint32_t>(kRate));
    ASSERT_EQ(wav.samples.size(), static_cast<size_t>(2 * kRate));
    // The window is the newest audio: the 1000 Hz tone, through Opus.
    EXPECT_NEAR(tone_hz(wav.samples, kRate / 2, kRate), 1000.0, 20.0);
    EXPECT_NEAR(rms(wav.samples, kRate / 2, kRate), 12000.0 / std::sqrt(2.0), 1500.0);
    EXPECT_EQ(buffer.dropped_samples(), 0u);
}

TEST(ClipBuffer, AFullQueueDropsAndCountsTheSamples) {
    TempDir dir;
    ClipBuffer buffer(kRate, 60, dir.path().string());
    ASSERT_TRUE(buffer.start());
    // 3 s at once, faster than real time: the 2 s queue takes what fits.
    double phase = 0.0;
    write_tone(buffer, 440.0, 3.0, phase);
    EXPECT_GE(buffer.dropped_samples(), static_cast<uint64_t>(kRate) / 2);
    EXPECT_LE(buffer.dropped_samples(), static_cast<uint64_t>(kRate));
}

TEST(ClipBuffer, CaptureTakesAudioTheWriterHasNotSeenYet) {
    TempDir dir;
    ClipBuffer buffer(kRate, 60, dir.path().string());
    ASSERT_TRUE(buffer.start());

    double phase = 0.0;
    write_tone(buffer, 440.0, 1.0, phase);
    // No wait for the writer thread: capture drains the RAM queue itself.
    const fs::path out = dir.path() / "clip.wav";
    ASSERT_TRUE(buffer.capture(1.0f, out.string()));
    EXPECT_EQ(read_wav(out).samples.size(), static_cast<size_t>(kRate));
}

TEST(ClipBuffer, TheRingKeepsOnlyItsLength) {
    TempDir dir;
    ClipBuffer buffer(kRate, 2, dir.path().string());
    ASSERT_TRUE(buffer.start());

    double phase = 0.0;
    write_tone_paced(buffer, 500.0, 5, phase);
    const fs::path out = dir.path() / "clip.wav";
    ASSERT_TRUE(buffer.capture(10.0f, out.string()));
    EXPECT_EQ(read_wav(out).samples.size(), static_cast<size_t>(2 * kRate));
    EXPECT_EQ(buffer.dropped_samples(), 0u);
}

TEST(ClipBuffer, StopLeavesNoFileInTheFolder) {
    TempDir dir;
    {
        ClipBuffer buffer(kRate, 60, dir.path().string());
        ASSERT_TRUE(buffer.start());
        double phase = 0.0;
        write_tone(buffer, 440.0, 1.0, phase);
        buffer.stop();
        EXPECT_EQ(dir.entries(), 0u);
        EXPECT_FALSE(buffer.is_active());
        EXPECT_FALSE(buffer.capture(1.0f, (dir.path() / "late.wav").string()));
    }
    EXPECT_EQ(dir.entries(), 0u);
}

TEST(ClipBuffer, AStartThatCannotMakeTheFolderStaysOff) {
    TempDir dir;
    // A regular file where the folder must go.
    const fs::path blocker = dir.path() / "blocker";
    std::ofstream(blocker) << "x";
    ClipBuffer buffer(kRate, 60, (blocker / "clips").string());
    EXPECT_FALSE(buffer.start());
    EXPECT_FALSE(buffer.is_active());

    std::vector<int16_t> chunk(kChunk, 1000);
    buffer.write(chunk.data(), chunk.size());
    EXPECT_FALSE(buffer.capture(1.0f, (dir.path() / "clip.wav").string()));
}

TEST(ClipBuffer, ASecondStartBeginsEmpty) {
    TempDir dir;
    ClipBuffer buffer(kRate, 60, dir.path().string());
    ASSERT_TRUE(buffer.start());
    double phase = 0.0;
    write_tone(buffer, 440.0, 2.0, phase);
    ASSERT_TRUE(buffer.start());
    EXPECT_TRUE(buffer.is_active());
    EXPECT_FALSE(buffer.capture(1.0f, (dir.path() / "clip.wav").string()));
}
