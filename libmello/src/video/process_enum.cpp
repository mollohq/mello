#include "process_enum.hpp"
#include "../util/log.hpp"

#ifdef _WIN32
#include "capture_process.hpp"
#include <Windows.h>
#include <dwmapi.h>
#include <TlHelp32.h>
#include <algorithm>
#include <cctype>
#pragma comment(lib, "dwmapi.lib")
#endif

namespace mello::video {

static constexpr const char* TAG = "video/process";

#ifdef _WIN32

// Hardcoded known game list — will be replaced with a bundled JSON file
// loaded at runtime once asset pipeline is in place.
struct KnownGame {
    const char* name;
    const char* exe;
};

static const KnownGame KNOWN_GAMES[] = {
    {"Minecraft",         "javaw.exe"},
    {"Fortnite",          "FortniteClient-Win64-Shipping.exe"},
    {"League of Legends", "League of Legends.exe"},
    {"Valorant",          "VALORANT-Win64-Shipping.exe"},
    {"Rocket League",     "RocketLeague.exe"},
    {"CS2",               "cs2.exe"},
    {"Roblox",            "RobloxPlayerBeta.exe"},
    {"Apex Legends",      "r5apex.exe"},
    {"Overwatch 2",       "Overwatch.exe"},
    {"GTA V",             "GTA5.exe"},
};

static bool iequals(const std::string& a, const std::string& b) {
    if (a.size() != b.size()) return false;
    for (size_t i = 0; i < a.size(); ++i) {
        if (std::tolower(static_cast<unsigned char>(a[i])) !=
            std::tolower(static_cast<unsigned char>(b[i]))) return false;
    }
    return true;
}

std::vector<GameProcess> enumerate_game_processes() {
    std::vector<GameProcess> result;

    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snap == INVALID_HANDLE_VALUE) {
        MELLO_LOG_ERROR(TAG, "CreateToolhelp32Snapshot failed");
        return result;
    }

    PROCESSENTRY32W pe{};
    pe.dwSize = sizeof(pe);

    if (Process32FirstW(snap, &pe)) {
        do {
            char exe_name[260]{};
            WideCharToMultiByte(CP_UTF8, 0, pe.szExeFile, -1, exe_name, sizeof(exe_name), nullptr, nullptr);

            for (const auto& game : KNOWN_GAMES) {
                if (iequals(exe_name, game.exe)) {
                    GameProcess gp;
                    gp.pid  = pe.th32ProcessID;
                    gp.name = game.name;
                    gp.exe  = exe_name;
                    gp.is_fullscreen = query_exclusive_fullscreen_output(pe.th32ProcessID) >= 0;
                    result.push_back(std::move(gp));
                    break;
                }
            }
        } while (Process32NextW(snap, &pe));
    }

    CloseHandle(snap);

    MELLO_LOG_DEBUG(TAG, "Enumerated %zu game processes", result.size());
    return result;
}

static BOOL CALLBACK enum_windows_cb(HWND hwnd, LPARAM lparam) {
    auto* result = reinterpret_cast<std::vector<VisibleWindow>*>(lparam);

    if (!IsWindowVisible(hwnd)) return TRUE;
    if (hwnd == GetDesktopWindow()) return TRUE;
    if (hwnd == GetShellWindow()) return TRUE;

    // Skip tool windows (floating toolbars, tooltips, etc.)
    LONG_PTR ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    if (ex_style & WS_EX_TOOLWINDOW) return TRUE;

    // Skip cloaked UWP windows (hidden Store apps, etc.)
    BOOL cloaked = FALSE;
    DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &cloaked, sizeof(cloaked));
    if (cloaked) return TRUE;

    int title_len = GetWindowTextLengthW(hwnd);
    if (title_len <= 0) return TRUE;

    std::wstring wtitle(title_len + 1, L'\0');
    GetWindowTextW(hwnd, wtitle.data(), title_len + 1);

    char title_utf8[256]{};
    WideCharToMultiByte(CP_UTF8, 0, wtitle.c_str(), -1, title_utf8, sizeof(title_utf8), nullptr, nullptr);

    DWORD pid = 0;
    GetWindowThreadProcessId(hwnd, &pid);

    VisibleWindow vw;
    vw.hwnd  = hwnd;
    vw.title = title_utf8;
    vw.pid   = pid;
    result->push_back(std::move(vw));

    return TRUE;
}

std::vector<VisibleWindow> enumerate_visible_windows() {
    std::vector<VisibleWindow> result;
    EnumWindows(enum_windows_cb, reinterpret_cast<LPARAM>(&result));
    MELLO_LOG_DEBUG(TAG, "Enumerated %zu visible windows", result.size());
    return result;
}

#else

std::vector<GameProcess> enumerate_game_processes() {
    MELLO_LOG_WARN(TAG, "Game process enumeration not supported on this platform");
    return {};
}

std::vector<VisibleWindow> enumerate_visible_windows() {
    MELLO_LOG_WARN(TAG, "Window enumeration not supported on this platform");
    return {};
}

#endif

} // namespace mello::video
