#pragma once

// The game clip MP4 read back (CLIPS.md 15.9): m3llo's own demux, the same on
// every platform. It reads the moov box and gives each track's samples with
// their file offsets, so a player reads exactly the bytes it needs. Codecs:
// H.264 (`avc1`, `avc3`) and Opus (`Opus` with `dOps`). A clip from another
// app can carry other codecs: the track is there, with `supported` false.
//
// The file comes from other users, so every field is checked against the
// box and the file size before it is used.

#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <functional>
#include <string>
#include <vector>

namespace mello::replay {

/// Reads `len` bytes at `offset` into `buf`. False on an error, a short
/// file, or a stop. A reader that waits for bytes (the range reader) waits
/// inside this call.
using Mp4ReadAt = std::function<bool(uint64_t offset, uint8_t* buf, size_t len)>;

struct Mp4Sample {
    uint64_t offset = 0;  ///< in the file
    uint32_t size = 0;
    int64_t time = 0;     ///< presentation time, in the track's timescale, before the edit
    uint32_t duration = 0;
    bool sync = false;
};

struct Mp4Track {
    enum class Kind { Video, Audio, Other };
    Kind kind = Kind::Other;
    uint32_t track_id = 0;
    uint32_t codec = 0;  ///< the sample entry's fourcc
    bool supported = false;
    uint32_t timescale = 0;
    /// The edit list's media time: the first heard (or seen) sample time.
    /// Samples before it decode but do not play (the Opus pre-skip).
    int64_t edit_media_time = 0;
    std::vector<Mp4Sample> samples;  ///< in decode order

    // Video.
    uint32_t width = 0;
    uint32_t height = 0;
    uint8_t nal_length_size = 4;
    std::vector<std::vector<uint8_t>> sps;
    std::vector<std::vector<uint8_t>> pps;

    // Audio.
    uint32_t channels = 0;
    uint32_t sample_rate = 0;
    uint16_t pre_skip = 0;
    /// The `roll` group's distance in samples (negative: frames before a
    /// seek target that the decoder needs). 0: none.
    int32_t roll_distance = 0;

    /// `time` from the start of play, in microseconds.
    int64_t time_us(int64_t time) const {
        return timescale ? (time - edit_media_time) * 1'000'000 / static_cast<int64_t>(timescale) : 0;
    }
};

struct Mp4Movie {
    uint64_t file_size = 0;
    uint64_t duration_us = 0;
    std::vector<Mp4Track> tracks;

    const Mp4Track* video() const;
    const Mp4Track* audio() const;
};

/// Finds the moov box from the top-level box headers and parses it. A moov
/// after the mdat box works too (two reads more). False with `error` set when
/// the file is not an MP4 this demux understands.
bool open_mp4(const Mp4ReadAt& read, uint64_t file_size, Mp4Movie& movie, std::string* error);

/// Parses the body of a moov box. Exposed for the tests.
bool parse_moov(const uint8_t* body, size_t size, uint64_t file_size, Mp4Movie& movie, std::string* error);

/// A reader over a file on disk.
Mp4ReadAt file_reader(const std::filesystem::path& path, uint64_t* file_size);

/// The bytes of one sample.
bool read_sample(const Mp4ReadAt& read, const Mp4Sample& sample, std::vector<uint8_t>& out);

/// An H.264 sample with NAL lengths, as Annex B (4-byte start codes).
/// False when a length does not fit the sample.
bool avcc_to_annex_b(const uint8_t* data, size_t size, uint8_t length_size, std::vector<uint8_t>& out);

}  // namespace mello::replay
