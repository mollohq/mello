#pragma once
#include <filesystem>

namespace mello::replay {

/// Copy the MP4 `src` to `dst` with the moov box before the mdat box, so a
/// player can start before the download ends (plans/GAME-CLIPS.md 5.4). The
/// chunk offsets (stco, co64) move by the size of moov. The media data is
/// copied in blocks, never read whole into RAM. False when `src` is not a
/// plain MP4 with one moov and one mdat, or an offset does not fit.
bool mp4_faststart(const std::filesystem::path& src, const std::filesystem::path& dst);

} // namespace mello::replay
