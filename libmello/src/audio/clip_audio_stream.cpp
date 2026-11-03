#include "clip_audio_stream.hpp"

#include <algorithm>
#include <cstring>

namespace mello::audio {

ClipAudioStream::ClipAudioStream(size_t capacity_frames)
    : ring_(capacity_frames * kChannels), capacity_(capacity_frames) {}

size_t ClipAudioStream::free_frames() const {
    const uint64_t w = write_pos_.load(std::memory_order_relaxed);
    const uint64_t r = reset_pending_.load(std::memory_order_acquire)
                           ? reset_read_.load(std::memory_order_relaxed)
                           : read_pos_.load(std::memory_order_acquire);
    return capacity_ - static_cast<size_t>(w - r);
}

size_t ClipAudioStream::buffered_frames() const {
    const uint64_t w = write_pos_.load(std::memory_order_acquire);
    const uint64_t r = read_pos_.load(std::memory_order_acquire);
    return w > r ? static_cast<size_t>(w - r) : 0;
}

size_t ClipAudioStream::write(const int16_t* stereo, size_t frames) {
    const size_t n = std::min(frames, free_frames());
    uint64_t w = write_pos_.load(std::memory_order_relaxed);
    for (size_t done = 0; done < n;) {
        const size_t at = static_cast<size_t>(w % capacity_);
        const size_t run = std::min(n - done, capacity_ - at);
        std::memcpy(&ring_[at * kChannels], stereo + done * kChannels, run * kChannels * sizeof(int16_t));
        done += run;
        w += run;
    }
    write_pos_.store(w, std::memory_order_release);
    return n;
}

void ClipAudioStream::reset(uint64_t position_frames) {
    reset_read_.store(write_pos_.load(std::memory_order_relaxed), std::memory_order_relaxed);
    reset_played_.store(position_frames, std::memory_order_relaxed);
    reset_pending_.store(true, std::memory_order_release);
    // The clock reads the new position at once, not at the next mix.
    played_.store(position_frames, std::memory_order_release);
    ended_.store(false, std::memory_order_release);
}

void ClipAudioStream::apply_reset() {
    if (!reset_pending_.load(std::memory_order_acquire)) return;
    read_pos_.store(reset_read_.load(std::memory_order_relaxed), std::memory_order_release);
    played_.store(reset_played_.load(std::memory_order_relaxed), std::memory_order_release);
    reset_pending_.store(false, std::memory_order_release);
}

bool ClipAudioStream::mix_into(int16_t* stereo, int16_t* mono, size_t frames, int64_t now_us) {
    apply_reset();
    if (paused_.load(std::memory_order_acquire)) return false;
    const uint64_t r = read_pos_.load(std::memory_order_relaxed);
    const uint64_t w = write_pos_.load(std::memory_order_acquire);
    const size_t n = std::min(frames, static_cast<size_t>(w - r));
    for (size_t i = 0; i < n; ++i) {
        const size_t at = static_cast<size_t>((r + i) % capacity_) * kChannels;
        const int32_t l = ring_[at];
        const int32_t rr = ring_[at + 1];
        const int32_t out_l = std::clamp(static_cast<int32_t>(stereo[2 * i]) + l, -32768, 32767);
        const int32_t out_r = std::clamp(static_cast<int32_t>(stereo[2 * i + 1]) + rr, -32768, 32767);
        stereo[2 * i] = static_cast<int16_t>(out_l);
        stereo[2 * i + 1] = static_cast<int16_t>(out_r);
        if (mono) {
            mono[i] = static_cast<int16_t>(
                std::clamp(static_cast<int32_t>(mono[i]) + (l + rr) / 2, -32768, 32767));
        }
    }
    read_pos_.store(r + n, std::memory_order_release);
    played_.fetch_add(n, std::memory_order_acq_rel);
    if (n > 0) last_mix_us_.store(now_us, std::memory_order_release);
    return n > 0;
}

}  // namespace mello::audio
