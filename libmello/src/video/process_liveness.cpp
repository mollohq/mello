#ifdef _WIN32
#include "process_liveness.hpp"
#include "../util/log.hpp"

namespace mello::video {

static constexpr const char* TAG = "video/capture";

void ProcessLiveness::track(uint32_t pid) {
    reset();
    if (pid == 0) return;
    HANDLE h = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
    if (h) {
        std::lock_guard<std::mutex> lock(mutex_);
        handle_ = h;
    } else if (GetLastError() == ERROR_INVALID_PARAMETER) {
        // No such process: already gone before capture started.
        MELLO_LOG_INFO(TAG, "ProcessLiveness: pid=%u already exited at track time", pid);
        exited_.store(true, std::memory_order_relaxed);
        return;
    } else {
        // Access denied (elevated game): the process exists, but we cannot
        // hold it. Fall back to pid polling; any ambiguous answer reads as
        // alive.
        MELLO_LOG_INFO(TAG, "ProcessLiveness: pid=%u handle denied (err=%u), polling by pid",
            pid, static_cast<unsigned>(GetLastError()));
        std::lock_guard<std::mutex> lock(mutex_);
        fallback_pid_ = pid;
    }
    // The process may already be a zombie: resolve it now rather than
    // waiting for the first poll.
    refresh();
}

void ProcessLiveness::refresh() const {
    if (exited_.load(std::memory_order_relaxed)) return;
    std::lock_guard<std::mutex> lock(mutex_);
    if (exited_.load(std::memory_order_relaxed)) return;
    if (handle_) {
        if (WaitForSingleObject(handle_, 0) == WAIT_OBJECT_0) {
            MELLO_LOG_INFO(TAG, "ProcessLiveness: tracked process exited");
            exited_.store(true, std::memory_order_relaxed);
        }
        return;
    }
    if (fallback_pid_ == 0) return;
    HANDLE h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, fallback_pid_);
    if (!h) {
        if (GetLastError() == ERROR_INVALID_PARAMETER) {
            MELLO_LOG_INFO(TAG, "ProcessLiveness: pid=%u gone", fallback_pid_);
            exited_.store(true, std::memory_order_relaxed);
        }
        return;
    }
    DWORD code = STILL_ACTIVE;
    if (GetExitCodeProcess(h, &code) && code != STILL_ACTIVE) {
        MELLO_LOG_INFO(TAG, "ProcessLiveness: pid=%u exited (code=%u)",
            fallback_pid_, static_cast<unsigned>(code));
        exited_.store(true, std::memory_order_relaxed);
    }
    CloseHandle(h);
}

void ProcessLiveness::reset() {
    std::lock_guard<std::mutex> lock(mutex_);
    if (handle_) {
        CloseHandle(handle_);
        handle_ = nullptr;
    }
    fallback_pid_ = 0;
    exited_.store(false, std::memory_order_relaxed);
}

} // namespace mello::video
#endif
