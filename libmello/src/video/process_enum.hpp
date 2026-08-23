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
    std::string exe;           // executable filename only (matching key)
    bool        is_fullscreen; // main window covers its monitor (borderless or exclusive)
    std::string path;          // full executable path; empty when windowless
    std::string window_title;  // main window title; empty when windowless
    bool        is_foreground = false;
    // Process creation time in Unix epoch ms; 0 when unavailable. Pairs with
    // `pid` to identify a process across client restarts — pids are recycled,
    // (pid, started_at_ms) is not.
    int64_t     started_at_ms = 0;
};

/// Returns running processes that match the bundled game list (assets/games.json).
std::vector<GameProcess> enumerate_game_processes();

struct VisibleWindow {
    void*       hwnd;
    std::string title;
    std::string exe;  // executable filename only
    std::string path; // full executable path
    uint32_t    pid;
};

/// Returns all visible top-level windows suitable for capture.
std::vector<VisibleWindow> enumerate_visible_windows();

} // namespace mello::video
