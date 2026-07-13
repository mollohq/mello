#pragma once

#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <string>

namespace rtc {
class Track;
}

namespace mello::transport {

struct RtpVideoSenderConfig {
    uint32_t ssrc = 0;
    uint8_t payload_type = 96;
    std::string cname;
    uint64_t pacing_target_bps = 4'000'000;
};

struct RtpVideoSenderStats {
    uint64_t access_units_enqueued = 0;
    uint64_t access_units_sent = 0;
    uint64_t access_units_dropped = 0;
    uint64_t access_units_rejected = 0;
    uint64_t bytes_sent = 0;
    uint64_t send_failures = 0;
    uint64_t rtp_packets_sent = 0;
    uint64_t rtp_wire_bytes_sent = 0;
    uint64_t queued_access_units = 0;
    uint64_t peak_queued_access_units = 0;
    uint64_t queued_bytes = 0;
    uint64_t peak_queued_bytes = 0;
    uint64_t pacing_target_bps = 0;
    uint64_t current_pacing_delay_us = 0;
    uint64_t max_pacing_delay_us = 0;
    uint64_t local_idr_requests = 0;
    uint64_t pli_requests = 0;
    uint64_t remb_reports = 0;
    uint32_t latest_remb_bitrate_bps = 0;
};

// One producer thread may call send_access_unit(). It only copies accepted
// access units into a bounded queue; packetization and pacing run on a worker.
// libdatachannel may invoke feedback callbacks from its own RTCP threads. All
// callbacks supplied to this class are serialized with each other.
class RtpVideoSender final {
public:
    using PliCallback = std::function<void()>;
    using RembCallback = std::function<void(uint32_t bitrate_bps)>;
    using LocalIdrNeededCallback = std::function<void()>;

    // ~133 ms at 60 fps — absorbs VBR bursts while the pacing worker drains.
    static constexpr size_t kMaxQueuedAccessUnits = 8;
    static constexpr size_t kMaxQueuedBytes = 4 * 1024 * 1024;

    RtpVideoSender(
        std::shared_ptr<rtc::Track> track,
        RtpVideoSenderConfig config,
        PliCallback on_pli = {},
        RembCallback on_remb = {},
        LocalIdrNeededCallback on_local_idr_needed = {}
    ) noexcept;
    ~RtpVideoSender();

    RtpVideoSender(const RtpVideoSender&) = delete;
    RtpVideoSender& operator=(const RtpVideoSender&) = delete;
    RtpVideoSender(RtpVideoSender&&) noexcept;
    RtpVideoSender& operator=(RtpVideoSender&&) noexcept;

    // Each call must contain exactly one complete Annex-B H.264 access unit.
    // capture_timestamp_us is capture-clock time, not wall-clock time.
    bool send_access_unit(
        const uint8_t* annex_b,
        size_t size,
        uint64_t capture_timestamp_us
    ) noexcept;

    // Zero is invalid because it would stop a non-empty packet batch forever.
    bool set_pacing_target_bps(uint64_t bitrate_bps) noexcept;
    bool is_open() const noexcept;
    RtpVideoSenderStats stats() const noexcept;

private:
    struct State;
    std::shared_ptr<State> state_;
};

} // namespace mello::transport
