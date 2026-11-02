// The game clip player is Windows only until the macOS work (plans/GAME-CLIPS.md
// 6.2, 9.2): AVURLAsset with a resource loader takes the place of Media
// Foundation there.
#ifndef _WIN32
#include "clip_player.hpp"

namespace mello::clip {

std::unique_ptr<ClipPlayer> ClipPlayer::start(ClipPlayerOptions) { return nullptr; }

}  // namespace mello::clip
#endif
