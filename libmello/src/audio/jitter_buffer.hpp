#pragma once
#include <cstdint>
#include <vector>
#include <map>
#include <mutex>

namespace mello::audio {

static constexpr int JITTER_MAX_PACKETS = 50;
static constexpr int JITTER_TARGET_MS = 60;   // Target buffering delay
static constexpr int JITTER_MIN_MS = 20;
static constexpr int JITTER_MAX_MS = 200;

struct JitterPacket {
    std::vector<uint8_t> data;
    uint32_t sequence;
    int64_t arrival_time_ms;
};

class JitterBuffer {
public:
    JitterBuffer();
    ~JitterBuffer() = default;

    void reset();

    // Push a received packet into the buffer
    void push(uint32_t sequence, const uint8_t* data, int size);

    // Get the next packet to decode. Returns false if none available.
    // Packet is removed from the buffer.
    bool pop(std::vector<uint8_t>& out_data);

    // Get stats
    int buffered_count() const;
    int target_delay_ms() const { return target_delay_ms_; }
    float avg_hold_ms() const { return avg_hold_ms_; }

private:
    int64_t now_ms() const;
    void adapt_target();

    std::map<uint32_t, JitterPacket> packets_;
    mutable std::mutex mutex_;

    uint32_t next_seq_ = 0;
    bool first_packet_ = true;
    int target_delay_ms_ = JITTER_TARGET_MS;
    int64_t last_pop_time_ = 0;

    // For adaptive delay estimation
    int64_t last_arrival_ = 0;
    float jitter_estimate_ = 0.0f;
    float avg_hold_ms_ = 0.0f;
};

} // namespace mello::audio
