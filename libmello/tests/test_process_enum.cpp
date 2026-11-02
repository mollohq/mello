// Tests for process enumeration (video/process_enum.cpp).
//
// These run against the live system process list, so they assert structural
// invariants rather than specific processes: enumeration succeeds, windowed
// processes carry a full path + title, and windowless ones stay empty.

#include <gtest/gtest.h>

#include <algorithm>
#include <chrono>

#include "video/process_enum.hpp"

using mello::video::enumerate_game_processes;
using mello::video::enumerate_visible_windows;

TEST(ProcessEnum, EnumeratesProcesses) {
#if !defined(_WIN32) && !defined(__linux__)
    // process_enum.cpp enumerates on Windows and Linux; the other branch
    // returns an empty vector, so this can never pass elsewhere.
    GTEST_SKIP() << "process enumeration is Windows and Linux only";
#else
    auto procs = enumerate_game_processes();
    ASSERT_FALSE(procs.empty());
    for (const auto& p : procs) {
        EXPECT_FALSE(p.exe.empty());
        // The exe field stays a bare filename (the game-DB matching key).
        EXPECT_EQ(p.exe.find('\\'), std::string::npos);
    }
#endif
}

TEST(ProcessEnum, WindowedProcessesCarryPathAndTitle) {
#ifndef _WIN32
    // These are the Windows rules. process_enum_macos.mm does not follow them
    // yet: it never sets a window title, it sets a path for every known app,
    // and its exe is the app name. The test then fails on any Mac with a known
    // app open (Steam on the CI runner). Spec 17 section 8.3.
    GTEST_SKIP() << "the windowless and path rules are Windows-only";
#else
    auto procs = enumerate_game_processes();
    int windowed = 0;
    for (const auto& p : procs) {
        if (p.window_title.empty()) {
            // Windowless processes must not report a path (it is only
            // resolved for processes with a visible window).
            EXPECT_TRUE(p.path.empty()) << p.exe;
            continue;
        }
        ++windowed;
        // A windowed process resolves its full executable path unless access
        // was denied (elevated processes).
        if (!p.path.empty()) {
            EXPECT_NE(p.path.find('\\'), std::string::npos) << p.path;
            // Path ends with the exe filename.
            ASSERT_GE(p.path.size(), p.exe.size());
            EXPECT_EQ(p.path.substr(p.path.size() - p.exe.size()), p.exe);
        }
    }
    // Interactive sessions always have visible windows; headless CI may not.
    if (windowed == 0) {
        GTEST_SKIP() << "no visible windows in this session";
    }
#endif
}

TEST(ProcessEnum, AtMostOneForegroundProcess) {
    auto procs = enumerate_game_processes();
    int fg = 0;
    for (const auto& p : procs) {
        if (p.is_foreground) ++fg;
    }
    EXPECT_LE(fg, 1);
}

// Sessions are dated from the process creation time, so a game already running
// when Mello launches reports the hours it actually ran rather than the minutes
// we happened to watch. A wrong value here silently corrupts every duration.
TEST(ProcessEnum, ProcessesCarryACreationTime) {
#if !defined(_WIN32) && !defined(__linux__)
    GTEST_SKIP() << "process enumeration is Windows and Linux only";
#else
    auto procs = enumerate_game_processes();
    ASSERT_FALSE(procs.empty());

    // Wall clock now, in the same units the field uses.
    const int64_t now_ms =
        std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::system_clock::now().time_since_epoch())
            .count();

    int with_time = 0;
    for (const auto& p : procs) {
        if (p.started_at_ms == 0) continue;  // protected/system process
        ++with_time;
        // Sanity-bound rather than exact: a creation time in the future or
        // before Windows existed means the FILETIME conversion is wrong.
        EXPECT_GT(p.started_at_ms, 946684800000LL) << p.exe;  // > year 2000
        EXPECT_LE(p.started_at_ms, now_ms + 1000) << p.exe;   // not in the future
    }
    // This process is ours, so at least one creation time must be readable.
    EXPECT_GT(with_time, 0);
#endif
}

TEST(ProcessEnum, VisibleWindowsExposeFullPath) {
    auto windows = enumerate_visible_windows();
    for (const auto& w : windows) {
        if (!w.path.empty()) {
            EXPECT_NE(w.path.find('\\'), std::string::npos) << w.path;
        }
    }
}

#if defined(__linux__)
#include <unistd.h>

// Linux reads /proc. The test process itself must be in the list, under the
// name of its executable, with the start time of a process that began
// recently.
TEST(ProcessEnum, LinuxListsThisProcessWithItsStartTime) {
    const auto procs = enumerate_game_processes();
    const uint32_t self = static_cast<uint32_t>(getpid());
    const auto it = std::find_if(procs.begin(), procs.end(),
                                 [&](const auto& p) { return p.pid == self; });
    ASSERT_NE(it, procs.end());
    EXPECT_EQ(it->exe, "mello_procenum_tests");
    EXPECT_EQ(it->path.substr(0, 1), "/");
    const int64_t now_ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                               std::chrono::system_clock::now().time_since_epoch())
                               .count();
    EXPECT_LE(it->started_at_ms, now_ms + 1000);
    EXPECT_GT(it->started_at_ms, now_ms - 10 * 60 * 1000) << "started over 10 min ago";
}
#endif

#if !defined(__APPLE__)
// process_enum.cpp is not compiled on Apple (process_enum_macos.mm is).
using mello::video::detail::path_basename;

TEST(ProcessEnum, BasenameSplitsOnBothSeparators) {
    EXPECT_EQ(path_basename(R"(Z:\a\b\cs2.exe)"), "cs2.exe");
    EXPECT_EQ(path_basename("/usr/bin/mello"), "mello");
    EXPECT_EQ(path_basename("plain"), "plain");
}
#endif

#if defined(__linux__)
#include <filesystem>

using mello::video::detail::wine_path_to_host;

namespace {
// A Proton prefix like the one on a real install: c: -> ../drive_c,
// s: -> the Steam library, z: -> /.
struct FakePrefix {
    std::filesystem::path root;
    std::filesystem::path library;
    FakePrefix() {
        root = std::filesystem::temp_directory_path() /
               ("mello-wine-" + std::to_string(::getpid()));
        std::filesystem::remove_all(root);
        library = root / "Steam";
        std::filesystem::create_directories(root / "pfx/dosdevices");
        std::filesystem::create_directories(root / "pfx/drive_c/Games");
        std::filesystem::create_directories(library / "steamapps/common/Game");
        std::filesystem::create_directory_symlink("../drive_c", root / "pfx/dosdevices/c:");
        std::filesystem::create_directory_symlink(library, root / "pfx/dosdevices/s:");
        std::filesystem::create_directory_symlink("/", root / "pfx/dosdevices/z:");
    }
    ~FakePrefix() { std::filesystem::remove_all(root); }
    std::string prefix() const { return (root / "pfx").string(); }
};
} // namespace

TEST(ProcessEnum, ProtonSteamDriveMapsThroughThePrefix) {
    FakePrefix p;
    const std::string want = (std::filesystem::canonical(p.library) /
                              "steamapps/common/Game/game.exe").string();
    EXPECT_EQ(wine_path_to_host(R"(S:\steamapps\common\Game\game.exe)", p.prefix()), want);
    // The long form that Wine also uses.
    EXPECT_EQ(wine_path_to_host(R"(\\?\S:\steamapps\common\Game\game.exe)", p.prefix()), want);
}

TEST(ProcessEnum, DriveCResolvesToDriveC) {
    FakePrefix p;
    EXPECT_EQ(wine_path_to_host(R"(C:\Games\g.exe)", p.prefix()),
              (std::filesystem::canonical(p.root / "pfx/drive_c") / "Games/g.exe").string());
}

TEST(ProcessEnum, ZDriveMapsWithOrWithoutAPrefix) {
    FakePrefix p;
    EXPECT_EQ(wine_path_to_host(R"(Z:\opt\g.exe)", p.prefix()), "/opt/g.exe");
    EXPECT_EQ(wine_path_to_host(R"(z:\opt\g.exe)", ""), "/opt/g.exe");
}

TEST(ProcessEnum, AnUnknownDriveHasNoHostPath) {
    FakePrefix p;
    EXPECT_EQ(wine_path_to_host(R"(D:\g.exe)", p.prefix()), "");
    EXPECT_EQ(wine_path_to_host(R"(C:\windows\steam.exe)", ""), "");
    EXPECT_EQ(wine_path_to_host("Z:", p.prefix()), "");
    EXPECT_EQ(wine_path_to_host("game.exe", p.prefix()), "");
}
#endif
