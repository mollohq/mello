#pragma once
#include <atomic>
#include <condition_variable>
#include <cstdint>
#include <deque>
#include <filesystem>
#include <memory>
#include <mutex>
#include <thread>
#include <vector>

namespace mello::replay {

/// One access unit in the ring file.
struct AccessUnitRef {
    uint64_t segment_seq = 0;
    uint64_t offset = 0;      ///< byte offset in the ring file
    uint32_t size = 0;
    uint64_t capture_us = 0;  ///< capture time of the frame, steady clock
    bool     keyframe = false;
};

/// The rolling video buffer of the game clip replay (CLIPS.md 15.4).
///
/// One preallocated ring file of fixed slots. A slot holds one segment: one
/// GOP, from an IDR with SPS and PPS up to the next IDR. An index in RAM keeps
/// the byte offset, the size, the capture time and the keyframe flag of each
/// access unit.
///
/// The encode thread hands each access unit to a bounded RAM queue, and a
/// writer thread writes the queue to the file. A disk stall never blocks the
/// encoder: a full queue drops the unit, and every unit after it up to the
/// next keyframe, because a P-frame without its reference cannot be decoded.
/// The ring then asks for a keyframe (`take_keyframe_request`).
///
/// A capture pins the segments that it reads (`pin_window`). The writer skips
/// a pinned slot and takes the next free one. The file is deleted when the
/// ring stops and the last window is gone, and when the process dies.
///
/// Threads: `push` and `take_keyframe_request` on the encode thread. `start`,
/// `stop` and `flush` on one control thread. `pin_window` and the window
/// reads on any thread.
class ReplayVideoRing {
public:
    /// 30 s of clip, one GOP before it, and spare segments for pinning (5.2).
    static constexpr uint32_t kSegments = 40;
    /// The RAM queue between the encode thread and the writer (CLIPS.md 15.11).
    static constexpr uint64_t kQueueBytes = 2ull * 1024 * 1024;

    struct Config {
        std::filesystem::path dir;          ///< empty: util::default_clip_dir()
        uint32_t segments = kSegments;
        uint64_t segment_bytes = 0;         ///< capacity of one slot
        uint64_t queue_bytes = kQueueBytes;
        /// False: no writer thread, only `flush` writes. For tests that
        /// must control when the queue drains.
        bool writer_thread = true;
    };

    /// The slot size for a bitrate: two seconds of the peak rate (1.25 x the
    /// bitrate). The recording VBV is one second of the peak rate
    /// (`vbv_bits_for`), so a GOP of one second carries at most that much.
    static uint64_t segment_bytes_for(uint32_t bitrate_kbps);

    struct Stats {
        uint64_t units_written = 0;
        uint64_t segments_written = 0;
        uint64_t queue_drops = 0;     ///< units the full queue dropped
        uint64_t overflow_drops = 0;  ///< units that did not fit in their slot
        uint64_t pinned_skips = 0;    ///< segments that skipped a pinned slot
        uint64_t no_slot_drops = 0;   ///< segments with every slot pinned
    };

    class Window;

    ReplayVideoRing() = default;
    ~ReplayVideoRing();
    ReplayVideoRing(const ReplayVideoRing&) = delete;
    ReplayVideoRing& operator=(const ReplayVideoRing&) = delete;

    /// Make the ring file and start the writer. False, and inactive, when the
    /// file cannot be made.
    bool start(const Config& config);
    /// Stop the writer. The file goes when the last window is gone.
    void stop();
    /// False after `stop` and after a write to the file failed.
    bool is_active() const { return active_.load(std::memory_order_acquire); }

    /// Encode thread. Takes the unit into the RAM queue; never waits on disk.
    void push(std::vector<uint8_t>&& data, uint64_t capture_us, bool keyframe);
    /// Encode thread. True once after the ring dropped units: the next frame
    /// must be an IDR to start a new segment.
    bool take_keyframe_request() {
        return keyframe_request_.exchange(false, std::memory_order_acq_rel);
    }

    /// Write what the queue holds now, on this thread. The writer thread does
    /// the same every few milliseconds.
    void flush();

    /// Pin the segments from the last keyframe at or before `start_us` up to
    /// the newest unit at or before `end_us`. When the ring is younger than
    /// the window, the window starts at the oldest segment. An empty window
    /// when the ring has no unit in the range.
    Window pin_window(uint64_t start_us, uint64_t end_us);

    Stats stats() const;

    struct State;

private:
    void writer_loop();
    void drain();  // file_mutex_ held by the caller
    void write_unit(std::vector<uint8_t>& data, uint64_t capture_us, bool keyframe);
    static Stats stats_of(const State& state);

    struct QueuedUnit {
        std::vector<uint8_t> data;
        uint64_t capture_us;
        bool keyframe;
    };

    std::shared_ptr<State> state_;

    // Encode thread to writer.
    std::mutex queue_mutex_;
    std::deque<QueuedUnit> queue_;
    uint64_t queued_bytes_ = 0;
    uint64_t queue_limit_ = kQueueBytes;
    uint64_t dropped_in_queue_ = 0;  // since the last drain
    bool push_dropping_ = false;     // drop units up to the next keyframe
    std::atomic<bool> keyframe_request_{false};

    // Serializes the writer thread and `flush`, and guards `state_`.
    mutable std::mutex file_mutex_;
    bool write_skipping_ = false;  // file_mutex_: drop units up to the next keyframe

    std::thread writer_;
    std::mutex wake_mutex_;
    std::condition_variable wake_;
    bool stop_writer_ = false;

    std::atomic<bool> active_{false};
};

/// Pinned segments of the ring. The writer does not reuse their slots while
/// the window lives. Move-only. The window keeps the ring file open, so a mux
/// can finish after the ring stops.
class ReplayVideoRing::Window {
public:
    Window() = default;
    ~Window();
    Window(Window&& other) noexcept;
    Window& operator=(Window&& other) noexcept;
    Window(const Window&) = delete;
    Window& operator=(const Window&) = delete;

    bool empty() const { return units_.empty(); }
    /// The units in decode order. The first one is a keyframe.
    const std::vector<AccessUnitRef>& units() const { return units_; }
    /// Read the bytes of one unit. False on a read error.
    bool read(const AccessUnitRef& unit, std::vector<uint8_t>& out) const;
    /// Unpin now. The destructor does the same.
    void release();

private:
    friend class ReplayVideoRing;
    std::shared_ptr<ReplayVideoRing::State> state_;
    std::vector<uint32_t> slots_;
    std::vector<AccessUnitRef> units_;
};

} // namespace mello::replay
