#include "hook_log.hpp"

#include <windows.h>

#include <cstdarg>
#include <cstdio>

namespace mello_hook {
namespace {

HANDLE g_file = INVALID_HANDLE_VALUE;
char   g_role[16] = "hook";

}  // namespace

void log_open(const char* role) {
    if (g_file != INVALID_HANDLE_VALUE) return;
    if (role) {
        std::snprintf(g_role, sizeof(g_role), "%s", role);
    }

    char dir[MAX_PATH]{};
    const DWORD len = GetTempPathA(MAX_PATH, dir);
    if (len == 0 || len >= MAX_PATH) return;

    char path[MAX_PATH]{};
    std::snprintf(path, sizeof(path), "%smello-%s-%lu.log", dir, g_role, GetCurrentProcessId());

    // Opened for append and shared for reading, so the client can read the log
    // while the game runs.
    g_file = CreateFileA(path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr,
                         OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, nullptr);
}

void log_line(const char* format, ...) {
    char line[512];
    SYSTEMTIME t{};
    GetLocalTime(&t);
    const int head = std::snprintf(line, sizeof(line), "%02u:%02u:%02u.%03u [%s:%lu] ",
                                   t.wHour, t.wMinute, t.wSecond, t.wMilliseconds,
                                   g_role, GetCurrentProcessId());
    if (head < 0 || head >= static_cast<int>(sizeof(line))) return;

    va_list args;
    va_start(args, format);
    const int body = std::vsnprintf(line + head, sizeof(line) - head - 2, format, args);
    va_end(args);
    if (body < 0) return;

    int total = head + body;
    if (total > static_cast<int>(sizeof(line)) - 2) total = static_cast<int>(sizeof(line)) - 2;
    line[total] = '\n';
    line[total + 1] = '\0';

    // The debugger view is what a developer reads while a game runs.
    OutputDebugStringA(line);
    if (g_file != INVALID_HANDLE_VALUE) {
        DWORD written = 0;
        WriteFile(g_file, line, static_cast<DWORD>(total + 1), &written, nullptr);
    }
}

void log_close() {
    if (g_file == INVALID_HANDLE_VALUE) return;
    CloseHandle(g_file);
    g_file = INVALID_HANDLE_VALUE;
}

}  // namespace mello_hook
