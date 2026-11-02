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

/// Returns every running process. The game catalogue in mello-core decides
/// which of them is a game (spec 17 section 2.1); libmello does not filter.
///
/// Linux: from /proc, the user's own processes only (another user's exe link
/// is not readable). A Wine or Proton process reports the Windows program of
/// its argv0, not the Wine preloader that /proc/<pid>/exe names, with its
/// drive mapped to the host path through the process's Wine prefix. No window
/// information: mello-core adds it from KWin (spec 17 section 2.9).
std::vector<GameProcess> enumerate_game_processes();

namespace detail {

#if defined(__linux__)
/// The host path of a Wine path. The drive letter resolves through the
/// prefix's `dosdevices/<letter>:` link: Proton starts a Steam game from `S:`
/// (the Steam library), `C:` is `drive_c`, `Z:` is `/`. Without a prefix only
/// `Z:` maps. Accepts the `\\?\` form. Empty when the drive has no link.
std::string wine_path_to_host(const std::string& windows_path, const std::string& wine_prefix);
#endif

/// The last component of a path, split on both `/` and `\`.
std::string path_basename(const std::string& path);

} // namespace detail

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
