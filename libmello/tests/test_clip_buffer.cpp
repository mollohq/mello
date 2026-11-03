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

// The render thread's crew mix: a sine in render-sized chunks, each with the
// time it plays. The time moves on with the samples.
struct Crew {
    double phase = 0.0;
    uint64_t t_us = 10'000'000;
};

// Writes `seconds` of a sine in render-sized chunks.
void write_tone(ClipBuffer& buffer, double hz, double seconds, Crew& crew) {
    const size_t total = static_cast<size_t>(seconds * kRate);
    std::vector<int16_t> chunk(kChunk);
    for (size_t done = 0; done < total; done += kChunk) {
        for (auto& s : chunk) {
            s = static_cast<int16_t>(12000.0 * std::sin(crew.phase));
            crew.phase += 2.0 * M_PI * hz / kRate;
        }
        buffer.write_crew(chunk.data(), chunk.size(), crew.t_us);
        crew.t_us += kChunk * 1'000'000 / kRate;
    }
}

// Writes `seconds` of a sine one second at a time and flushes the queue after
// each second, as the writer thread keeps up with the real-time render
// thread. The RAM queue holds 2 s.
void write_tone_paced(ClipBuffer& buffer, double hz, int seconds, Crew& crew) {
    for (int i = 0; i < seconds; ++i) {
        write_tone(buffer, hz, 1.0, crew);
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

TEST(ClipBuffer, TheRingsAreFilesOfUnderTwoMegabytes) {
    // CLIPS.md 4.1: 60 seconds of the crew mix and of the mic, 768 KB each.
    ClipBuffer buffer(kRate, 60);
    EXPECT_EQ(buffer.ring_bytes(), 2u * 60u * 50u * ClipBuffer::kSlotBytes);
    EXPECT_LT(buffer.ring_bytes(), 2u * 1024u * 1024u);
}

TEST(ClipBuffer, CaptureSavesTheLastSecondsAsWav) {
    TempDir dir;
    ClipBuffer buffer(kRate, 60, dir.path().string());
    ASSERT_TRUE(buffer.start());

    Crew crew;
    write_tone_paced(buffer, 300.0, 3, crew);
    write_tone_paced(buffer, 1000.0, 3, crew);

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
    Crew crew;
    write_tone(buffer, 440.0, 3.0, crew);
    EXPECT_GE(buffer.dropped_samples(), static_cast<uint64_t>(kRate) / 2);
    EXPECT_LE(buffer.dropped_samples(), static_cast<uint64_t>(kRate));
}

TEST(ClipBuffer, CaptureTakesAudioTheWriterHasNotSeenYet) {
    TempDir dir;
    ClipBuffer buffer(kRate, 60, dir.path().string());
    ASSERT_TRUE(buffer.start());

    Crew crew;
    write_tone(buffer, 440.0, 1.0, crew);
    // No wait for the writer thread: capture drains the RAM queue itself.
    const fs::path out = dir.path() / "clip.wav";
    ASSERT_TRUE(buffer.capture(1.0f, out.string()));
    EXPECT_EQ(read_wav(out).samples.size(), static_cast<size_t>(kRate));
}

TEST(ClipBuffer, TheRingKeepsOnlyItsLength) {
    TempDir dir;
    ClipBuffer buffer(kRate, 2, dir.path().string());
    ASSERT_TRUE(buffer.start());

    Crew crew;
    write_tone_paced(buffer, 500.0, 5, crew);
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
        Crew crew;
        write_tone(buffer, 440.0, 1.0, crew);
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
    buffer.write_crew(chunk.data(), chunk.size(), 10'000'000);
    EXPECT_FALSE(buffer.capture(1.0f, (dir.path() / "clip.wav").string()));
}

TEST(ClipBuffer, ASecondStartBeginsEmpty) {
    TempDir dir;
    ClipBuffer buffer(kRate, 60, dir.path().string());
    ASSERT_TRUE(buffer.start());
    Crew crew;
    write_tone(buffer, 440.0, 2.0, crew);
    ASSERT_TRUE(buffer.start());
    EXPECT_TRUE(buffer.is_active());
    EXPECT_FALSE(buffer.capture(1.0f, (dir.path() / "clip.wav").string()));
}

namespace {

constexpr uint64_t kT0 = 10'000'000;

// 2 s of crew silence from kT0, in render-sized chunks.
void write_crew_silence(ClipBuffer& buffer, float volume_after_1s = 1.0f) {
    const std::vector<int16_t> silence(kChunk, 0);
    for (uint64_t k = 0; k < 200; ++k) {
        buffer.write_crew(silence.data(), kChunk, kT0 + k * 10'000, k < 100 ? 1.0f : volume_after_1s);
    }
}

// 2 s of mic in 20 ms frames from kT0: silence, and a 1 kHz tone in the
// frames for which `tone(frame)` is true.
template <typename Fn>
void write_mic(ClipBuffer& buffer, Fn tone) {
    std::vector<int16_t> frame(960);
    for (uint64_t f = 0; f < 100; ++f) {
        for (size_t i = 0; i < frame.size(); ++i) {
            frame[i] = tone(f, i) ? static_cast<int16_t>(16000.0 * std::sin(2.0 * M_PI * 1000.0 * i / kRate))
                                  : int16_t{0};
        }
        buffer.write_mic(frame.data(), frame.size(), kT0 + f * 20'000);
    }
}

} // namespace

// CLIPS.md 15.4: the mic keeps its capture time. It comes in on its own
// thread, after the crew audio of the same time, and a 2 ms burst in it lands
// at its capture time in the clip mix, the Opus lookahead taken out.
TEST(ClipBuffer, TheMicKeepsItsCaptureTime) {
    TempDir dir;
    ClipBuffer buffer(kRate, 60, dir.path().string());
    ASSERT_TRUE(buffer.start());
    write_crew_silence(buffer);
    buffer.flush();
    // The burst: the first 2 ms of the frame at 1.3 s.
    write_mic(buffer, [](uint64_t f, size_t i) { return f == 65 && i < 96; });
    buffer.flush();

    ClipBuffer::VoiceWindow window;
    ASSERT_TRUE(buffer.read_window(kT0, kT0 + 2'000'000, window));
    ASSERT_FALSE(window.crew.empty());
    ASSERT_FALSE(window.mic.empty());
    mello::replay::ClipAudioSource crew;
    crew.channels = 1;
    crew.chunks = window.crew;
    mello::replay::ClipAudioSource mic;
    mic.channels = 1;
    mic.chunks = window.mic;
    const auto out = mello::replay::mix_clip_audio(kT0, kT0 + 2'000'000, {crew, mic});

    size_t peak = 0;
    for (size_t i = 0; i < out.size() / 2; ++i) {
        if (std::abs(out[2 * i]) > std::abs(out[2 * peak])) peak = i;
    }
    const double expected = 1.3 * kRate + 48;  // the middle of the burst
    const double off_ms = (static_cast<double>(peak) - expected) / 48.0;
    EXPECT_LE(std::abs(off_ms), 2.0) << "the burst is " << off_ms << " ms off its capture time";
}

// CLIPS.md 15.4: the crew mix carries m3llo's mixer volume, and the window
// gives it as gain points.
TEST(ClipBuffer, TheCrewVolumeComesWithTheWindow) {
    TempDir dir;
    ClipBuffer buffer(kRate, 60, dir.path().string());
    ASSERT_TRUE(buffer.start());
    write_crew_silence(buffer, 0.5f);
    buffer.flush();

    ClipBuffer::VoiceWindow window;
    ASSERT_TRUE(buffer.read_window(kT0, kT0 + 2'000'000, window));
    ASSERT_EQ(window.crew_volume.size(), 2u);
    EXPECT_NEAR(window.crew_volume[0].second, 1.0f, 1e-4f);
    EXPECT_NEAR(window.crew_volume[1].second, 0.5f, 1e-4f);
    EXPECT_NEAR(static_cast<double>(window.crew_volume[1].first), kT0 + 1'000'000.0, 10'000.0)
        << "the step is at 1 s";
}

// F7: the WAV has the mic at its own time, mixed with the crew audio.
TEST(ClipBuffer, CaptureMixesTheMicAtItsOwnTime) {
    TempDir dir;
    ClipBuffer buffer(kRate, 60, dir.path().string());
    ASSERT_TRUE(buffer.start());
    write_crew_silence(buffer);
    // The tone from 0.5 s to 1.0 s.
    write_mic(buffer, [](uint64_t f, size_t) { return f >= 25 && f < 50; });

    const fs::path out = dir.path() / "clip.wav";
    ASSERT_TRUE(buffer.capture(2.0f, out.string()));
    const Wav wav = read_wav(out);
    ASSERT_EQ(wav.samples.size(), static_cast<size_t>(2 * kRate));
    EXPECT_GT(rms(wav.samples, kRate * 6 / 10, kRate / 4), 5000.0) << "the tone";
    EXPECT_LT(rms(wav.samples, kRate / 10, kRate * 3 / 10), 300.0) << "before the tone";
    EXPECT_LT(rms(wav.samples, kRate * 12 / 10, kRate * 6 / 10), 300.0) << "after the tone";
}
