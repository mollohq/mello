#pragma once
#include <cstdint>
#include <utility>
#include <vector>

namespace mello::replay {

/// PCM with the capture time of its first frame, 48 kHz.
struct TimedPcm {
    uint64_t capture_us = 0;
    std::vector<int16_t> samples;  ///< interleaved, `channels` per frame
};

/// One source of the clip audio: the game audio, the crew voice or the mic.
struct ClipAudioSource {
    uint32_t channels = 2;  ///< 1: the same signal on both channels (center)
    float gain = 1.0f;
    /// A gain over time, as (time, gain) points. A point holds until the
    /// next one; before the first one, the first one holds. Empty: unity.
    /// The crew voice gets m3llo's mixer slider here (plan Q20).
    std::vector<std::pair<uint64_t, float>> gain_points;
    std::vector<TimedPcm> chunks;  ///< oldest first
};

constexpr uint32_t kClipAudioRate = 48000;

/// The clip audio (plans/GAME-CLIPS.md 5.3): 48 kHz stereo int16 from
/// `t0_us` to `end_us`.
///
/// Each source is placed at its own times. Its chunks are cut into runs with
/// no gap, and each run gets a line fitted to its chunk times. The line
/// removes the drift of the source clock (300 ppm is 9 ms over 30 s) and the
/// jitter of arrival-time stamps, so neither moves nor cracks the audio. A gap
/// is silence at its place in time. The sum goes through a limiter at -1 dBFS
/// (plan Q9: unity gain with a limiter).
std::vector<int16_t> mix_clip_audio(uint64_t t0_us, uint64_t end_us,
                                    const std::vector<ClipAudioSource>& sources);

namespace detail {

/// A run of chunks with no gap, and the line t(n) = start_us + n * us_per_frame
/// for its frame n.
struct AudioRun {
    size_t first_chunk = 0;
    size_t chunk_count = 0;
    uint64_t frames = 0;
    double start_us = 0;
    double us_per_frame = 0;
};

/// A chunk this much off the line of the run before it starts a new run.
constexpr double kRunGapUs = 20'000;
/// A fitted rate this far from 48 kHz is not drift: the run keeps 48 kHz.
constexpr double kMaxDriftPpm = 1000;

std::vector<AudioRun> fit_runs(const std::vector<TimedPcm>& chunks, uint32_t channels);

/// Peak limiter, in place, on interleaved stereo floats at full scale 1.0:
/// 5 ms of lookahead, 100 ms release, ceiling -1 dBFS.
void limit(std::vector<float>& stereo);

} // namespace detail

} // namespace mello::replay
