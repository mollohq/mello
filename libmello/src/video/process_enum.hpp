#pragma once
#include <string>
#include <vector>
#include <cstdint>

namespace mello::video {

struct MonitorInfo {
    uint32_t    index;
    std::string name;
    uint32_t    width;
    uint32_t    height;
    bool        primary;
};

/// Returns connected displays via DXGI enumeration.
std::vector<MonitorInfo> enumerate_monitors();

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
    std::string exe;
    uint32_t    pid;
};

/// Returns all visible top-level windows suitable for capture.
std::vector<VisibleWindow> enumerate_visible_windows();

} // namespace mello::video
