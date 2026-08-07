// Tests for process enumeration (video/process_enum.cpp).
//
// These run against the live system process list, so they assert structural
// invariants rather than specific processes: enumeration succeeds, windowed
// processes carry a full path + title, and windowless ones stay empty.

#include <gtest/gtest.h>

#include "video/process_enum.hpp"

using mello::video::enumerate_game_processes;
using mello::video::enumerate_visible_windows;

TEST(ProcessEnum, EnumeratesProcesses) {
#ifndef _WIN32
    // process_enum.cpp only has a Windows implementation; the other branch
    // returns an empty vector, so this can never pass elsewhere.
    GTEST_SKIP() << "process enumeration is Windows-only";
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
}

TEST(ProcessEnum, AtMostOneForegroundProcess) {
    auto procs = enumerate_game_processes();
    int fg = 0;
    for (const auto& p : procs) {
        if (p.is_foreground) ++fg;
    }
    EXPECT_LE(fg, 1);
}

TEST(ProcessEnum, VisibleWindowsExposeFullPath) {
    auto windows = enumerate_visible_windows();
    for (const auto& w : windows) {
        if (!w.path.empty()) {
            EXPECT_NE(w.path.find('\\'), std::string::npos) << w.path;
        }
    }
}
