#pragma once

#include "rtp_h264_receiver.hpp"

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <optional>
#include <vector>

namespace rtc {
class Track;
}

namespace mello::transport {

namespace detail {

struct GenericNackBlock {
    uint16_t pid = 0;
    uint16_t blp = 0;
};

std::vector<GenericNackBlock> compress_generic_nack_sequences(
    const std::vector<uint16_t>& sequences
);

std::vector<uint8_t> make_generic_nack_packet(
    uint32_t sender_ssrc,
    uint32_t media_ssrc,
    const std::vector<uint16_t>& sequences
);

std::vector<uint8_t> make_pli_packet(
    uint32_t sender_ssrc,
    uint32_t media_ssrc
);

} // namespace detail

struct RtpVideoReceiverAccessUnit {
    std::vector<uint8_t> annex_b;
    bool is_idr = false;
    uint32_t rtp_timestamp = 0;
};

enum class RtpVideoReceiverPopResult {
    Empty,
    BufferTooSmall,
    Ok,
};

struct RtpVideoReceiverSessionConfig {
    uint8_t payload_type = 96;
    // Zero generates a non-zero local feedback SSRC.
    uint32_t local_feedback_ssrc = 0;
    std::chrono::milliseconds receiver_report_interval{1000};
    // TWCC was negotiated on this leg: record transport-wide arrival times
    // and emit TWCC feedback reports (~50 ms cadence).
    bool twcc_enabled = false;
};

struct RtpVideoReceiverSessionStats {
    uint64_t ingress_packets = 0;
    uint64_t ingress_bytes = 0;
    uint64_t ingress_dropped_packets = 0;
    uint64_t ingress_dropped_bytes = 0;
    uint64_t ingress_overflows = 0;
    uint64_t ingress_queued_packets = 0;
    uint64_t ingress_queued_bytes = 0;
    uint64_t peak_ingress_queued_packets = 0;
    uint64_t peak_ingress_queued_bytes = 0;
    uint64_t wrong_ssrc_packets_after_recovery = 0;

    uint64_t access_units_queued_total = 0;
    uint64_t access_unit_bytes_queued_total = 0;
    uint64_t access_units_dropped = 0;
    uint64_t access_unit_bytes_dropped = 0;
    uint64_t output_queued_access_units = 0;
    uint64_t output_queued_bytes = 0;
    uint64_t peak_output_queued_access_units = 0;
    uint64_t peak_output_queued_bytes = 0;

    uint64_t nack_packets_sent = 0;
    uint64_t nack_sequences_sent = 0;
    uint64_t pli_requests = 0;
    uint64_t pli_packets_sent = 0;
    uint64_t remb_packets_sent = 0;
    uint64_t twcc_packets_sent = 0;
    uint64_t receiver_reports_sent = 0;
    uint64_t sender_reports_received = 0;
    uint64_t invalid_rtcp_packets = 0;
    uint64_t feedback_send_failures = 0;
    uint64_t core_restarts = 0;

    uint32_t payload_type = 0;
    uint32_t local_feedback_ssrc = 0;
    uint32_t remote_media_ssrc = 0;
    uint32_t receive_target_bps = 0;
    bool has_remote_media_ssrc = false;
    bool awaiting_output_idr = true;

    RtpH264Receiver::Stats core;
};

// The libdatachannel callback only classifies and copies datagrams. One worker
// owns the H.264 receiver, RTCP state, and all outbound feedback. Consumers poll
// complete access units; no user callback runs on either producer thread.
class RtpVideoReceiverSession final {
public:
    static constexpr size_t kMaxIngressPackets = 512;
    static constexpr size_t kMaxIngressBytes = 2 * 1024 * 1024;
    static constexpr size_t kMaxOutputAccessUnits = 3;
    static constexpr size_t kMaxOutputBytes = 4 * 1024 * 1024;

    explicit RtpVideoReceiverSession(
        std::shared_ptr<rtc::Track> track,
        uint8_t payload_type = 96
    ) noexcept;
    RtpVideoReceiverSession(
        std::shared_ptr<rtc::Track> track,
        RtpVideoReceiverSessionConfig config
    ) noexcept;
    ~RtpVideoReceiverSession();

    RtpVideoReceiverSession(const RtpVideoReceiverSession&) = delete;
    RtpVideoReceiverSession& operator=(const RtpVideoReceiverSession&) = delete;
    RtpVideoReceiverSession(RtpVideoReceiverSession&&) noexcept;
    RtpVideoReceiverSession& operator=(RtpVideoReceiverSession&&) noexcept;

    std::optional<RtpVideoReceiverAccessUnit> pop_access_unit() noexcept;

    // On BufferTooSmall, size is the required byte count and the queue head is
    // retained. Metadata describes that retained access unit.
    RtpVideoReceiverPopResult pop_access_unit(
        uint8_t* buffer,
        size_t capacity,
        size_t& size,
        bool& is_idr,
        uint32_t& rtp_timestamp
    ) noexcept;

    // Queues a REMB update for the worker. The latest target is retained until
    // a remote SSRC and libdatachannel feedback callback are available.
    bool set_receive_target(uint32_t bitrate_bps) noexcept;

    bool is_open() const noexcept;
    RtpVideoReceiverSessionStats stats() const noexcept;

private:
    struct State;
    std::shared_ptr<State> state_;
};

} // namespace mello::transport
