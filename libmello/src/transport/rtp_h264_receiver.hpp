#pragma once

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <optional>
#include <type_traits>
#include <vector>

namespace mello::transport {

// Receives one H.264 RTP stream and emits complete Annex-B access units.
//
// Streaming-quality stage 3 (spec 12 section 7.2). The receiver repairs and
// assembles; it does not decide when a frame is shown and it never asks for a
// key frame on its own. The viewer timeline (VideoReceiveTimeline) does both.
//
// - An access unit goes out as soon as it is complete, also before an older
//   one that still waits for a repair. The timeline puts them in order.
// - Each access unit carries its frame id (the sequence number of its last
//   packet) and the id it decodes from (the last packet before it), so the
//   timeline knows which frames a missing one blocks.
// - A missing packet is NACKed every max(RTT, 10 ms) until its repair
//   deadline: the render time of the first frame after the gap plus one RTT.
//   While packets of that frame still arrive (a large key frame on a slow
//   link), the deadline moves with them. A packet still missing after its
//   deadline is lost for real: `on_loss` runs once and the access units that
//   needed it are dropped.
// - No window of a fixed number of access units, no stall timer and no hard
//   age. The packet memory has bounds (kMaxPackets, kMaxBufferedBytes) and
//   forgets the packets of an access unit when the stream is silent for
//   kPacketMemory.
//
// Thread contract: single-thread-affine. Construction, packet delivery,
// tick(), stats() and destruction run on one externally serialized thread.
// Callbacks run synchronously on that thread; they must not throw or re-enter
// this receiver.
class RtpH264Receiver {
public:
    using Clock = std::chrono::steady_clock;
    using TimePoint = Clock::time_point;

    struct AccessUnit {
        std::vector<uint8_t> annex_b;
        uint32_t rtp_timestamp = 0;
        /// Unwrapped sequence numbers of the first and the last packet. The
        /// last one is the frame id.
        int64_t first_sequence = 0;
        int64_t last_sequence = 0;
        /// The frame id this access unit decodes from: the last media packet
        /// before it. -1 for a key frame.
        int64_t reference = -1;
        TimePoint first_arrival{};
        TimePoint last_arrival{};
        /// An IDR with its SPS and PPS: decodes on its own.
        bool keyframe = false;
        bool has_idr = false;
        /// A NACK repair delivered at least one of its packets.
        bool retransmitted = false;
    };

    /// Takes the whole AccessUnit, or (for the sender's loopback tests) the
    /// older form `(annex_b, is_idr, rtp_timestamp)`.
    class AccessUnitCallback {
    public:
        AccessUnitCallback() = default;
        template <typename F,
                  typename = std::enable_if_t<std::is_invocable_v<F&, AccessUnit&&>>>
        AccessUnitCallback(F f) : fn_(std::move(f)) {}
        template <typename F, typename = void,
                  typename = std::enable_if_t<
                      !std::is_invocable_v<F&, AccessUnit&&> &&
                      std::is_invocable_v<F&, const std::vector<uint8_t>&, bool, uint32_t>>>
        AccessUnitCallback(F f)
            : fn_([g = std::move(f)](AccessUnit&& unit) mutable {
                  g(unit.annex_b, unit.has_idr, unit.rtp_timestamp);
              }) {}
        explicit operator bool() const { return static_cast<bool>(fn_); }
        void operator()(AccessUnit&& unit) const { fn_(std::move(unit)); }

    private:
        std::function<void(AccessUnit&&)> fn_;
    };
    using NackCallback =
        std::function<void(const std::vector<uint16_t>& missing_sequences)>;
    /// A packet missed its repair deadline: a reference is gone for real.
    using LossCallback = std::function<void(int64_t lost_sequence)>;
    /// The render time of a frame with this RTP timestamp, empty while the
    /// timeline knows no frame yet.
    using RenderTimeFn =
        std::function<std::optional<TimePoint>(uint32_t rtp_timestamp)>;

    struct Callbacks {
        AccessUnitCallback on_access_unit;
        NackCallback on_nack;
        LossCallback on_loss;
        RenderTimeFn render_time;
    };

    struct Config {
        uint8_t payload_type = 96;
        /// Round trip to the SFU, until set_rtt() gives a measured one.
        std::chrono::microseconds rtt{30'000};
    };

    struct Stats {
        uint64_t packets = 0;
        // Full RTP datagram bytes passed to on_rtp_packet(), including rejects.
        uint64_t bytes_received = 0;
        uint64_t accepted_packets = 0;
        uint64_t accepted_bytes = 0;
        uint64_t padding_packets = 0;
        uint64_t duplicates = 0;
        // After its access unit was given up (lost, or older than the last
        // key frame).
        uint64_t late_packets = 0;
        uint64_t invalid_rtp_packets = 0;
        uint64_t invalid_h264_packets = 0;
        uint64_t wrong_payload_type_packets = 0;
        uint64_t wrong_ssrc_packets = 0;
        uint64_t backwards_time_inputs = 0;

        uint64_t missing_sequences_detected = 0;
        uint64_t repaired_packets = 0;
        // Packets that missed their repair deadline.
        uint64_t lost_packets = 0;
        // nacks counts individual sequence numbers requested, including retries.
        uint64_t nacks = 0;
        uint64_t nack_callbacks = 0;
        uint64_t complete_access_units = 0;
        // Access units given up: a packet was lost, or a key frame made them
        // obsolete, or the packet memory forgot them.
        uint64_t incomplete_access_units = 0;
        uint64_t emitted_access_units = 0;
        // Emitted out of order: before an older access unit that was still
        // incomplete.
        uint64_t out_of_order_access_units = 0;
        // on_loss calls (one per loss event).
        uint64_t loss_events = 0;

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

        // Filled by the receiver session from the timeline, for the C ABI
        // fields that described the old IDR gate.
        uint64_t pli_requests = 0;
        uint64_t gate_dropped_access_units = 0;
        uint64_t gate_entries = 0;
        uint64_t gate_exits = 0;
        bool gated = false;
    };

    /// Bounds of the packet memory (not yet emitted media packets).
    inline static constexpr size_t kMaxPackets = 4096;
    inline static constexpr size_t kMaxBufferedBytes = 8 * 1024 * 1024;
    /// A gap this large is a discontinuity: the receiver gives up everything
    /// before it instead of NACKing it.
    inline static constexpr int64_t kMaxGap = 2048;
    /// Sequence numbers remembered for duplicate and late detection.
    inline static constexpr int64_t kHistory = 8192;
    inline static constexpr std::chrono::milliseconds kReorderGrace{3};
    inline static constexpr size_t kNewerPacketGrace = 2;
    /// Shortest NACK repeat interval; the repeat is max(RTT, this).
    inline static constexpr std::chrono::milliseconds kMinNackRepeat{10};
    /// Room for a retransmission after the last packet of the frame that
    /// follows a gap.
    inline static constexpr std::chrono::milliseconds kRepairMargin{10};
    /// Repair window when the timeline knows no render time yet.
    inline static constexpr std::chrono::milliseconds kDefaultRepairWindow{150};
    /// Packet memory: when no packet arrives for this long, incomplete access
    /// units are forgotten. Memory only; it never asks for a key frame.
    inline static constexpr std::chrono::milliseconds kPacketMemory{600};
    /// A key frame counts as arriving while its last packet is this recent.
    inline static constexpr std::chrono::milliseconds kKeyframeProgress{200};

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

    // Drives reorder grace, NACK repetition and repair deadlines when no RTP
    // packet arrives.
    void tick(TimePoint now);

    /// The measured round trip. Applies at once to the NACK interval and the
    /// repair deadlines.
    void set_rtt(std::chrono::microseconds rtt);

    /// True while a key frame is arriving: an access unit that starts with an
    /// SPS or an IDR slice is incomplete and its last packet is recent.
    bool keyframe_in_progress(TimePoint now) const;

    Stats stats() const;

    /// Cheap counter for the per-packet SSRC bookkeeping of the session.
    uint64_t accepted_packets() const;

private:
    class Impl;
    std::unique_ptr<Impl> impl_;
};

/// True when an H.264 RTP payload can only be the first packet of an access
/// unit: an AUD, SEI or SPS NAL, or the slice that starts at macroblock 0
/// (single NAL, STAP-A or the first FU-A fragment). Exposed for tests.
bool h264_payload_starts_access_unit(const uint8_t* payload, size_t size);

} // namespace mello::transport
