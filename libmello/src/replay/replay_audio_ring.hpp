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

/// One 10 ms chunk in the game audio ring.
struct AudioChunkRef {
    uint64_t seq = 0;
    uint64_t capture_us = 0;  ///< capture time of the first frame, steady clock
    uint32_t frames = 0;      ///< valid frames, at most kChunkFrames
};

/// The rolling game audio buffer of the game clip replay (plans/GAME-CLIPS.md
/// 5.2): 48 kHz stereo int16 PCM in a ring file of 10 ms chunks, with the
/// capture time of each chunk.
///
/// The audio thread hands packets of any size to a bounded RAM queue. The
/// writer thread cuts them into chunks and writes them to the file. A packet
/// whose time is more than kGapUs after the end of the chunk before it starts
/// a new chunk, so a gap keeps its place in time. The mux places each chunk at
/// its own time; it does not count samples from the start.
///
/// A window reads the chunks after the ring went on: it checks the sequence
/// number of each chunk on disk, and a chunk that the writer overwrote reads
/// as missing (silence in the clip). The ring holds 40 s and a clip 31 s, so
/// this needs a mux of more than 9 s.
///
/// Threads: `push` on the audio capture thread. `start`, `stop` and `flush`
/// on one control thread. `window` and the window reads on any thread.
class ReplayAudioRing {
public:
    static constexpr uint32_t kRate = 48000;
    static constexpr uint32_t kChannels = 2;
    static constexpr uint32_t kChunkFrames = kRate / 100;  // 10 ms
    static constexpr uint32_t kSeconds = 40;
    /// A packet this much later than the end of the chunk before it starts a
    /// new chunk. Arrival-time stamps jitter by a few milliseconds.
    static constexpr uint64_t kGapUs = 20'000;
    /// The RAM queue between the audio thread and the writer.
    static constexpr uint32_t kQueueFrames = 2 * kRate;

    struct Config {
        std::filesystem::path dir;  ///< empty: util::default_clip_dir()
        uint32_t seconds = kSeconds;
        /// False: no writer thread, only `flush` writes. For tests that
        /// must control when the queue drains.
        bool writer_thread = true;
    };

    struct Stats {
        uint64_t chunks_written = 0;
        uint64_t frames_dropped = 0;  ///< frames the full queue dropped
    };

    class Window;

    ReplayAudioRing() = default;
    ~ReplayAudioRing();
    ReplayAudioRing(const ReplayAudioRing&) = delete;
    ReplayAudioRing& operator=(const ReplayAudioRing&) = delete;

    bool start(const Config& config);
    void stop();
    bool is_active() const { return active_.load(std::memory_order_acquire); }

    /// Audio thread. `interleaved` holds `frames` stereo frames at 48 kHz.
    /// `capture_us` is the capture time of the first frame.
    void push(const int16_t* interleaved, size_t frames, uint64_t capture_us);

    /// Write what the queue holds now, on this thread. A partly filled chunk
    /// is written too, and the next chunk starts after it.
    void flush();

    /// The chunks that overlap [start_us, end_us), oldest first.
    Window window(uint64_t start_us, uint64_t end_us);

    Stats stats() const;

    struct State;

private:
    void writer_loop();
    void drain(bool partial);  // file_mutex_ held by the caller
    bool write_chunk();        // file_mutex_ held

    struct Packet {
        std::vector<int16_t> samples;
        uint64_t capture_us;
    };

    std::shared_ptr<State> state_;

    // Audio thread to writer.
    std::mutex queue_mutex_;
    std::deque<Packet> queue_;
    uint64_t queued_frames_ = 0;
    uint64_t dropped_frames_ = 0;  // since the last drain

    // The chunk being filled (file_mutex_).
    mutable std::mutex file_mutex_;
    std::vector<int16_t> chunk_;
    uint32_t chunk_frames_ = 0;
    uint64_t chunk_us_ = 0;

    std::thread writer_;
    std::mutex wake_mutex_;
    std::condition_variable wake_;
    bool stop_writer_ = false;

    std::atomic<bool> active_{false};
};

/// The chunks of a time range. Keeps the ring file open, so a mux can finish
/// after the ring stops. Move-only.
class ReplayAudioRing::Window {
public:
    Window() = default;
    Window(Window&&) noexcept = default;
    Window& operator=(Window&&) noexcept = default;
    Window(const Window&) = delete;
    Window& operator=(const Window&) = delete;

    bool empty() const { return chunks_.empty(); }
    const std::vector<AudioChunkRef>& chunks() const { return chunks_; }
    /// Read `chunk.frames` interleaved stereo frames into `out`. False when
    /// the writer overwrote the chunk or the read failed.
    bool read(const AudioChunkRef& chunk, int16_t* out) const;

private:
    friend class ReplayAudioRing;
    std::shared_ptr<ReplayAudioRing::State> state_;
    std::vector<AudioChunkRef> chunks_;
};

} // namespace mello::replay
