#include "jitter_buffer.hpp"
#include "../util/test_clock.hpp"
#include <algorithm>
#include <cmath>

namespace mello::audio {

int64_t SequenceUnwrapper::unwrap(uint32_t sequence) {
    const uint16_t low = static_cast<uint16_t>(sequence & 0xFFFFu);
    if (!has_last_) {
        has_last_ = true;
        last_ = low;
        return last_;
    }
    // Signed 16-bit distance from the previous value: wrap-safe.
    const auto step = static_cast<int16_t>(
        static_cast<uint16_t>(low - static_cast<uint16_t>(last_ & 0xFFFF)));
    last_ += step;
    return last_;
}

JitterBuffer::JitterBuffer() = default;

void JitterBuffer::reset() {
    std::lock_guard<std::mutex> lock(mutex_);
    reset_locked();
    unwrapper_.reset();
}

void JitterBuffer::reset_locked() {
    packets_.clear();
    next_seq_ = 0;
    first_packet_ = true;
    prebuffering_ = true;
    target_delay_ms_ = JITTER_TARGET_MS;
    last_pop_time_ = 0;
    stream_start_ms_ = 0;
    last_arrival_ = 0;
    jitter_estimate_ = 0.0f;
}

int64_t JitterBuffer::now_ms() const {
    // steady_clock in production; the voice quality gate can drive it.
    return util::steady_now_ms();
}

void JitterBuffer::push(uint32_t raw_sequence, const uint8_t* data, int size) {
    std::lock_guard<std::mutex> lock(mutex_);

    // Every comparison below is on the extended sequence, so the 16-bit RTP
    // wrap is an ordinary step of one.
    const int64_t sequence = unwrapper_.unwrap(raw_sequence);
    int64_t arrival = now_ms();

    if (first_packet_) {
        next_seq_ = sequence;
        first_packet_ = false;
        prebuffering_ = true;
        stream_start_ms_ = arrival;
        last_arrival_ = arrival;
    }

    // Detect sequence discontinuity (track re-wire) and reset. The
    // unwrapper keeps its state: the new stream continues from here.
    if (!first_packet_ && packets_.empty()) {
        const int64_t gap = (sequence > next_seq_)
            ? sequence - next_seq_
            : next_seq_ - sequence;
        if (gap > SEQ_DISCONTINUITY_THRESHOLD) {
            discontinuity_resets_++;
            reset_locked();
            next_seq_ = sequence;
            first_packet_ = false;
            prebuffering_ = true;
            stream_start_ms_ = arrival;
            last_arrival_ = arrival;
        }
    }

    if (last_arrival_ > 0 && arrival > last_arrival_) {
        float delta = static_cast<float>(arrival - last_arrival_);
        float deviation = std::abs(delta - 20.0f);
        jitter_estimate_ = jitter_estimate_ * 0.95f + deviation * 0.05f;
    }
    last_arrival_ = arrival;

    if (packets_.size() >= JITTER_MAX_PACKETS) {
        packets_.erase(packets_.begin());
        dropped_overflow_++;
    }

    if (!packets_.empty() && sequence < next_seq_) {
        dropped_late_++;
        return;
    }

    JitterPacket pkt;
    pkt.data.assign(data, data + size);
    pkt.sequence = sequence;
    pkt.arrival_time_ms = arrival;

    packets_[sequence] = std::move(pkt);
    adapt_target();
}

JitterPopResult JitterBuffer::pop(std::vector<uint8_t>& out_data, int64_t* out_sequence) {
    std::lock_guard<std::mutex> lock(mutex_);

    if (packets_.empty()) {
        return JitterPopResult::None;
    }

    // Pre-buffering: wait until we've accumulated enough packets before
    // first playout, giving the buffer a head start against jitter.
    if (prebuffering_) {
        int64_t elapsed = now_ms() - stream_start_ms_;
        int needed = std::max(2, target_delay_ms_ / 20);
        if (static_cast<int>(packets_.size()) < needed && elapsed < target_delay_ms_) {
            return JitterPopResult::None;
        }
        prebuffering_ = false;
    }

    auto it = packets_.find(next_seq_);
    if (it == packets_.end()) {
        // If newer packets have already been buffered long enough, consider
        // the expected packet lost and let the caller conceal.
        if (!packets_.empty() && packets_.begin()->first > next_seq_) {
            int64_t oldest_hold = now_ms() - packets_.begin()->second.arrival_time_ms;
            if (oldest_hold >= target_delay_ms_ ||
                static_cast<int>(packets_.size()) >= JITTER_MAX_PACKETS/3) {
                underruns_++;
                next_seq_++;
                return JitterPopResult::Missing;
            }
        }
        return JitterPopResult::None;
    }

    // Enforce playout delay: don't release a packet until it has been
    // held in the buffer for at least target_delay_ms_.
    int64_t hold = now_ms() - it->second.arrival_time_ms;
    if (hold < target_delay_ms_ && static_cast<int>(packets_.size()) < JITTER_MAX_PACKETS / 2) {
        return JitterPopResult::None;
    }

    avg_hold_ms_ = avg_hold_ms_ * 0.9f + static_cast<float>(hold) * 0.1f;

    if (out_sequence) {
        *out_sequence = it->second.sequence;
    }
    out_data = std::move(it->second.data);
    packets_.erase(it);
    next_seq_++;
    last_pop_time_ = now_ms();
    return JitterPopResult::Packet;
}

int JitterBuffer::buffered_count() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return static_cast<int>(packets_.size());
}

void JitterBuffer::adapt_target() {
    int new_target = static_cast<int>(jitter_estimate_ * 2.0f + 20.0f);
    new_target = std::max(JITTER_MIN_MS, std::min(JITTER_MAX_MS, new_target));
    target_delay_ms_ = (target_delay_ms_ * 7 + new_target) / 8;
}

} // namespace mello::audio
