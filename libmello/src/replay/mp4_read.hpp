#pragma once

// A game clip MP4 read back, for the stream gate's replay lane
// (CLIPS.md 15.13): the H.264 units as a decoder takes them, and
// the sound as PCM. Windows: Media Foundation. False elsewhere.

#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <functional>
#include <vector>

namespace mello::replay {

/// One access unit in Annex B, with its time from the start of the clip.
struct Mp4VideoUnit {
    std::vector<uint8_t> annex_b;
    int64_t time_us = 0;
    bool keyframe = false;
};

/// Calls `on_unit` with each video unit of `mp4` in decode order, the
/// track's parameter sets before the first key frame. `on_unit` returns
/// false to stop. False when the file does not open or has no video.
bool read_mp4_video(const std::filesystem::path& mp4,
                    const std::function<bool(const Mp4VideoUnit&)>& on_unit, uint32_t* width,
                    uint32_t* height);

/// Calls `on_pcm` with the sound of `mp4` decoded to 48 kHz stereo int16,
/// each chunk with the time of its first frame. False when the file does not
/// open or has no sound.
bool read_mp4_audio(const std::filesystem::path& mp4,
                    const std::function<bool(const int16_t* stereo, size_t frames, int64_t time_us)>& on_pcm);

}  // namespace mello::replay
