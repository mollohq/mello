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
static constexpr int64_t SEQ_DISCONTINUITY_THRESHOLD = 1000;

/// Extends a 16-bit RTP sequence number to a running 64-bit counter.
///
/// The SFU path carries the 16-bit RTP sequence in the packet header
/// (peer_connection.cpp); it wraps from 65535 to 0 every 22 minutes of
/// speech. The P2P path carries a 32-bit counter (AudioPipeline::get_packet).
/// Both use only the low 16 bits here: each value is placed at the extended
/// position nearest to the previous one, so a step of up to 32767 packets
/// either way keeps its order. Not thread safe; the owner locks.
class SequenceUnwrapper {
public:
    int64_t unwrap(uint32_t sequence);
    void reset() { has_last_ = false; }

private:
    bool has_last_ = false;
    int64_t last_ = 0;
};

struct JitterPacket {
    std::vector<uint8_t> data;
    int64_t sequence;
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

    // `sequence` is the packet header value: a 16-bit RTP sequence (SFU) or
    // a 32-bit counter (P2P). The buffer unwraps it (SequenceUnwrapper), so
    // the timeline below sees one monotonic extended sequence.
    void push(uint32_t sequence, const uint8_t* data, int size);

    // Pops from playout timeline:
    // - Packet when data is ready
    // - Missing when a packet is considered lost and concealment should run
    // - None when still prebuffering / waiting for delay
    // out_sequence receives the extended sequence of the packet, or of the
    // lost packet for Missing.
    JitterPopResult pop(std::vector<uint8_t>& out_data, int64_t* out_sequence = nullptr);

    // Copies the payload of the buffered packet with extended sequence
    // `sequence` into out_data, and leaves it in the buffer. False when the
    // packet is not buffered. The receive path reads the packet after a
    // Missing one this way, to conceal the loss with its in-band FEC.
    bool peek(int64_t sequence, std::vector<uint8_t>& out_data) const;

    int buffered_count() const;
    int target_delay_ms() const { return target_delay_ms_; }
    float avg_hold_ms() const { return avg_hold_ms_; }
    uint32_t underruns() const { return underruns_; }
    // Diagnostic counters for the voice quality gate. Lifetime of this
    // buffer; reset() does not clear them.
    uint32_t dropped_late() const { return dropped_late_; }
    uint32_t dropped_overflow() const { return dropped_overflow_; }
    uint32_t discontinuity_resets() const { return discontinuity_resets_; }

private:
    int64_t now_ms() const;
    void adapt_target();
    void reset_locked();

    std::map<int64_t, JitterPacket> packets_;
    mutable std::mutex mutex_;

    SequenceUnwrapper unwrapper_;
    int64_t next_seq_ = 0;
    bool first_packet_ = true;
    bool prebuffering_ = true;
    int target_delay_ms_ = JITTER_TARGET_MS;
    int64_t last_pop_time_ = 0;
    int64_t stream_start_ms_ = 0;

    int64_t last_arrival_ = 0;
    float jitter_estimate_ = 0.0f;
    float avg_hold_ms_ = 0.0f;
    uint32_t underruns_ = 0;
    uint32_t dropped_late_ = 0;
    uint32_t dropped_overflow_ = 0;
    uint32_t discontinuity_resets_ = 0;
};

} // namespace mello::audio
