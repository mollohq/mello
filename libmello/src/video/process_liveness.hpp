#pragma once
// Tracks whether one specific OS process is still alive.
//
// Used to end a stream when the captured game quits. Distinct from
// target_available(): a minimized window pauses the stream, a dead process
// ends it. The two must not share a signal — while a quit game looks exactly
// like a tabbed-out one (no window, no frames), only the quit one is final.
//
// Tracks the process *object* via an open handle, not the pid: if the OS
// reuses the pid for a new process, the old handle still refers to the dead
// one. Answered from a sticky flag, so `exited()` is a plain atomic load.
//
// `refresh()` is const and internally synchronized: backends without a
// monitor thread (WGC) refresh lazily from the ~1 Hz stats poll, which may
// race stop(). The poll itself is cheap by design (zero-timeout wait).
//
// Bias is always toward alive: access-denied or any ambiguous answer means
// the stream keeps running. A stream must never end on uncertainty.

#ifdef _WIN32
#include <windows.h>
#include <atomic>
#include <cstdint>
#include <mutex>

namespace mello::video {

class ProcessLiveness {
public:
    ProcessLiveness() = default;
    ~ProcessLiveness() { reset(); }
    ProcessLiveness(const ProcessLiveness&) = delete;
    ProcessLiveness& operator=(const ProcessLiveness&) = delete;

    /// Bind to a pid. pid 0 (or untrackable targets like monitors) means
    /// "never exits": exited() stays false until track() is called again.
    /// Called once from CaptureSource::initialize.
    void track(uint32_t pid);

    /// Re-poll the tracked process. Cheap by design (zero-timeout wait).
    /// Called each monitor-thread pass, or lazily from target_exited() by
    /// backends without a monitor thread. Safe on any thread.
    void refresh() const;

    /// True once the tracked process is known dead. Sticky. Safe on any
    /// thread; called from the ~1 Hz stats poll.
    bool exited() const { return exited_.load(std::memory_order_relaxed); }

    /// Drop the handle and clear state. Called from CaptureSource::stop().
    void reset();

private:
    mutable std::mutex     mutex_;
    HANDLE                 handle_ = nullptr;      // guarded by mutex_
    uint32_t               fallback_pid_ = 0;      // guarded; polled when the handle was denied
    // Mutable: refresh() is const (lazy polling from the stats path) and
    // MSVC treats atomic::store as non-const.
    mutable std::atomic<bool> exited_{false};
};

} // namespace mello::video
#endif
