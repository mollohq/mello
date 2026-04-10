#include "clip_buffer.hpp"
#include "../util/log.hpp"
#include <fstream>
#include <algorithm>
#include <cstring>

namespace mello::audio {

ClipBuffer::ClipBuffer(int sample_rate, int buffer_seconds)
    : capacity_(static_cast<size_t>(sample_rate) * buffer_seconds)
    , sample_rate_(sample_rate) {
    ring_.resize(capacity_, 0);
}

void ClipBuffer::start() {
    std::lock_guard<std::mutex> lock(mutex_);
    write_pos_ = 0;
    total_written_ = 0;
    std::fill(ring_.begin(), ring_.end(), static_cast<int16_t>(0));
    active_.store(true, std::memory_order_release);
    MELLO_LOG_INFO("clip_buffer", "started (capacity=%zu samples, %.1fs)",
                   capacity_, static_cast<float>(capacity_) / sample_rate_);
}

void ClipBuffer::stop() {
    active_.store(false, std::memory_order_release);
    MELLO_LOG_INFO("clip_buffer", "stopped");
}

void ClipBuffer::write(const int16_t* samples, size_t count) {
    if (!active_.load(std::memory_order_relaxed)) return;

    std::lock_guard<std::mutex> lock(mutex_);
    for (size_t i = 0; i < count; ++i) {
        ring_[write_pos_] = samples[i];
        write_pos_ = (write_pos_ + 1) % capacity_;
    }
    total_written_ += count;
}

bool ClipBuffer::capture(float seconds, const std::string& output_path) {
    if (seconds <= 0.0f || output_path.empty()) {
        MELLO_LOG_WARN("clip_buffer", "capture: invalid params (seconds=%.1f)", seconds);
        return false;
    }

    size_t requested = static_cast<size_t>(seconds * sample_rate_);
    std::vector<int16_t> out;

    {
        std::lock_guard<std::mutex> lock(mutex_);

        size_t available = std::min(total_written_, capacity_);
        size_t to_copy = std::min(requested, available);
        if (to_copy == 0) {
            MELLO_LOG_WARN("clip_buffer", "capture: no audio in buffer");
            return false;
        }

        out.resize(to_copy);

        // Read backwards from write_pos_
        size_t start;
        if (write_pos_ >= to_copy) {
            start = write_pos_ - to_copy;
        } else {
            start = capacity_ - (to_copy - write_pos_);
        }

        // Linearize the ring segment into `out`
        size_t first_chunk = std::min(to_copy, capacity_ - start);
        std::memcpy(out.data(), ring_.data() + start, first_chunk * sizeof(int16_t));
        if (first_chunk < to_copy) {
            std::memcpy(out.data() + first_chunk, ring_.data(),
                        (to_copy - first_chunk) * sizeof(int16_t));
        }

        MELLO_LOG_INFO("clip_buffer", "capture: extracted %zu samples (%.1fs) from buffer",
                        to_copy, static_cast<float>(to_copy) / sample_rate_);
    }

    if (!write_wav(output_path, out.data(), out.size(), sample_rate_)) {
        MELLO_LOG_ERROR("clip_buffer", "capture: failed to write WAV to %s", output_path.c_str());
        return false;
    }

    MELLO_LOG_INFO("clip_buffer", "capture: saved %s (%.1fs, %zu bytes)",
                    output_path.c_str(), static_cast<float>(out.size()) / sample_rate_,
                    out.size() * sizeof(int16_t) + 44);
    return true;
}

bool ClipBuffer::write_wav(const std::string& path, const int16_t* data,
                           size_t sample_count, int sample_rate) {
    std::ofstream file(path, std::ios::binary);
    if (!file) return false;

    uint32_t data_size = static_cast<uint32_t>(sample_count * sizeof(int16_t));
    uint32_t file_size = data_size + 36;
    uint16_t channels = 1;
    uint16_t bits_per_sample = 16;
    uint32_t sr = static_cast<uint32_t>(sample_rate);
    uint32_t byte_rate = sr * channels * bits_per_sample / 8;
    uint16_t block_align = channels * bits_per_sample / 8;
    uint32_t fmt_size = 16;
    uint16_t audio_format = 1; // PCM

    file.write("RIFF", 4);
    file.write(reinterpret_cast<const char*>(&file_size), 4);
    file.write("WAVE", 4);

    file.write("fmt ", 4);
    file.write(reinterpret_cast<const char*>(&fmt_size), 4);
    file.write(reinterpret_cast<const char*>(&audio_format), 2);
    file.write(reinterpret_cast<const char*>(&channels), 2);
    file.write(reinterpret_cast<const char*>(&sr), 4);
    file.write(reinterpret_cast<const char*>(&byte_rate), 4);
    file.write(reinterpret_cast<const char*>(&block_align), 2);
    file.write(reinterpret_cast<const char*>(&bits_per_sample), 2);

    file.write("data", 4);
    file.write(reinterpret_cast<const char*>(&data_size), 4);
    file.write(reinterpret_cast<const char*>(data), data_size);

    return file.good();
}

} // namespace mello::audio
