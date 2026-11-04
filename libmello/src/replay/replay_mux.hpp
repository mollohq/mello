#pragma once
#include "replay_video_ring.hpp"
#include <cstdint>
#include <filesystem>
#include <functional>
#include <vector>

namespace mello::replay {

/// What the mux needs for one game clip (CLIPS.md 15.4 and 15.5).
struct ClipMuxInput {
    /// Access units in decode order. The first one is an IDR with SPS and
    /// PPS. The clip's time zero is the capture time of the first one.
    std::vector<AccessUnitRef> units;
    /// Reads the bytes of one unit (a ring window's `read`).
    std::function<bool(const AccessUnitRef&, std::vector<uint8_t>&)> read_unit;
    uint32_t width = 0;
    uint32_t height = 0;
    uint32_t fps = 60;  ///< the nominal rate, for the track header only
    /// The end of the clip. The last frame lasts until then.
    uint64_t end_us = 0;
    /// 48 kHz stereo int16 from the first unit's capture time (mix_clip_audio).
    /// The mux encodes it to Opus. Empty: no audio track.
    std::vector<int16_t> audio;
    uint32_t audio_bitrate = 160'000;
};

struct ClipMuxResult {
    uint64_t duration_us = 0;
    uint32_t video_frames = 0;
    uint64_t bytes = 0;
};

/// Write the clip as an MP4 with the moov box first: the H.264 units as they
/// are (no decode, no encode), each at its own capture time, and the audio as
/// Opus. Runs on the caller's thread; it takes about a second, never the
/// command loop. False on any failure; the log says why. m3llo's own writer
/// (mp4_write.cpp), the same on every platform.
bool write_clip_mp4(const ClipMuxInput& input, const std::filesystem::path& path,
                    ClipMuxResult* result = nullptr);

/// Write a JPEG of a top-down BGRA image (`stride` bytes a row). Windows: WIC.
bool write_poster_jpeg(const uint8_t* bgra, uint32_t width, uint32_t height, uint32_t stride,
                       const std::filesystem::path& path);

/// Write a JPEG of the frame of `mp4` at or after `time_us` from the start of
/// the clip, decoded once. The poster when no frame came after the press: a
/// minimized game (CLIPS.md 15.4).
bool write_poster_from_mp4(const std::filesystem::path& mp4, uint64_t time_us,
                           const std::filesystem::path& jpeg);

} // namespace mello::replay
