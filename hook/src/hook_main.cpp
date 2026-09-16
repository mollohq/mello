// The hook DLL's entry points and its one thread.
//
// What runs where:
//  - `DllMain` starts a thread and returns. Nothing else. Any other work would
//    run under the loader lock, inside a game, which is how hooks crash games.
//  - `mello_hook_proc` is the window hook procedure the injection helper points
//    SetWindowsHookEx at. It does nothing but pass the message on. Loading this
//    DLL into the game is its whole purpose.
//  - `hook_thread` opens the shared block, installs the detours, and then
//    watches for the client going away.
//
// This DLL pins itself. The injection helper calls UnhookWindowsHookEx as soon
// as the hook is in, and the unload that follows would pull the code out from
// under a game thread that is inside a detour.

#include <windows.h>

#include "hook_dxgi.hpp"
#include "hook_log.hpp"
#include "hook_state.hpp"
#include "mello_hook_protocol.h"

namespace {

HMODULE g_module = nullptr;

// Keeps the DLL loaded for the life of the process. Detours cannot be taken out
// safely while a game thread may be inside one, so the code must stay.
void pin_module() {
    HMODULE pinned = nullptr;
    GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                       reinterpret_cast<LPCWSTR>(&pin_module), &pinned);
}

DWORD WINAPI hook_thread(LPVOID) {
    using namespace mello_hook;

    log_open("hook");
    pin_module();

    HookState& state = HookState::instance();
    if (!state.open()) {
        // No shared block: this process is not one m3llo asked for. Say nothing
        // and do nothing for the rest of its life.
        log_close();
        return 0;
    }

    log_line("hook %u-bit loaded into pid %lu", static_cast<unsigned>(MELLO_HOOK_BITS),
             GetCurrentProcessId());

    if (!install_dxgi_hooks(*state.info())) {
        // `last_error` already says why. The client reads it and moves the
        // capture ladder on.
        state.signal_ready();
        log_line("no present function was hooked; the hook stays idle");
        log_close();
        return 0;
    }

    // From here the present path does the work. This thread only watches.
    state.signal_ready();

    bool capturing = false;
    for (;;) {
        if (state.wait_for_stop(500)) {
            log_line("client asked the hook to stop");
            break;
        }
        const bool wanted = state.capture_wanted();
        if (wanted != capturing) {
            capturing = wanted;
            log_line(capturing ? "capture on" : "capture off (stopped or heartbeat lost)");
        }
    }

    stop_dxgi_capture();
    log_line("hook idle; the detours stay in place until the game exits");
    log_close();
    return 0;
}

}  // namespace

// The injection helper points SetWindowsHookEx at this. It must do nothing.
//
// It is exported by name through src/mello_hook.def, not with dllexport: on
// x86 the calling convention would decorate the name and the helper's
// GetProcAddress would miss it.
extern "C" LRESULT CALLBACK mello_hook_proc(int code, WPARAM wparam, LPARAM lparam) {
    return CallNextHookEx(nullptr, code, wparam, lparam);
}

BOOL APIENTRY DllMain(HMODULE module, DWORD reason, LPVOID) {
    if (reason == DLL_PROCESS_ATTACH) {
        g_module = module;
        DisableThreadLibraryCalls(module);
        const HANDLE thread = CreateThread(nullptr, 0, hook_thread, nullptr, 0, nullptr);
        if (thread) CloseHandle(thread);
    }
    return TRUE;
}
