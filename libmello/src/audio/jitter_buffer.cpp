#include "jitter_buffer.hpp"
#include <chrono>
#include <algorithm>
#include <cmath>

namespace mello::audio {

JitterBuffer::JitterBuffer() = default;

void JitterBuffer::reset() {
    std::lock_guard<std::mutex> lock(mutex_);
    packets_.clear();
    next_seq_ = 0;
    first_packet_ = true;
    target_delay_ms_ = JITTER_TARGET_MS;
    last_pop_time_ = 0;
    last_arrival_ = 0;
    jitter_estimate_ = 0.0f;
}

int64_t JitterBuffer::now_ms() const {
    auto now = std::chrono::steady_clock::now();
    return std::chrono::duration_cast<std::chrono::milliseconds>(
        now.time_since_epoch()).count();
}

void JitterBuffer::push(uint32_t sequence, const uint8_t* data, int size) {
    std::lock_guard<std::mutex> lock(mutex_);

    int64_t arrival = now_ms();

    if (first_packet_) {
        next_seq_ = sequence;
        first_packet_ = false;
        last_arrival_ = arrival;
    }

    // Estimate jitter from inter-arrival variance
    if (last_arrival_ > 0) {
        float delta = static_cast<float>(arrival - last_arrival_);
        // Expected inter-arrival is 20ms (one Opus frame)
        float deviation = std::abs(delta - 20.0f);
        // Exponential moving average
        jitter_estimate_ = jitter_estimate_ * 0.95f + deviation * 0.05f;
    }
    last_arrival_ = arrival;

    // Drop very old packets
    if (packets_.size() >= JITTER_MAX_PACKETS) {
        packets_.erase(packets_.begin());
    }

    // Don't accept packets that are older than what we've already played
    if (!packets_.empty() && sequence < next_seq_) {
        return;
    }

    JitterPacket pkt;
    pkt.data.assign(data, data + size);
    pkt.sequence = sequence;
    pkt.arrival_time_ms = arrival;

    packets_[sequence] = std::move(pkt);
    adapt_target();
}

bool JitterBuffer::pop(std::vector<uint8_t>& out_data) {
    std::lock_guard<std::mutex> lock(mutex_);

    auto it = packets_.find(next_seq_);
    if (it == packets_.end()) {
        // Packet loss -- skip ahead if we have later packets
        if (!packets_.empty() && packets_.begin()->first > next_seq_ + 3) {
            next_seq_ = packets_.begin()->first;
            it = packets_.begin();
        } else {
            return false;
        }
    }

    int64_t hold = now_ms() - it->second.arrival_time_ms;
    avg_hold_ms_ = avg_hold_ms_ * 0.9f + static_cast<float>(hold) * 0.1f;

    out_data = std::move(it->second.data);
    packets_.erase(it);
    next_seq_++;
    last_pop_time_ = now_ms();
    return true;
}

int JitterBuffer::buffered_count() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return static_cast<int>(packets_.size());
}

void JitterBuffer::adapt_target() {
    // Adapt target delay based on observed jitter
    // Target = 2 * jitter_estimate, clamped to [MIN, MAX]
    int new_target = static_cast<int>(jitter_estimate_ * 2.0f + 20.0f);
    new_target = std::max(JITTER_MIN_MS, std::min(JITTER_MAX_MS, new_target));

    // Smooth transition
    target_delay_ms_ = (target_delay_ms_ * 7 + new_target) / 8;
}

} // namespace mello::audio
