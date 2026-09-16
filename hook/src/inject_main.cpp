// mello-inject — loads the hook DLL into one game process.
//
// Usage: mello-inject64.exe <pid> [timeout_ms]
//
// The method is the one from plan 3.4: a WH_GETMESSAGE window hook on a thread
// of the game that owns a window. Windows loads the DLL into the game when that
// thread next handles a message, so the helper posts one. There is no
// CreateRemoteThread path, now or later: anti-cheat products flag a remote
// thread far more often than a window hook.
//
// The client creates the shared block and the events before it runs this, so
// the helper can wait on the "ready" event the hook signals.
//
// Exit codes:
//   0  the hook signalled ready
//   1  a bad argument, or Windows refused the hook
//   2  the hook did not load in time
//   3  no window thread in the target process

#include <windows.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>

#include "hook_log.hpp"
#include "mello_hook_protocol.h"

namespace {

struct FindThread {
    DWORD pid = 0;
    DWORD thread = 0;
    HWND  window = nullptr;
    LONG  area = 0;
};

BOOL CALLBACK pick_window(HWND window, LPARAM param) {
    auto* find = reinterpret_cast<FindThread*>(param);
    DWORD pid = 0;
    const DWORD thread = GetWindowThreadProcessId(window, &pid);
    if (pid != find->pid || !IsWindowVisible(window)) return TRUE;

    RECT rect{};
    if (!GetWindowRect(window, &rect)) return TRUE;
    const LONG area = (rect.right - rect.left) * (rect.bottom - rect.top);
    if (area <= find->area) return TRUE;

    find->area = area;
    find->thread = thread;
    find->window = window;
    return TRUE;
}

// The game's own window thread is the one that pumps messages, so it is the one
// that loads the DLL.
bool find_window_thread(DWORD pid, DWORD* out_thread, HWND* out_window) {
    FindThread find;
    find.pid = pid;
    EnumWindows(pick_window, reinterpret_cast<LPARAM>(&find));
    if (!find.thread) return false;
    *out_thread = find.thread;
    *out_window = find.window;
    return true;
}

// The hook DLL sits next to this helper, so a moved or copied install still
// finds it and no search path is involved.
bool hook_dll_path(wchar_t* out, size_t count) {
    wchar_t self[MAX_PATH]{};
    if (GetModuleFileNameW(nullptr, self, MAX_PATH) == 0) return false;
    wchar_t* slash = wcsrchr(self, L'\\');
    if (!slash) return false;
    *slash = L'\0';
    _snwprintf_s(out, count, _TRUNCATE, L"%s\\mello-hook%d.dll", self, MELLO_HOOK_BITS);
    return true;
}

}  // namespace

int wmain(int argc, wchar_t** argv) {
    using namespace mello_hook;
    log_open("inject");

    if (argc < 2) {
        std::fprintf(stderr, "usage: mello-inject%d <pid> [timeout_ms]\n", MELLO_HOOK_BITS);
        return 1;
    }
    const DWORD pid = static_cast<DWORD>(_wtoi(argv[1]));
    DWORD timeout_ms = 4000;
    if (argc >= 3) {
        const int value = _wtoi(argv[2]);
        if (value > 0) timeout_ms = static_cast<DWORD>(value);
    }
    if (pid == 0) {
        std::fprintf(stderr, "bad pid\n");
        return 1;
    }

    DWORD thread = 0;
    HWND window = nullptr;
    if (!find_window_thread(pid, &thread, &window)) {
        log_line("pid %lu has no visible window thread", pid);
        return 3;
    }

    wchar_t dll[MAX_PATH]{};
    if (!hook_dll_path(dll, MAX_PATH)) {
        log_line("cannot build the hook path");
        return 1;
    }

    const HMODULE module = LoadLibraryW(dll);
    if (!module) {
        log_line("cannot load %ls: %lu", dll, GetLastError());
        return 1;
    }
    const auto proc = reinterpret_cast<HOOKPROC>(GetProcAddress(module, "mello_hook_proc"));
    if (!proc) {
        log_line("mello_hook_proc is missing from the hook DLL");
        FreeLibrary(module);
        return 1;
    }

    char name[64];
    object_name(name, sizeof(name), MELLO_HOOK_NAME_READY, pid);
    const HANDLE ready = OpenEventA(SYNCHRONIZE, FALSE, name);
    if (!ready) {
        // The client must create its block and events before it injects.
        log_line("no ready event for pid %lu; the client did not set up its block", pid);
        FreeLibrary(module);
        return 1;
    }

    const HHOOK hook = SetWindowsHookExW(WH_GETMESSAGE, proc, module, thread);
    if (!hook) {
        log_line("SetWindowsHookEx on thread %lu failed: %lu", thread, GetLastError());
        CloseHandle(ready);
        FreeLibrary(module);
        return 1;
    }

    // Windows loads the DLL when the thread handles its next message. A game
    // that is busy rendering may not see one, so the helper sends them.
    const UINT wake = RegisterWindowMessageA(MELLO_HOOK_WAKE_MESSAGE);
    const DWORD deadline = GetTickCount() + timeout_ms;
    DWORD result = WAIT_TIMEOUT;
    while (GetTickCount() < deadline) {
        PostThreadMessageW(thread, wake, 0, 0);
        if (window) PostMessageW(window, wake, 0, 0);
        result = WaitForSingleObject(ready, 100);
        if (result == WAIT_OBJECT_0) break;
    }

    // Removing the window hook makes Windows drop its reference to the DLL,
    // both here and in the game. The DLL pinned itself in DllMain, so it stays.
    UnhookWindowsHookEx(hook);
    CloseHandle(ready);
    // The DLL is deliberately not freed. It pinned itself in DllMain, so the
    // reference stays anyway, and this helper exits in a moment. Unloading a
    // DLL that runs a thread is how the helper used to crash.

    if (result != WAIT_OBJECT_0) {
        log_line("the hook did not report ready within %lu ms", timeout_ms);
        return 2;
    }
    log_line("hook ready in pid %lu (thread %lu)", pid, thread);
    return 0;
}
