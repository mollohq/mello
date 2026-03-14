#pragma once
#include <string>
#include <vector>
#include <cstdint>

namespace mello::video {

struct GameProcess {
    uint32_t    pid;
    std::string name;
    std::string exe;
    bool        is_fullscreen;
};

/// Returns running processes that match the bundled game list (assets/games.json).
std::vector<GameProcess> enumerate_game_processes();

} // namespace mello::video
