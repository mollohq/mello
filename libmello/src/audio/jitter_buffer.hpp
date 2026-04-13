#pragma once
#include <cstdint>
#include <vector>
#include <map>
#include <mutex>

namespace mello::audio {

static constexpr int JITTER_MAX_PACKETS = 50;
static constexpr int JITTER_TARGET_MS = 60;
static constexpr int JITTER_MIN_MS = 20;
static constexpr int JITTER_MAX_MS = 200;
static constexpr uint32_t SEQ_DISCONTINUITY_THRESHOLD = 1000;

struct JitterPacket {
    std::vector<uint8_t> data;
    uint32_t sequence;
    int64_t arrival_time_ms;
};

enum class JitterPopResult {
    None,    // No packet ready yet
    Packet,  // Packet popped into out_data/out_sequence
    Missing, // Expected packet considered lost; playout should conceal
};

class JitterBuffer {
public:
    JitterBuffer();
    ~JitterBuffer() = default;

    void reset();

    void push(uint32_t sequence, const uint8_t* data, int size);

    // Pops from playout timeline:
    // - Packet when data is ready
    // - Missing when a packet is considered lost and concealment should run
    // - None when still prebuffering / waiting for delay
    JitterPopResult pop(std::vector<uint8_t>& out_data, uint32_t* out_sequence = nullptr);

    int buffered_count() const;
    int target_delay_ms() const { return target_delay_ms_; }
    float avg_hold_ms() const { return avg_hold_ms_; }
    uint32_t underruns() const { return underruns_; }

private:
    int64_t now_ms() const;
    void adapt_target();
    void reset_locked();

    std::map<uint32_t, JitterPacket> packets_;
    mutable std::mutex mutex_;

    uint32_t next_seq_ = 0;
    bool first_packet_ = true;
    bool prebuffering_ = true;
    int target_delay_ms_ = JITTER_TARGET_MS;
    int64_t last_pop_time_ = 0;
    int64_t stream_start_ms_ = 0;

    int64_t last_arrival_ = 0;
    float jitter_estimate_ = 0.0f;
    float avg_hold_ms_ = 0.0f;
    uint32_t underruns_ = 0;
};

} // namespace mello::audio
