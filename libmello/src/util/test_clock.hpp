#pragma once
#include <atomic>
#include <chrono>
#include <cstdint>

namespace mello::util {

// Playout clock with an optional test override.
//
// The jitter buffer times packet holds against this clock. Production never
// sets the override, so steady_now_ms() is std::chrono::steady_clock in ms.
// The voice quality gate (mello-sys/tests/voice_gate.rs) sets it through
// mello_test_set_clock_ms() so a run is deterministic and faster than real
// time. Any thread may read it; the test harness is the only writer.
inline std::atomic<int64_t> g_test_clock_ms{-1};

inline void set_test_clock_ms(int64_t now_ms) {
    g_test_clock_ms.store(now_ms < 0 ? -1 : now_ms, std::memory_order_relaxed);
}

inline int64_t steady_now_ms() {
    const int64_t test_now = g_test_clock_ms.load(std::memory_order_relaxed);
    if (test_now >= 0) return test_now;
    return std::chrono::duration_cast<std::chrono::milliseconds>(
               std::chrono::steady_clock::now().time_since_epoch())
        .count();
}

} // namespace mello::util
