#pragma once
#include <vector>
#include <mutex>
#include <atomic>
#include <string>
#include <cstdint>

namespace mello::audio {

// Rolling PCM ring buffer for voice clip capture.
// Holds the last N seconds of mixed audio output. On capture,
// extracts a time window, writes it as WAV to disk.
// Thread-safe: write() from playback thread, capture() from any thread.
class ClipBuffer {
public:
    explicit ClipBuffer(int sample_rate = 48000, int buffer_seconds = 60);

    // Append mixed audio samples. Called from the playback render path — must be fast.
    void write(const int16_t* samples, size_t count);

    // Extract the last `seconds` of audio and save as WAV to `output_path`.
    // Blocks until file is written. Returns true on success.
    bool capture(float seconds, const std::string& output_path);

    void start();
    void stop();
    bool is_active() const { return active_.load(std::memory_order_relaxed); }

private:
    bool write_wav(const std::string& path, const int16_t* data,
                   size_t sample_count, int sample_rate);

    std::vector<int16_t> ring_;
    size_t write_pos_ = 0;
    size_t total_written_ = 0;
    size_t capacity_;
    int sample_rate_;
    std::atomic<bool> active_{false};
    mutable std::mutex mutex_;
};

} // namespace mello::audio
