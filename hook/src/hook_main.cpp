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

#include "hook_d3d9.hpp"
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

    // A game uses one graphics API, and the modules it loaded say which. Both
    // are tried: a game can load d3d9.dll for its launcher and dxgi.dll for
    // itself, and a hook on a module the game never presents through costs
    // nothing.
    const bool dxgi = install_dxgi_hooks(*state.info());
    const bool d3d9 = install_d3d9_hooks(*state.info());
    if (!dxgi && !d3d9) {
        // `last_error` already says why. The client reads it and moves the
        // capture ladder on.
        state.signal_ready();
        log_line("no present function was hooked; the hook stays idle");
        log_close();
        return 0;
    }

    // From here the present path does the work. This thread only watches, for
    // as long as the game runs.
    //
    // It never ends on a stop. The first stream leaves this DLL loaded, and a
    // later stream has to find it armed: the client resets the ready event
    // before it injects, so this thread raising it again is what tells the
    // injection helper that a hook which is already in the game is listening.
    bool capturing = false;
    bool stopped = false;
    for (;;) {
        state.signal_ready();
        const bool stop = state.wait_for_stop(500);
        if (stop != stopped) {
            stopped = stop;
            if (stop) log_line("the client ended its stream");
        }
        // Capture itself follows `capture_enabled` and the heartbeat, which the
        // present path reads. The resources belong to that thread and it
        // releases them there; releasing them here could pull a texture out
        // from under a copy in progress.
        const bool wanted = !stop && state.capture_wanted();
        if (wanted != capturing) {
            capturing = wanted;
            log_line(capturing ? "capture on" : "capture off (stopped or heartbeat lost)");
        }
        if (stop) {
            // The stop event stays set until the next stream clears it. Sleep
            // rather than spin on it.
            Sleep(500);
        }
    }
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
