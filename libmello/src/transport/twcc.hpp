#pragma once

#include <cstddef>
#include <cstdint>
#include <deque>
#include <mutex>
#include <unordered_map>
#include <vector>

namespace mello::transport {

// Transport-Wide Congestion Control (TWCC, RFC 8888) building blocks for the
// native RTP stream path:
//
// - TwccSendStamper: stamps transport-wide sequence numbers into egress RTP
//   (header extension, one-byte-header profile 0xBEDE) and records per-packet
//   send times for later feedback processing.
// - parse_twcc_feedback: parses TWCC RTCP feedback (PT=205, FMT=15).
// - TwccFeedbackGenerator: receiver-side feedback report builder.
// - GccEstimator: delay-gradient (GCC-style) send-side bitrate estimator fed
//   by parsed feedback.

inline constexpr char kTwccExtensionUri[] =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";
inline constexpr int kTwccExtensionId = 3;

class TwccSendStamper {
public:
    // Stamps `packet` (marshaled RTP bytes) with the next transport-wide
    // sequence number and records `send_time_us` for it. Re-stamping a
    // packet that already carries our element (retransmit) overwrites the
    // sequence in place. Returns false for non-RTP input.
    bool stamp(std::vector<uint8_t>& packet, int64_t send_time_us);

    // Last sequence assigned by stamp() (for tests and diagnostics).
    uint16_t next_sequence() const { return next_sequence_; }

    // Send time recorded for a feedback-referenced sequence. False when the
    // record was evicted or never existed.
    bool send_time_for(uint16_t sequence, int64_t& out_us) const;

private:
    static constexpr size_t kMaxSendRecords = 2048;

    // stamp() runs on the pacing worker; send_time_for() on the RTCP thread.
    mutable std::mutex mutex_;
    uint16_t next_sequence_ = 0;
    std::unordered_map<uint16_t, int64_t> send_times_;
    std::deque<uint16_t> send_order_;
};

struct TwccPacketResult {
    uint16_t sequence = 0;
    bool received = false;
    // Valid only when received: absolute arrival time in microseconds on the
    // receiver's clock (reconstructed from reference time + deltas).
    int64_t arrival_time_us = 0;
};

struct TwccFeedback {
    uint32_t sender_ssrc = 0;
    uint32_t media_ssrc = 0;
    uint8_t feedback_packet_count = 0;
    std::vector<TwccPacketResult> packets;
};

// Parses one TWCC RTCP packet (a single feedback message, not a compound).
// Returns false on malformed input.
bool parse_twcc_feedback(
    const uint8_t* data,
    size_t size,
    TwccFeedback& out
);

// Reads the transport-wide sequence from a marshaled RTP packet's header
// extension (our extmap id). False when absent or unparsable.
bool extract_twcc_sequence(
    const uint8_t* data,
    size_t size,
    uint16_t& out_sequence
);

class TwccFeedbackGenerator {
public:
    // Records the arrival of one TWCC-stamped packet. `twcc_sequence` is the
    // extension value; `arrival_time_us` from the local monotonic clock.
    void on_packet(uint16_t twcc_sequence, int64_t arrival_time_us);

    // Builds one TWCC RTCP packet covering everything recorded since the last
    // call. Returns empty when nothing is pending. Deltas are clamped to the
    // reportable range; unrepresentable packets are marked not-received.
    std::vector<uint8_t> build_feedback(uint32_t sender_ssrc, uint32_t media_ssrc);

    size_t pending() const { return pending_.size(); }

private:
    struct Arrival {
        int64_t at_us = 0;
    };

    uint16_t last_emitted_sequence_ = 0;
    bool has_emitted_ = false;
    uint8_t feedback_packet_count_ = 0;
    std::unordered_map<uint16_t, Arrival> pending_;
};

// Delay-gradient send-side estimator (simplified GCC): trendline over
// packet-group delay deltas, threshold overuse detector, AIMD rate control.
// Fed by TWCC feedback; emits a pacing/encoder target in bits per second.
class GccEstimator {
public:
    struct Config {
        uint64_t min_bps = 300'000;
        uint64_t max_bps = 12'000'000;
        // Trend above this for longer than overuse_time_ms means overuse.
        double overuse_threshold_ms = 12.5;
        int64_t overuse_time_ms = 100;
        // Loss fraction above this applies the loss-based cap.
        double loss_high_water = 0.10;
        double loss_low_water = 0.02;
    };

    explicit GccEstimator(Config config, uint64_t initial_bps);

    // Feed one feedback-referenced packet. `send_time_us` may be -1 when the
    // send record was evicted (packet is then ignored for delay). Lost
    // packets (received=false) only count toward the loss rate.
    void on_packet(
        uint16_t sequence,
        bool received,
        int64_t send_time_us,
        int64_t arrival_time_us
    );

    uint64_t target_bps() const { return target_bps_; }

    // Test/diagnostic visibility.
    double trend_ms() const { return trend_ms_; }
    double loss_rate() const { return smoothed_loss_; }

private:
    struct Group {
        int64_t first_send_us = 0;
        int64_t last_send_us = 0;
        int64_t arrival_sum_us = 0;
        size_t received = 0;
        size_t packets = 0;
    };

    void close_group(int64_t now_us);
    double trendline_slope_ms() const;

    const Config config_;
    uint64_t target_bps_;

    Group open_group_{};
    bool has_open_group_ = false;
    bool has_prev_group_ = false;
    int64_t prev_arrival_mean_us_ = 0;
    int64_t prev_send_mean_us_ = 0;

    // Accumulated delay change (WebRTC trendline input): the window stores
    // the running sum so a sustained per-group delay growth shows up as a
    // positive regression slope.
    double accumulated_delay_ms_ = 0.0;
    static constexpr size_t kTrendlineWindow = 20;
    std::deque<double> trendline_;

    double trend_ms_ = 0.0;
    int64_t overuse_since_us_ = 0;
    bool overusing_ = false;
    int64_t last_decrease_us_ = 0;
    int64_t last_increase_us_ = 0;

    double smoothed_loss_ = 0.0;
    uint64_t feedback_packets_seen_ = 0;
    uint64_t feedback_packets_lost_ = 0;
};

} // namespace mello::transport
