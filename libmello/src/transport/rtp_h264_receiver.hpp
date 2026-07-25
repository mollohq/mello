#pragma once

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <vector>

namespace mello::transport {

// Receives one H.264 RTP stream and emits complete Annex-B access units.
//
// Thread contract: this class is single-thread-affine. Construction, packet
// delivery, tick(), stats(), and destruction must all happen on the same
// externally-serialized thread. Callbacks run synchronously on that thread;
// they must not throw or re-enter this receiver. Callback byte/vector views
// are valid only for the duration of the callback.
class RtpH264Receiver {
public:
    using Clock = std::chrono::steady_clock;
    using TimePoint = Clock::time_point;

    using AccessUnitCallback =
        std::function<void(const std::vector<uint8_t>& annex_b,
                           bool is_idr,
                           uint32_t rtp_timestamp)>;
    using NackCallback =
        std::function<void(const std::vector<uint16_t>& missing_sequences)>;
    using PliCallback = std::function<void()>;

    struct Callbacks {
        AccessUnitCallback on_access_unit;
        NackCallback on_nack;
        PliCallback on_pli;
    };

    struct Config {
        uint8_t payload_type = 96;
        std::chrono::milliseconds pli_cooldown{1000};
        // NACK retry budget per missing sequence. Zero selects the static
        // default (kMaxNackAttempts); the session sets this from the
        // measured RTT so high-RTT links get more repair chances within the
        // AU stall deadline.
        size_t nack_max_attempts = 0;
    };

    struct Stats {
        uint64_t packets = 0;
        // Full RTP datagram bytes passed to on_rtp_packet(), including rejects.
        uint64_t bytes_received = 0;
        uint64_t accepted_packets = 0;
        uint64_t accepted_bytes = 0;
        uint64_t duplicates = 0;
        uint64_t late_packets = 0;
        uint64_t invalid_rtp_packets = 0;
        uint64_t invalid_h264_packets = 0;
        uint64_t wrong_payload_type_packets = 0;
        uint64_t wrong_ssrc_packets = 0;
        uint64_t backwards_time_inputs = 0;

        uint64_t missing_sequences_detected = 0;
        uint64_t repaired_packets = 0;
        // nacks counts individual sequence numbers requested, including retries.
        uint64_t nacks = 0;
        uint64_t nack_callbacks = 0;
        uint64_t complete_access_units = 0;
        uint64_t incomplete_access_units = 0;
        uint64_t emitted_access_units = 0;
        uint64_t pli_requests = 0;

        uint64_t gate_dropped_access_units = 0;
        uint64_t gate_entries = 1; // The receiver starts gated.
        uint64_t gate_exits = 0;
        uint64_t buffer_evictions = 0;
        uint64_t sequence_discontinuities = 0;

        size_t buffered_access_units = 0;
        size_t buffered_packets = 0;
        size_t buffered_bytes = 0;
        size_t peak_buffered_access_units = 0;
        size_t peak_buffered_packets = 0;
        size_t peak_buffered_bytes = 0;
        bool has_ssrc = false;
        uint32_t ssrc = 0;
        uint64_t extended_highest_sequence = 0;
        int64_t cumulative_loss = 0;
        // RFC 3550 inter-arrival jitter in 90 kHz RTP timestamp ticks.
        uint32_t interarrival_jitter = 0;
        bool gated = true;
    };

    inline static constexpr size_t kMaxAccessUnits = 3;
    inline static constexpr size_t kMaxPackets = 256;
    inline static constexpr size_t kMaxBufferedBytes = 1024 * 1024;
    inline static constexpr std::chrono::milliseconds kReorderGrace{3};
    inline static constexpr size_t kNewerPacketGrace = 2;
    inline static constexpr std::chrono::milliseconds kNackRepeat{15};
    inline static constexpr size_t kMaxNackAttempts = 2;
    // Host RTP pacing and SFU per-viewer relay queues can spread one AU's
    // fragments over tens of ms; 45ms was too tight for local assembly.
    // Applied to the time since the AU's last fragment (stall deadline):
    // paced AUs may legitimately span longer than 120 ms end-to-end, but
    // only while fragments keep arriving.
    inline static constexpr std::chrono::milliseconds kAccessUnitDeadline{120};
    // Hard cap on total AU age regardless of progress. At 1.5 Mbps a 100 KB
    // IDR takes ~530 ms on the wire; an AU older than this can never become
    // useful and must be dropped so the gate/PLI recovery can proceed.
    inline static constexpr std::chrono::milliseconds kAccessUnitHardDeadline{600};

    RtpH264Receiver();
    explicit RtpH264Receiver(Callbacks callbacks);
    RtpH264Receiver(Callbacks callbacks, const Config& config);
    ~RtpH264Receiver();

    RtpH264Receiver(const RtpH264Receiver&) = delete;
    RtpH264Receiver& operator=(const RtpH264Receiver&) = delete;
    RtpH264Receiver(RtpH264Receiver&&) = delete;
    RtpH264Receiver& operator=(RtpH264Receiver&&) = delete;

    // now must come from one monotonic timeline. Backwards values are counted
    // and ignored without advancing receiver state or deadlines.
    void on_rtp_packet(const uint8_t* data, size_t size, TimePoint now);

    // Drives reorder grace, NACK repetition, AU deadlines, and PLI cooldown
    // deterministically when no RTP packet arrives.
    void tick(TimePoint now);

    Stats stats() const;
    bool gated() const;

private:
    class Impl;
    std::unique_ptr<Impl> impl_;
};

} // namespace mello::transport
