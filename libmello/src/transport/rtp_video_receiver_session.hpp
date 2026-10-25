#pragma once

#include "av_sync.hpp"
#include "rtp_h264_receiver.hpp"
#include "video_receive_timeline.hpp"

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
    // ULPFEC (XOR parity, ulpfec/90000 PT 127) was negotiated: buffer parity
    // packets arriving on the media SSRC + 1 and reconstruct single losses
    // per group before NACK repair would be needed.
    bool fec_enabled = false;
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

    // Access units inserted into the timeline, and refused by it.
    uint64_t access_units_queued_total = 0;
    uint64_t access_unit_bytes_queued_total = 0;
    uint64_t access_units_dropped = 0;
    uint64_t access_unit_bytes_dropped = 0;
    // Frames the timeline holds (the C ABI kept the old output queue names).
    uint64_t output_queued_access_units = 0;
    uint64_t output_queued_bytes = 0;
    uint64_t peak_output_queued_access_units = 0;
    uint64_t peak_output_queued_bytes = 0;
    // Key-frame requests not sent: a key frame was arriving already.
    uint64_t keyframe_requests_absorbed = 0;

    uint64_t nack_packets_sent = 0;
    uint64_t nack_sequences_sent = 0;
    uint64_t pli_requests = 0;
    uint64_t pli_packets_sent = 0;
    uint64_t remb_packets_sent = 0;
    uint64_t twcc_packets_sent = 0;
    // ULPFEC repair: media packets reconstructed from parity (recovered) and
    // recovery attempts that failed (no covering block yet, or 2+ losses in
    // the group — those fall through to NACK as before).
    uint64_t rx_fec_recovered = 0;
    uint64_t rx_fec_unrecoverable = 0;
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
    // The timeline waits for a key frame.
    bool awaiting_output_idr = true;

    RtpH264Receiver::Stats core;
    TimelineStats timeline;
};

// The libdatachannel callback only classifies and copies datagrams. One worker
// owns the H.264 receiver, RTCP state, and all outbound feedback. Complete
// access units go into the viewer timeline (one per session); the viewer
// pipeline polls the timeline on its decode thread (streaming-quality stage
// 3, spec 12 section 7). A PLI goes out only for the timeline's
// KeyframeNeeded, with a 1 s cooldown.
class RtpVideoReceiverSession final {
public:
    static constexpr size_t kMaxIngressPackets = 2048;
    static constexpr size_t kMaxIngressBytes = 4 * 1024 * 1024;
    // Largest access unit the pull API hands out.
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

    // The pull API of the tests and tools: the next frame the timeline
    // releases now, without a render clock. The viewer pipeline does not use
    // it; it polls timeline() itself.
    std::optional<RtpVideoReceiverAccessUnit> pop_access_unit() noexcept;

    // On BufferTooSmall, size is the required byte count and the frame is
    // retained for the next call. Metadata describes that retained frame.
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

    // The measured control-channel RTT. Applies at once to the NACK
    // interval, the repair deadlines and the timeline.
    void set_rtt_hint(float rtt_ms) noexcept;

    // The viewer timeline of this session. Null only for a moved-from
    // session.
    std::shared_ptr<VideoReceiveTimeline> timeline() const noexcept;

    // Ask the host for a key frame (the timeline's KeyframeNeeded). Any
    // thread; the worker sends the PLI.
    void request_keyframe() noexcept;

    bool is_open() const noexcept;
    RtpVideoReceiverSessionStats stats() const noexcept;

    // ---- streaming-quality stage 7 ----
    // The A/V sync input of this viewer (av_sync.hpp). The worker runs it
    // every 20 ms (it decides once per its own interval) and it sets the
    // timeline's minimum delay. Null: no sync. Any thread.
    void set_av_sync(std::shared_ptr<AvSyncInput> input) noexcept;
    // ---- end stage 7 ----

private:
    struct State;
    std::shared_ptr<State> state_;
};

} // namespace mello::transport
