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

struct VisibleWindow {
    void*       hwnd;
    std::string title;
    uint32_t    pid;
};

/// Returns all visible top-level windows suitable for capture.
std::vector<VisibleWindow> enumerate_visible_windows();

} // namespace mello::video
