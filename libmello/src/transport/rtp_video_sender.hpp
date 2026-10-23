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

enum class SendAccessUnitResult {
    Accepted,
    Backpressure,
    Failed,
};

struct RtpVideoSenderConfig {
    uint32_t ssrc = 0;
    uint8_t payload_type = 96;
    std::string cname;
    // The manager ceiling: 1.08 x the encoder bitrate (pacer.rs). Without
    // goog_cc the pacer runs at 2.5 x this value.
    uint64_t pacing_target_bps = 4'000'000;
    // Transport-wide congestion control was negotiated for this leg: stamp
    // egress with TWCC sequence numbers and run goog_cc (GoogCcController)
    // from the feedback. goog_cc then sets the pacing rate (2.5 x its
    // target), the probes and the target reported as GCC feedback.
    bool twcc_enabled = false;
    // goog_cc limits. 0 picks a default: start = 60 % of the ceiling, max =
    // the ceiling. The QualityController sets them (set_bitrate_limits).
    uint64_t start_bps = 0;
    uint64_t min_bps = 300'000;
    uint64_t max_bps = 0;
    // RFC 4588 retransmission stream, when the answer negotiated it: its own
    // SSRC and payload type (apt = payload_type). 0 = not negotiated; then a
    // retransmit reuses the media SSRC and sequence number, and there is no
    // padding (probes use media only).
    uint32_t rtx_ssrc = 0;
    uint8_t rtx_payload_type = 0;
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
    // NACK retransmission path: requested by receivers, repaired from the
    // sender cache, or dropped (cache miss / queue overflow / older than the
    // last IDR).
    uint64_t rtx_requests = 0;
    uint64_t rtx_sent = 0;
    uint64_t rtx_cache_misses = 0;
    uint64_t rtx_queue_dropped = 0;
    uint64_t rtx_bytes_sent = 0;
    // TWCC: feedback reports processed and the goog_cc encoder target.
    uint64_t twcc_reports = 0;
    uint64_t gcc_target_bps = 0;
    // The rate the pacer runs at now (goog_cc pacing rate, or 2.5 x the
    // ceiling without goog_cc).
    uint64_t pacing_rate_bps = 0;
    uint64_t probe_clusters_started = 0;
    uint64_t probe_clusters_completed = 0;
    uint64_t padding_packets_sent = 0;
    // Feedback outage: the times media stopped after 1 s without feedback,
    // and whether it is stopped now.
    uint64_t send_pauses = 0;
    bool send_paused = false;
    // Time from the IDR access unit entering the sender to its last packet
    // on the wire.
    uint64_t idr_access_units_sent = 0;
    uint64_t last_idr_send_us = 0;
    uint64_t max_idr_send_us = 0;
    // IDRs that took more than 50 ms (the stage 4 gate's bound).
    uint64_t idr_access_units_over_50ms = 0;
    // Retransmit queue flushes on an IDR request (PLI or local IDR).
    uint64_t rtx_queue_flushes = 0;
    // Re-probes after an allocation raise (a rung step up).
    uint64_t reprobes = 0;
    // NACKed packets not resent: one resend per round trip per packet, and
    // retransmits at most at the target rate (libwebrtc's two rules).
    uint64_t rtx_suppressed = 0;
    // goog_cc's smoothed round trip from the TWCC reports; 0 unknown.
    uint64_t feedback_rtt_us = 0;
};

// One producer thread may call send_access_unit(). It only copies accepted
// access units into a bounded queue; packetization, pacing and goog_cc run on
// one worker thread. libdatachannel may invoke feedback callbacks from its own
// RTCP threads. All callbacks supplied to this class are serialized with each
// other.
class RtpVideoSender final {
public:
    using PliCallback = std::function<void()>;
    using RembCallback = std::function<void(uint32_t bitrate_bps)>;
    using LocalIdrNeededCallback = std::function<void()>;
    using GccTargetCallback = std::function<void(uint32_t bitrate_bps)>;

    // ~266 ms at 60 fps — absorbs VBR bursts while the pacing worker drains.
    static constexpr size_t kMaxQueuedAccessUnits = 16;
    static constexpr size_t kMaxQueuedBytes = 4 * 1024 * 1024;

    RtpVideoSender(
        std::shared_ptr<rtc::Track> track,
        RtpVideoSenderConfig config,
        PliCallback on_pli = {},
        RembCallback on_remb = {},
        LocalIdrNeededCallback on_local_idr_needed = {},
        GccTargetCallback on_gcc_target = {}
    ) noexcept;
    ~RtpVideoSender();

    RtpVideoSender(const RtpVideoSender&) = delete;
    RtpVideoSender& operator=(const RtpVideoSender&) = delete;
    RtpVideoSender(RtpVideoSender&&) noexcept;
    RtpVideoSender& operator=(RtpVideoSender&&) noexcept;

    // Each call must contain exactly one complete Annex-B H.264 access unit.
    // capture_timestamp_us is capture-clock time, not wall-clock time.
    SendAccessUnitResult send_access_unit(
        const uint8_t* annex_b,
        size_t size,
        uint64_t capture_timestamp_us
    ) noexcept;

    // Zero is invalid because it would stop a non-empty packet batch forever.
    bool set_pacing_target_bps(uint64_t bitrate_bps) noexcept;
    // goog_cc limits (OnTargetRateConstraints). Applied on the worker.
    // `start_bps` (0 = keep) restarts goog_cc at it while no feedback has
    // come yet: the sender can open before the stream manager sends its
    // limits, and its default start (60 % of the default ceiling) would
    // else set the start probes and the first encoder target.
    bool set_bitrate_limits(uint64_t min_bps, uint64_t start_bps, uint64_t max_bps) noexcept;
    // The encoder allocation (OnStreamsConfig max_total_allocated_bitrate).
    // A value over the previous one is a rung step up: the sender re-probes
    // (a route change at the current target, then the initial probes).
    bool set_max_allocated_bps(uint64_t bitrate_bps) noexcept;
    bool is_open() const noexcept;
    RtpVideoSenderStats stats() const noexcept;

private:
    struct State;
    std::shared_ptr<State> state_;
};

} // namespace mello::transport
