// iOS host-source enumeration — Step 1 stub.
// iOS has no screen/window/process capture (no mobile hosting in v1), so these
// return empty. The C-ABI host functions in mello.cpp surface this as 0 sources.
#include "process_enum.hpp"

namespace mello::video {

std::vector<MonitorInfo> enumerate_monitors() { return {}; }
std::vector<GameProcess> enumerate_game_processes() { return {}; }
std::vector<VisibleWindow> enumerate_visible_windows() { return {}; }

} // namespace mello::video
