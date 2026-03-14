#include "process_enum.hpp"
#include "../util/log.hpp"

#ifdef _WIN32
#include "capture_process.hpp"
#include <Windows.h>
#include <TlHelp32.h>
#include <algorithm>
#include <cctype>
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

#else

std::vector<GameProcess> enumerate_game_processes() {
    MELLO_LOG_WARN(TAG, "Game process enumeration not supported on this platform");
    return {};
}

#endif

} // namespace mello::video
