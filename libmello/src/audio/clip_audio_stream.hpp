#pragma once

// The sound of a game clip that plays (plans/GAME-CLIPS.md 6.2): 48 kHz
// stereo PCM from the clip's demux thread to the audio render thread, and
// the clip clock. The voice clip playback (a mono file decoded whole into a
// vector) is not reused: a game clip streams, and its picture follows this
// clock.
//
// Threads: one producer (the demux thread: write, reset, set_ended), one
// consumer (the render thread: mix_into). The render thread takes no lock.
// The other calls are for any thread.

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <vector>

namespace mello::audio {

class ClipAudioStream {
public:
    static constexpr uint32_t kSampleRate = 48000;
    static constexpr uint32_t kChannels = 2;

    /// `capacity_frames`: how much PCM may wait, in stereo frames.
    explicit ClipAudioStream(size_t capacity_frames = kSampleRate * 4);

    // ---- Producer --------------------------------------------------------

    /// Copies up to `frames` stereo frames. Returns how many fit.
    size_t write(const int16_t* stereo, size_t frames);
    size_t free_frames() const;
    /// A seek: drops the PCM that waits and sets the clock to
    /// `position_frames`. The render thread applies it at its next mix.
    void reset(uint64_t position_frames);
    /// The clip has no more sound after the PCM that waits.
    void set_ended(bool ended) { ended_.store(ended, std::memory_order_release); }

    // ---- Consumer (render thread) -----------------------------------------

    /// Adds the next `frames` stereo frames into `stereo` and their mono mix
    /// into `mono` (the echo canceller's reference), saturated. Fewer when
    /// fewer wait. Returns true when any frame played. Nothing plays while
    /// paused. `now_us` (steady clock) is kept as the time of the mix.
    bool mix_into(int16_t* stereo, int16_t* mono, size_t frames, int64_t now_us = 0);

    // ---- Any thread -----------------------------------------------------

    void set_paused(bool paused) { paused_.store(paused, std::memory_order_release); }
    bool paused() const { return paused_.load(std::memory_order_acquire); }
    bool ended() const { return ended_.load(std::memory_order_acquire); }
    /// The clip clock: the frames played since the start, a seek included.
    /// It stops while paused and while no PCM waits (buffering).
    uint64_t played_frames() const { return played_.load(std::memory_order_acquire); }
    /// The steady time of the last mix that played: the clock moves on from
    /// it between two mixes.
    int64_t last_mix_us() const { return last_mix_us_.load(std::memory_order_acquire); }
    size_t buffered_frames() const;

private:
    void apply_reset();

    std::vector<int16_t> ring_;  // interleaved stereo
    const size_t capacity_;      // frames
    std::atomic<uint64_t> write_pos_{0};  // frames written, producer
    std::atomic<uint64_t> read_pos_{0};   // frames read, consumer
    std::atomic<uint64_t> played_{0};
    std::atomic<int64_t> last_mix_us_{0};
    std::atomic<bool> paused_{false};
    std::atomic<bool> ended_{false};
    // A reset waits for the render thread: read up to `reset_read_` and set
    // the clock to `reset_played_`.
    std::atomic<bool> reset_pending_{false};
    std::atomic<uint64_t> reset_read_{0};
    std::atomic<uint64_t> reset_played_{0};
};

}  // namespace mello::audio
