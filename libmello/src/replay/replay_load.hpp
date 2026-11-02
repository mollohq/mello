#pragma once
#include <algorithm>
#include <atomic>
#include <cstdint>
#include <mutex>

namespace mello::replay {

/// The load level of the game clip replay under overload
/// (plans/GAME-CLIPS.md 3.6, F3). The replay sheds first: the first overload
/// signal drops it to 30 fps, the next one pauses it. Only then may the
/// stream give up its own quality. After 60 s with no overload the replay
/// goes one level back up. The ring keeps its content over a pause; a clip
/// shows the last frame for the gap.
///
/// Threads: `shed` on an encode thread (the stream's or the replay's),
/// `recover` on the capture thread, the readers on any thread.
class ReplayLoad {
public:
    enum class Level { Full = 0, Reduced = 1, Paused = 2 };

    static constexpr uint64_t kRecoverUs = 60'000'000;
    static constexpr uint32_t kReducedFps = 30;

    /// An overload signal at `now_us`. Goes one level down and returns true,
    /// or returns false when the replay is paused already (it has nothing
    /// left to give). At `full_fps` of 30 or less there is no reduced level:
    /// Full goes to Paused.
    bool shed(uint64_t now_us, uint32_t full_fps) {
        std::lock_guard<std::mutex> lock(mutex_);
        const Level now = level_.load(std::memory_order_relaxed);
        last_change_us_ = now_us;
        if (now == Level::Paused) return false;
        Level next = now == Level::Full ? Level::Reduced : Level::Paused;
        if (next == Level::Reduced && full_fps <= kReducedFps) next = Level::Paused;
        level_.store(next, std::memory_order_release);
        return true;
    }

    /// At `now_us`: after kRecoverUs with no overload and no change, one level
    /// up. Returns true when the level changed.
    bool recover(uint64_t now_us) {
        if (level_.load(std::memory_order_acquire) == Level::Full) return false;
        std::lock_guard<std::mutex> lock(mutex_);
        const Level now = level_.load(std::memory_order_relaxed);
        if (now == Level::Full || now_us < last_change_us_ + kRecoverUs) return false;
        level_.store(now == Level::Paused ? Level::Reduced : Level::Full, std::memory_order_release);
        last_change_us_ = now_us;
        return true;
    }

    Level level() const { return level_.load(std::memory_order_acquire); }

    /// The rate to record at: `full_fps`, 30 fps when reduced (never above
    /// `full_fps`), 0 when paused.
    uint32_t fps(uint32_t full_fps) const {
        switch (level()) {
            case Level::Full: return full_fps;
            case Level::Reduced: return std::min(full_fps, kReducedFps);
            case Level::Paused: return 0;
        }
        return full_fps;
    }

private:
    std::atomic<Level> level_{Level::Full};
    std::mutex mutex_;
    uint64_t last_change_us_ = 0;  // mutex_
};

}  // namespace mello::replay
