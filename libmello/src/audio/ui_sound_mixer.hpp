#pragma once

// Notification sounds (specs/features/NOTIFICATION-SOUNDS.md): short 48 kHz stereo
// sounds that the app loads once and plays on events (a member joins, a
// mute, a message). The render thread mixes them into the speaker output
// after the deafen gate and after the clip-buffer tap.
//
// Threads:
// - Control (any thread, the caller serializes, as for every MelloContext
//   call): load, play. They take `control_mutex_` only.
// - Render (the audio device thread): mix_into. It takes no lock and does
//   not allocate.
//
// A loaded sound never changes and is freed only with the mixer. A second
// load of the same slot publishes a new buffer and keeps the old one, so a
// sound that plays on the render thread stays valid.

#include <array>
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <mutex>
#include <vector>

namespace mello::audio {

class UiSoundMixer {
public:
    static constexpr uint32_t kSampleRate = 48000;
    static constexpr uint32_t kChannels = 2;
    static constexpr uint32_t kMaxSounds = 32;
    /// Sounds that play at the same time. A play over the limit replaces the
    /// sound that started first.
    static constexpr size_t kMaxVoices = 4;
    /// Play requests that wait for the render thread. A request over the
    /// limit is dropped.
    static constexpr size_t kQueueSize = 16;
    /// At most 10 s of sound in one slot.
    static constexpr size_t kMaxFrames = kSampleRate * 10;

    UiSoundMixer();

    // ---- Control ---------------------------------------------------------

    /// Copies `frames` interleaved stereo frames into slot `id`. Returns
    /// false for a bad id, no PCM or more than kMaxFrames.
    bool load(uint32_t id, const int16_t* stereo, size_t frames);
    /// Queues slot `id` to play at `gain` (0..1, clamped). Returns false
    /// when the slot is empty or the queue is full.
    bool play(uint32_t id, float gain);
    bool loaded(uint32_t id) const;

    // ---- Render thread -----------------------------------------------------

    /// Adds the next `frames` stereo frames of every sound that plays into
    /// `stereo`, and their mono mix into `mono` (the echo canceller's
    /// reference), saturated. Returns true when any sound played.
    bool mix_into(int16_t* stereo, int16_t* mono, size_t frames);

    /// Sounds that play now. For tests and logs; any thread.
    size_t active_voices() const { return active_.load(std::memory_order_relaxed); }

private:
    struct Sound {
        std::vector<int16_t> pcm;  // interleaved stereo
        size_t frames = 0;
    };
    struct Request {
        uint32_t id = 0;
        float gain = 0.0f;
    };
    struct Voice {
        const Sound* sound = nullptr;
        size_t pos = 0;  // frames played
        float gain = 0.0f;
        uint64_t order = 0;  // start order, for replacement
    };

    void take_requests();

    std::mutex control_mutex_;
    std::vector<std::unique_ptr<Sound>> owned_;  // under control_mutex_
    std::array<std::atomic<const Sound*>, kMaxSounds> sounds_;

    // One producer (under control_mutex_), one consumer (the render thread).
    std::array<Request, kQueueSize> queue_{};
    std::atomic<uint64_t> queue_write_{0};
    std::atomic<uint64_t> queue_read_{0};

    // Render thread only.
    std::array<Voice, kMaxVoices> voices_{};
    uint64_t next_order_ = 0;
    std::atomic<size_t> active_{0};
};

}  // namespace mello::audio
