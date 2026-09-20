// ProcessLiveness: the captured process object is tracked by handle, so a
// quit game ends the stream instead of pausing it forever.
#include <gtest/gtest.h>

#ifdef _WIN32
#include "video/process_liveness.hpp"
#include <windows.h>

using mello::video::ProcessLiveness;

TEST(ProcessLiveness, CurrentProcessIsAlive) {
    ProcessLiveness t;
    t.track(GetCurrentProcessId());
    t.refresh();
    EXPECT_FALSE(t.exited());
}

TEST(ProcessLiveness, ZeroPidNeverExits) {
    ProcessLiveness t;
    t.track(0);
    t.refresh();
    EXPECT_FALSE(t.exited());
}

TEST(ProcessLiveness, TerminatedChildReportsExitedAndStaysExited) {
    // CREATE_SUSPENDED pins the exact process object: track() holds it by
    // handle, so no pid-reuse race can confuse the answer.
    STARTUPINFOA si{};
    si.cb = sizeof(si);
    PROCESS_INFORMATION pi{};
    ASSERT_TRUE(CreateProcessA(nullptr,
        const_cast<char*>("cmd.exe /c exit 0"), nullptr, nullptr, FALSE,
        CREATE_SUSPENDED | CREATE_NO_WINDOW, nullptr, nullptr, &si, &pi));
    ProcessLiveness t;
    t.track(static_cast<uint32_t>(pi.dwProcessId));
    t.refresh();
    EXPECT_FALSE(t.exited()) << "suspended child must read as alive";
    EXPECT_TRUE(TerminateProcess(pi.hProcess, 1));
    // Bounded wait: CI must never hang on a wedged child.
    EXPECT_EQ(WaitForSingleObject(pi.hProcess, 5000), WAIT_OBJECT_0);
    t.refresh();
    EXPECT_TRUE(t.exited());
    t.refresh();
    EXPECT_TRUE(t.exited()) << "exit is sticky";
    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);
}

TEST(ProcessLiveness, AlreadyDeadPidReportsExited) {
    STARTUPINFOA si{};
    si.cb = sizeof(si);
    PROCESS_INFORMATION pi{};
    ASSERT_TRUE(CreateProcessA(nullptr,
        const_cast<char*>("cmd.exe /c exit 0"), nullptr, nullptr, FALSE,
        CREATE_NO_WINDOW, nullptr, nullptr, &si, &pi));
    EXPECT_EQ(WaitForSingleObject(pi.hProcess, 5000), WAIT_OBJECT_0);
    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);
    ProcessLiveness t;
    t.track(static_cast<uint32_t>(pi.dwProcessId));
    EXPECT_TRUE(t.exited());
}
#else
TEST(ProcessLiveness, UnsupportedPlatformPasses) {
    SUCCEED();
}
#endif
