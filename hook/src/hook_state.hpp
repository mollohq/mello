// The hook's view of the shared block the client created.
//
// The client creates the shared memory, the events and the keepalive mutex
// before it injects. This class only opens them. If any of it is missing, the
// hook stays dormant: that is the required behaviour if this DLL is ever loaded
// into a process m3llo did not mean to hook.
//
// Threads:
//  - `open`, `close` and `wait_for_stop` run on the hook thread.
//  - `capture_wanted`, `publish_frame` and `set_error` run on the game's
//    present thread. They take no lock and allocate nothing.

#pragma once

#include <windows.h>

#include <cstdint>

#include "mello_hook_protocol.h"

namespace mello_hook {

class HookState {
public:
    static HookState& instance();

    // Opens the block and the events for this process. False means the client
    // is not asking for anything and the hook must do nothing.
    bool open();
    void close();

    bool is_open() const { return info_ != nullptr; }
    MelloHookInfo* info() { return info_; }

    // True while the client wants frames and is still alive. Called on the
    // present path, so it only reads two numbers.
    bool capture_wanted() const;

    // Publishes a captured frame. `index` is the texture the frame landed in.
    // The frame counter is written last, with a release, so a reader that sees
    // the new counter also sees the texture index it belongs to.
    void publish_frame(uint32_t texture_index, uint64_t qpc);

    void publish_description(uint32_t api, uint32_t width, uint32_t height,
                             uint32_t dxgi_format, uint64_t adapter_luid,
                             const uint32_t* handles, uint32_t flags);

    void set_error(uint32_t error);
    void count_drop();
    void count_fault();
    /// Called on every present, before anything else. It is two instructions on
    /// the game's render thread and it is what tells the client the difference
    /// between a hook that cannot see and a game that is not drawing.
    void count_present();

    /// True when a frame is due at the client's `target_fps`, at QPC time
    /// `qpc`. The rule is libmello's `frame_cadence_accepts`: a credit
    /// accumulator, so a 144 fps game gives 60 copies for a 60 fps stream.
    /// Present thread only (the callers' reentrancy guard serializes it). Two
    /// loads and some arithmetic: nothing that allocates, locks or logs.
    bool copy_due(int64_t qpc);
    /// Counts one copy in `frames_copied`.
    void count_copy();

    void signal_ready();
    void signal_frame();

    // Waits up to `wait_ms` for the client to ask the hook to stop. True means
    // stop. The hook thread uses this as its whole loop.
    bool wait_for_stop(DWORD wait_ms) const;

    // True while the client's heartbeat is recent enough.
    bool client_alive() const;

private:
    HookState() = default;
    ~HookState() = default;
    HookState(const HookState&) = delete;
    HookState& operator=(const HookState&) = delete;

    HANDLE         mapping_    = nullptr;
    MelloHookInfo* info_       = nullptr;
    HANDLE         ready_event_ = nullptr;
    HANDLE         frame_event_ = nullptr;
    HANDLE         stop_event_  = nullptr;
    int64_t        qpc_frequency_ = 0;
    // When the next copy is due, in microseconds of QPC. Present thread only.
    uint64_t       copy_deadline_us_ = 0;
    uint32_t       copy_fps_ = 0;
};

// QueryPerformanceCounter, for the frame timestamps.
int64_t qpc_now();

}  // namespace mello_hook
