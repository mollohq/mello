#pragma once

#include <cstddef>
#include <cstdint>
#include <deque>
#include <memory>
#include <unordered_map>
#include <vector>

namespace mello::transport {

// Transport-Wide Congestion Control (TWCC, RFC 8888) building blocks for the
// native RTP stream path:
//
// - TwccSendStamper: stamps transport-wide sequence numbers into egress RTP
//   (header extension, one-byte-header profile 0xBEDE).
// - parse_twcc_feedback: parses TWCC RTCP feedback (PT=205, FMT=15).
// - TwccFeedbackGenerator: receiver-side feedback report builder.
// - GoogCcController: WebRTC goog_cc fed by the send history and the parsed
//   feedback.

inline constexpr char kTwccExtensionUri[] =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";
inline constexpr int kTwccExtensionId = 3;

class TwccSendStamper {
public:
    // Stamps `packet` (marshaled RTP bytes) with the next transport-wide
    // sequence number and returns it in `sequence`. Re-stamping a packet
    // that already carries our element (retransmit) overwrites the sequence
    // in place. Returns false for non-RTP input. The send history lives in
    // GoogCcController. Pacing worker only.
    bool stamp(std::vector<uint8_t>& packet, uint16_t& sequence);

    // The sequence the next stamp() assigns.
    uint16_t next_sequence() const { return next_sequence_; }

private:
    uint16_t next_sequence_ = 0;
};

struct TwccPacketResult {
    uint16_t sequence = 0;
    bool received = false;
    // Valid only when received: arrival time in microseconds on the
    // receiver's clock, the reference time plus the deltas (250 us units).
    int64_t arrival_time_us = 0;
};

struct TwccFeedback {
    uint32_t sender_ssrc = 0;
    uint32_t media_ssrc = 0;
    // 24-bit reference time in 64 ms units, as on the wire. The arrival
    // times below start from it; they wrap with it after about 12 days.
    uint32_t reference_time_64ms = 0;
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

    // Builds one TWCC RTCP packet (transport-cc draft section 3.1) that
    // starts after the last reported sequence. It covers every pending
    // packet unless the packet would grow over about 1.1 kB or a delta does
    // not fit in 16 bits; the rest stays pending for the next call. Small
    // deltas are unsigned 8 bits, large deltas signed 16 bits, never
    // clamped. Returns empty when nothing is pending.
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

// One probe cluster that goog_cc asks the pacer to send: at least
// `min_probes` packets and `min_bytes` bytes at `rate_bps`.
struct SendProbeCluster {
    int id = -1;
    uint64_t rate_bps = 0;
    int min_probes = 0;
    uint64_t min_bytes = 0;
};

// One packet as the pacer sent it.
struct SentPacketInfo {
    uint16_t transport_sequence = 0;
    int64_t send_time_us = 0;
    // RTP bytes after the TWCC stamp. The controller adds the IP, UDP and
    // SRTP overhead.
    size_t rtp_bytes = 0;
    // -1 when the packet is not part of a probe cluster.
    int probe_cluster_id = -1;
    int probe_min_probes = -1;
    int probe_min_bytes = -1;
};

// The send-side congestion controller of the stream sender: WebRTC goog_cc
// (M131, third_party/webrtc-goog-cc) behind its NetworkControllerInterface.
// It keeps the send history (60 s, or 32768 packets, whichever is less),
// turns TWCC feedback into TransportPacketsFeedback, and gives the target
// rate, the pacing rate, the padding rate and the probe clusters.
// Configuration: feedback_only (loss and round trip from TWCC), loss-based
// v2 and the probe controller at their upstream defaults.
//
// Thread model: not thread safe. The pacing worker of RtpVideoSender owns it
// and makes every call. Built only with MELLO_WEBRTC_GOOG_CC; without it
// available() is false and the sender paces at its ceiling.
class GoogCcController {
public:
    struct Config {
        uint64_t start_bps = 0;
        uint64_t min_bps = 300'000;
        uint64_t max_bps = 0;
    };

    // True when this build of libmello carries goog_cc.
    static bool available() noexcept;

    GoogCcController(Config config, int64_t now_us);
    ~GoogCcController();
    GoogCcController(const GoogCcController&) = delete;
    GoogCcController& operator=(const GoogCcController&) = delete;

    void on_packet_sent(const SentPacketInfo& packet);
    // One parsed report. `now_us` is the local time it arrived. Returns the
    // number of packets of the report that matched the send history.
    size_t on_feedback(const TwccFeedback& feedback, int64_t now_us);
    // Every 25 ms. `pacer_queue_bytes`: media waiting in the pacer.
    void on_process_interval(int64_t now_us, uint64_t pacer_queue_bytes);
    // After a feedback outage: libwebrtc's route change. The estimators
    // restart from the current target and the initial probes run again.
    void on_route_change(int64_t now_us);
    void set_bitrate_limits(int64_t now_us, uint64_t min_bps, uint64_t max_bps);
    // The QualityController's allocation. A higher value than before makes
    // goog_cc probe up to it (ProbeController::OnMaxTotalAllocatedBitrate).
    void set_max_total_allocated(int64_t now_us, uint64_t bps);

    uint64_t target_bps() const noexcept { return target_bps_; }
    // The rate for the encoder: the target less the congestion window
    // pushback.
    uint64_t encoder_target_bps() const noexcept;
    uint64_t pacing_bps() const noexcept { return pacing_bps_; }
    uint64_t padding_bps() const noexcept { return padding_bps_; }
    uint64_t bytes_in_flight() const noexcept { return in_flight_bytes_; }
    std::vector<SendProbeCluster> take_probe_clusters();

private:
    struct Impl;

    std::unique_ptr<Impl> impl_;
    uint64_t target_bps_ = 0;
    double cwnd_reduce_ratio_ = 0.0;
    uint64_t pacing_bps_ = 0;
    uint64_t padding_bps_ = 0;
    uint64_t in_flight_bytes_ = 0;
    std::vector<SendProbeCluster> probes_;
};

} // namespace mello::transport
