#include "rtp_video_receiver_session.hpp"

#include "twcc.hpp"
#include "ulpfec.hpp"
#include "video_receive_timeline.hpp"
#include "../util/log.hpp"

#include <rtc/mediahandler.hpp>
#include <rtc/message.hpp>
#include <rtc/rtp.hpp>
#include <rtc/track.hpp>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstring>
#include <deque>
#include <functional>
#include <limits>
#include <mutex>
#include <random>
#include <string>
#include <thread>
#include <utility>

namespace mello::transport {
namespace {

using SteadyClock = std::chrono::steady_clock;

constexpr auto kReceiverTickInterval = std::chrono::milliseconds(1);
// Stage 7: how often the worker runs the A/V sync input.
constexpr auto kAvSyncTickInterval = std::chrono::milliseconds(20);
// Core stats are copied for other threads at this cadence, not every tick.
constexpr auto kStatsPublishInterval = std::chrono::milliseconds(20);
constexpr auto kPliCooldown = std::chrono::milliseconds(1000);
// TWCC feedback cadence: ~50 ms keeps the estimator's delay signal fresh
// without measurable RTCP overhead (~42 packets per report at 830 pps).
constexpr auto kTwccReportInterval = std::chrono::milliseconds(50);
constexpr uint32_t kRtcpUnitsPerSecond = 65'536;
constexpr int64_t kMinSigned24 = -8'388'608;
constexpr int64_t kMaxSigned24 = 8'388'607;

uint16_t read_u16_be(const uint8_t* data) noexcept {
    return static_cast<uint16_t>(
        (static_cast<uint16_t>(data[0]) << 8)
        | static_cast<uint16_t>(data[1])
    );
}

uint32_t read_u32_be(const uint8_t* data) noexcept {
    return (static_cast<uint32_t>(data[0]) << 24)
        | (static_cast<uint32_t>(data[1]) << 16)
        | (static_cast<uint32_t>(data[2]) << 8)
        | static_cast<uint32_t>(data[3]);
}

uint64_t read_u64_be(const uint8_t* data) noexcept {
    return (static_cast<uint64_t>(read_u32_be(data)) << 32)
        | read_u32_be(data + 4);
}

bool is_receive_direction(rtc::Description::Direction direction) noexcept {
    return direction == rtc::Description::Direction::RecvOnly
        || direction == rtc::Description::Direction::SendRecv;
}

bool is_h264_format(const std::string& format) noexcept {
    return format.size() == 4
        && (format[0] == 'H' || format[0] == 'h')
        && format[1] == '2'
        && format[2] == '6'
        && format[3] == '4';
}

bool is_valid_receiver_track(const rtc::Track& track, uint8_t payload_type) {
    const auto description = track.description();
    if (description.type() != "video"
        || !is_receive_direction(description.direction())
        || !description.hasPayloadType(payload_type)) {
        return false;
    }

    const auto* const rtp_map = description.rtpMap(payload_type);
    return rtp_map != nullptr && is_h264_format(rtp_map->format);
}

bool is_valid_rtcp_compound(const uint8_t* data, size_t size) noexcept {
    if (data == nullptr || size < 4 || (size % 4) != 0) {
        return false;
    }

    size_t offset = 0;
    while (offset < size) {
        if (size - offset < 4 || (data[offset] >> 6) != 2) {
            return false;
        }

        const uint8_t packet_type = data[offset + 1];
        if (packet_type < 192 || packet_type > 223) {
            return false;
        }

        const size_t packet_size =
            (static_cast<size_t>(read_u16_be(data + offset + 2)) + 1) * 4;
        if (packet_size < 4 || packet_size > size - offset) {
            return false;
        }

        const bool has_padding = (data[offset] & 0x20) != 0;
        if (has_padding) {
            if (offset + packet_size != size) {
                return false;
            }
            const size_t padding = data[offset + packet_size - 1];
            if (padding == 0 || padding > packet_size - 4) {
                return false;
            }
        }

        offset += packet_size;
    }
    return offset == size;
}

uint32_t generate_feedback_ssrc() noexcept {
    try {
        std::random_device random;
        uint32_t value = static_cast<uint32_t>(random());
        value ^= static_cast<uint32_t>(random()) << 16;
        if (value != 0) {
            return value;
        }
    } catch (...) {
    }

    static std::atomic<uint32_t> fallback{0x4d454c4f};
    uint32_t value = fallback.fetch_add(0x9e3779b9, std::memory_order_relaxed);
    if (value == 0) {
        value = fallback.fetch_add(1, std::memory_order_relaxed);
    }
    return value == 0 ? 1 : value;
}

void update_max(std::atomic<uint64_t>& destination, uint64_t value) noexcept {
    uint64_t current = destination.load(std::memory_order_relaxed);
    while (current < value
           && !destination.compare_exchange_weak(
               current,
               value,
               std::memory_order_relaxed,
               std::memory_order_relaxed)) {
    }
}

class ReceiverMediaHandler final : public rtc::MediaHandler {
public:
    using IncomingCallback = std::function<void(
        rtc::message_vector&,
        const rtc::message_callback&
    )>;

    explicit ReceiverMediaHandler(IncomingCallback callback)
        : callback_(std::move(callback)) {}

    void incoming(
        rtc::message_vector& messages,
        const rtc::message_callback& send
    ) override {
        callback_(messages, send);
    }

private:
    IncomingCallback callback_;
};

} // namespace

namespace detail {

std::vector<GenericNackBlock> compress_generic_nack_sequences(
    const std::vector<uint16_t>& sequences
) {
    if (sequences.empty()) {
        return {};
    }

    std::vector<uint16_t> ordered = sequences;
    std::sort(ordered.begin(), ordered.end());
    ordered.erase(std::unique(ordered.begin(), ordered.end()), ordered.end());

    size_t start = 0;
    if (ordered.size() > 1) {
        uint32_t largest_gap = 0;
        for (size_t index = 0; index < ordered.size(); ++index) {
            const uint16_t current = ordered[index];
            const uint16_t next = ordered[(index + 1) % ordered.size()];
            const uint32_t gap = index + 1 < ordered.size()
                ? static_cast<uint32_t>(next) - current
                : static_cast<uint32_t>(next) + 65'536U - current;
            if (gap > largest_gap) {
                largest_gap = gap;
                start = (index + 1) % ordered.size();
            }
        }
    }

    std::vector<GenericNackBlock> blocks;
    blocks.reserve(ordered.size());
    for (size_t offset = 0; offset < ordered.size(); ++offset) {
        const uint16_t sequence = ordered[(start + offset) % ordered.size()];
        if (blocks.empty()) {
            blocks.push_back({sequence, 0});
            continue;
        }

        GenericNackBlock& block = blocks.back();
        const uint16_t delta = static_cast<uint16_t>(sequence - block.pid);
        if (delta >= 1 && delta <= 16) {
            block.blp = static_cast<uint16_t>(
                block.blp | static_cast<uint16_t>(1U << (delta - 1))
            );
        } else {
            blocks.push_back({sequence, 0});
        }
    }
    return blocks;
}

std::vector<uint8_t> make_generic_nack_packet(
    uint32_t sender_ssrc,
    uint32_t media_ssrc,
    const std::vector<uint16_t>& sequences
) {
    const auto blocks = compress_generic_nack_sequences(sequences);
    if (sender_ssrc == 0 || media_ssrc == 0 || blocks.empty()) {
        return {};
    }

    std::vector<uint8_t> packet(rtc::RtcpNack::Size(
        static_cast<unsigned int>(blocks.size())
    ));
    auto* const nack = reinterpret_cast<rtc::RtcpNack*>(packet.data());
    nack->preparePacket(
        sender_ssrc,
        static_cast<unsigned int>(blocks.size())
    );
    nack->header.setMediaSourceSSRC(media_ssrc);
    for (size_t index = 0; index < blocks.size(); ++index) {
        nack->parts[index].setPid(blocks[index].pid);
        nack->parts[index].setBlp(blocks[index].blp);
    }
    return packet;
}

std::vector<uint8_t> make_pli_packet(
    uint32_t sender_ssrc,
    uint32_t media_ssrc
) {
    if (sender_ssrc == 0 || media_ssrc == 0) {
        return {};
    }

    std::vector<uint8_t> packet(rtc::RtcpPli::Size());
    auto* const pli = reinterpret_cast<rtc::RtcpPli*>(packet.data());
    pli->preparePacket(sender_ssrc);
    pli->header.setMediaSourceSSRC(media_ssrc);
    return packet;
}

} // namespace detail

struct RtpVideoReceiverSession::State
    : public std::enable_shared_from_this<RtpVideoReceiverSession::State> {
    enum class IngressKind {
        Rtp,
        Rtcp,
    };

    struct IngressPacket {
        std::vector<uint8_t> bytes;
        IngressKind kind = IngressKind::Rtp;
    };

    explicit State(RtpVideoReceiverSessionConfig session_config)
        : config(std::move(session_config)),
          local_feedback_ssrc(
              config.local_feedback_ssrc == 0
                  ? generate_feedback_ssrc()
                  : config.local_feedback_ssrc
          ),
          timeline(std::make_shared<VideoReceiveTimeline>()) {}

    ~State() {
        shutdown();
    }

    void start_worker() {
        const auto self = shared_from_this();
        worker = std::thread([self]() noexcept {
            self->worker_main();
        });
    }

    void remember_send_callback(const rtc::message_callback& send) noexcept {
        try {
            std::lock_guard<std::mutex> lock(send_mutex);
            if (!stopping.load(std::memory_order_acquire)) {
                send_callback = send;
            }
        } catch (...) {
        }
    }

    void on_incoming(
        rtc::message_vector& messages,
        const rtc::message_callback& send
    ) noexcept {
        remember_send_callback(send);

        rtc::message_vector remaining;
        try {
            remaining.reserve(messages.size());
            for (auto& message : messages) {
                if (!message) {
                    continue;
                }
                if (message->type != rtc::Message::Binary
                    && message->type != rtc::Message::Control) {
                    remaining.push_back(std::move(message));
                    continue;
                }

                const auto* const bytes =
                    reinterpret_cast<const uint8_t*>(message->data());
                const bool is_rtcp =
                    message->type == rtc::Message::Control
                    || is_valid_rtcp_compound(bytes, message->size());
                enqueue_ingress(
                    bytes,
                    message->size(),
                    is_rtcp ? IngressKind::Rtcp : IngressKind::Rtp
                );
            }
            messages.swap(remaining);
        } catch (...) {
            messages.swap(remaining);
        }
    }

    // One packet that cannot be queued is dropped alone. The queue keeps
    // what it has: a dropped packet is a gap the receiver NACKs, while a
    // flushed queue was a gap of hundreds of packets and a key frame.
    void signal_ingress_overflow(size_t rejected_bytes) noexcept {
        ingress_dropped_packets.fetch_add(1, std::memory_order_relaxed);
        ingress_dropped_bytes.fetch_add(
            static_cast<uint64_t>(rejected_bytes),
            std::memory_order_relaxed
        );
        ingress_overflows.fetch_add(1, std::memory_order_relaxed);
    }

    void enqueue_ingress(
        const uint8_t* data,
        size_t size,
        IngressKind kind
    ) noexcept {
        if (stopping.load(std::memory_order_acquire)) {
            return;
        }

        ingress_packets.fetch_add(1, std::memory_order_relaxed);
        ingress_bytes.fetch_add(
            static_cast<uint64_t>(size),
            std::memory_order_relaxed
        );
        if ((data == nullptr && size != 0) || size > kMaxIngressBytes) {
            signal_ingress_overflow(size);
            return;
        }

        IngressPacket packet;
        packet.kind = kind;
        try {
            if (size != 0) {
                packet.bytes.assign(data, data + size);
            }
        } catch (...) {
            signal_ingress_overflow(size);
            return;
        }

        bool overflow = false;
        uint64_t dropped_packets = 0;
        uint64_t dropped_bytes = 0;
        uint64_t queue_size = 0;
        uint64_t queue_bytes = 0;
        {
            std::lock_guard<std::mutex> lock(ingress_mutex);
            if (stopping.load(std::memory_order_acquire)) {
                return;
            }

            overflow =
                ingress_queue.size() >= kMaxIngressPackets
                || size > kMaxIngressBytes - ingress_queued_bytes_value;
            if (overflow) {
                // Tail drop: the newest packet goes, the queue stays.
                dropped_packets = 1;
                dropped_bytes = static_cast<uint64_t>(size);
            } else {
                ingress_queue.push_back(std::move(packet));
                ingress_queued_bytes_value += size;
            }

            queue_size = static_cast<uint64_t>(ingress_queue.size());
            queue_bytes = static_cast<uint64_t>(ingress_queued_bytes_value);
            ingress_queued_packets.store(
                queue_size,
                std::memory_order_relaxed
            );
            ingress_queued_bytes.store(
                queue_bytes,
                std::memory_order_relaxed
            );
        }

        if (overflow) {
            ingress_dropped_packets.fetch_add(
                dropped_packets,
                std::memory_order_relaxed
            );
            ingress_dropped_bytes.fetch_add(
                dropped_bytes,
                std::memory_order_relaxed
            );
            ingress_overflows.fetch_add(1, std::memory_order_relaxed);
        } else {
            update_max(peak_ingress_queued_packets, queue_size);
            update_max(peak_ingress_queued_bytes, queue_bytes);
        }
        worker_cv.notify_one();
    }

    bool send_control(
        const uint8_t* data,
        size_t size,
        bool unavailable_is_failure
    ) noexcept {
        rtc::message_callback send;
        try {
            std::lock_guard<std::mutex> lock(send_mutex);
            send = send_callback;
        } catch (...) {
            if (unavailable_is_failure) {
                feedback_send_failures.fetch_add(1, std::memory_order_relaxed);
            }
            return false;
        }

        if (!send || data == nullptr || size == 0
            || stopping.load(std::memory_order_acquire)) {
            if (unavailable_is_failure) {
                feedback_send_failures.fetch_add(1, std::memory_order_relaxed);
            }
            return false;
        }

        try {
            auto message = rtc::make_message(size, rtc::Message::Control);
            std::memcpy(message->data(), data, size);
            send(std::move(message));
            return true;
        } catch (...) {
            feedback_send_failures.fetch_add(1, std::memory_order_relaxed);
            return false;
        }
    }

    // The only way to a PLI: the timeline's KeyframeNeeded. Any thread. The
    // worker sends it (it owns the RTCP state and the 1 s cooldown).
    void request_keyframe() noexcept {
        keyframe_requested.store(true, std::memory_order_release);
        worker_cv.notify_one();
    }

    // Worker thread. A key frame that is arriving now (its packets are
    // recent) answers the request already: no second IDR for it.
    void take_keyframe_request(SteadyClock::time_point now) noexcept {
        if (!keyframe_requested.exchange(false, std::memory_order_acq_rel)) {
            return;
        }
        if (receiver && receiver->keyframe_in_progress(now)) {
            keyframe_requests_absorbed.fetch_add(1, std::memory_order_relaxed);
            MELLO_LOG_INFO("transport/video-rx", "key frame request absorbed: a key frame is arriving");
            return;
        }
        pli_requests.fetch_add(1, std::memory_order_relaxed);
        pending_pli = true;
    }

    void flush_pending_pli(SteadyClock::time_point now) noexcept {
        if (!pending_pli || !has_remote_media_ssrc) {
            return;
        }
        if (have_last_pli && now - last_pli < kPliCooldown) {
            return;
        }

        try {
            const auto packet = detail::make_pli_packet(
                local_feedback_ssrc,
                remote_media_ssrc
            );
            if (send_control(packet.data(), packet.size(), false)) {
                pending_pli = false;
                have_last_pli = true;
                last_pli = now;
                pli_packets_sent.fetch_add(1, std::memory_order_relaxed);
            }
        } catch (...) {
            feedback_send_failures.fetch_add(1, std::memory_order_relaxed);
        }
    }

    void send_nack(const std::vector<uint16_t>& sequences) noexcept {
        if (sequences.empty() || !has_remote_media_ssrc) {
            feedback_send_failures.fetch_add(1, std::memory_order_relaxed);
            return;
        }

        try {
            std::vector<uint16_t> nack_sequences;
            if (config.fec_enabled) {
                // Parity repair runs ahead of NACK: a buffered FEC block can
                // reconstruct a single loss per group without a network
                // round trip. The receiver core forbids re-entrant
                // callbacks, so recovered packets are injected by the worker
                // loop once the current receiver call unwinds.
                nack_sequences.reserve(sequences.size());
                for (const uint16_t sequence : sequences) {
                    std::vector<uint8_t> recovered;
                    if (fec_recovery.recover(sequence, recovered)) {
                        fec_recovered_injections.push_back(
                            std::move(recovered)
                        );
                    } else {
                        nack_sequences.push_back(sequence);
                    }
                }
                sync_fec_stats();
            } else {
                nack_sequences = sequences;
            }

            if (nack_sequences.empty()) {
                return;
            }
            const auto packet = detail::make_generic_nack_packet(
                local_feedback_ssrc,
                remote_media_ssrc,
                nack_sequences
            );
            if (send_control(packet.data(), packet.size(), true)) {
                nack_packets_sent.fetch_add(1, std::memory_order_relaxed);
                nack_sequences_sent.fetch_add(
                    static_cast<uint64_t>(nack_sequences.size()),
                    std::memory_order_relaxed
                );
            }
        } catch (...) {
            feedback_send_failures.fetch_add(1, std::memory_order_relaxed);
        }
    }

    // Mirrors the recovery buffer's counters into the cross-thread atomics.
    // Worker thread only.
    void sync_fec_stats() noexcept {
        uint64_t recovered = 0;
        uint64_t unrecoverable = 0;
        fec_recovery.stats(recovered, unrecoverable);
        rx_fec_recovered.store(recovered, std::memory_order_relaxed);
        rx_fec_unrecoverable.store(unrecoverable, std::memory_order_relaxed);
    }

    bool send_remb(uint32_t bitrate_bps) noexcept {
        if (!has_remote_media_ssrc || bitrate_bps == 0) {
            return false;
        }

        try {
            const size_t size = rtc::RtcpRemb::SizeWithSSRCs(1);
            auto message = rtc::make_message(size, rtc::Message::Control);
            auto* const remb =
                reinterpret_cast<rtc::RtcpRemb*>(message->data());
            remb->preparePacket(local_feedback_ssrc, 1, bitrate_bps);
            remb->setSsrc(0, remote_media_ssrc);

            rtc::message_callback send;
            {
                std::lock_guard<std::mutex> lock(send_mutex);
                send = send_callback;
            }
            if (!send || stopping.load(std::memory_order_acquire)) {
                return false;
            }
            send(std::move(message));
            remb_packets_sent.fetch_add(1, std::memory_order_relaxed);
            return true;
        } catch (...) {
            feedback_send_failures.fetch_add(1, std::memory_order_relaxed);
            return false;
        }
    }

    uint32_t delay_since_last_sr(SteadyClock::time_point now) const noexcept {
        if (!have_last_sr || now <= last_sr_arrival) {
            return 0;
        }

        const auto elapsed =
            std::chrono::duration_cast<std::chrono::microseconds>(
                now - last_sr_arrival
            ).count();
        const uint64_t units =
            (static_cast<uint64_t>(elapsed) * kRtcpUnitsPerSecond)
            / 1'000'000U;
        return static_cast<uint32_t>(std::min<uint64_t>(
            units,
            std::numeric_limits<uint32_t>::max()
        ));
    }

    void send_receiver_report(SteadyClock::time_point now) noexcept {
        const RtpH264Receiver::Stats current = combined_core_stats();
        if (!has_remote_media_ssrc || !current.has_ssrc) {
            return;
        }

        const int64_t expected_signed =
            static_cast<int64_t>(current.accepted_packets)
            + current.cumulative_loss;
        const uint64_t expected = expected_signed > 0
            ? static_cast<uint64_t>(expected_signed)
            : 0;
        const uint64_t received = current.accepted_packets;
        const uint64_t expected_interval = expected >= rr_expected_prior
            ? expected - rr_expected_prior
            : 0;
        const uint64_t received_interval = received >= rr_received_prior
            ? received - rr_received_prior
            : 0;
        const int64_t lost_interval =
            static_cast<int64_t>(expected_interval)
            - static_cast<int64_t>(received_interval);

        uint8_t fraction_lost = 0;
        if (expected_interval != 0 && lost_interval > 0) {
            fraction_lost = static_cast<uint8_t>(std::min<uint64_t>(
                (static_cast<uint64_t>(lost_interval) * 256U)
                    / expected_interval,
                255
            ));
        }

        const int64_t clamped_loss = std::max(
            kMinSigned24,
            std::min(kMaxSigned24, current.cumulative_loss)
        );
        const uint32_t extended_highest =
            static_cast<uint32_t>(current.extended_highest_sequence);
        const bool sr_matches =
            have_last_sr && last_sr_sender_ssrc == remote_media_ssrc;

        try {
            const size_t size = rtc::RtcpRr::SizeWithReportBlocks(1);
            auto message = rtc::make_message(size, rtc::Message::Control);
            auto* const report =
                reinterpret_cast<rtc::RtcpRr*>(message->data());
            report->preparePacket(local_feedback_ssrc, 1);
            report->getReportBlock(0)->preparePacket(
                remote_media_ssrc,
                fraction_lost,
                static_cast<uint32_t>(clamped_loss),
                static_cast<uint16_t>(extended_highest & 0xffffU),
                static_cast<uint16_t>(extended_highest >> 16),
                current.interarrival_jitter,
                sr_matches ? last_sr_ntp : 0,
                sr_matches ? delay_since_last_sr(now) : 0
            );

            rtc::message_callback send;
            {
                std::lock_guard<std::mutex> lock(send_mutex);
                send = send_callback;
            }
            if (!send || stopping.load(std::memory_order_acquire)) {
                return;
            }
            send(std::move(message));
            rr_expected_prior = expected;
            rr_received_prior = received;
            receiver_reports_sent.fetch_add(1, std::memory_order_relaxed);
        } catch (...) {
            feedback_send_failures.fetch_add(1, std::memory_order_relaxed);
        }
    }

    void process_rtcp(
        const uint8_t* data,
        size_t size,
        SteadyClock::time_point now
    ) noexcept {
        if (!is_valid_rtcp_compound(data, size)) {
            invalid_rtcp_packets.fetch_add(1, std::memory_order_relaxed);
            return;
        }

        size_t offset = 0;
        while (offset < size) {
            const uint8_t packet_type = data[offset + 1];
            const size_t packet_size =
                (static_cast<size_t>(read_u16_be(data + offset + 2)) + 1) * 4;
            if (packet_type == 200 && packet_size >= 28) {
                last_sr_sender_ssrc = read_u32_be(data + offset + 4);
                last_sr_ntp = read_u64_be(data + offset + 8);
                last_sr_arrival = now;
                have_last_sr = true;
                sender_reports_received.fetch_add(
                    1,
                    std::memory_order_relaxed
                );
            }
            offset += packet_size;
        }
    }

    // Worker thread: one complete access unit from the H.264 core, in any
    // order, into the timeline.
    void queue_access_unit(RtpH264Receiver::AccessUnit&& unit) noexcept {
        const uint64_t bytes = static_cast<uint64_t>(unit.annex_b.size());
        bool inserted = false;
        try {
            TimelineInput input;
            input.id = unit.last_sequence;
            input.reference = unit.reference;
            input.rtp_timestamp = unit.rtp_timestamp;
            input.arrival_us = std::chrono::duration_cast<std::chrono::microseconds>(
                                   unit.last_arrival.time_since_epoch())
                                   .count();
            input.keyframe = unit.keyframe;
            input.retransmitted = unit.retransmitted;
            input.annex_b = std::move(unit.annex_b);
            inserted = timeline->insert(std::move(input));
        } catch (...) {
            inserted = false;
        }
        if (inserted) {
            access_units_queued_total.fetch_add(1, std::memory_order_relaxed);
            access_unit_bytes_queued_total.fetch_add(bytes, std::memory_order_relaxed);
        } else {
            access_units_dropped.fetch_add(1, std::memory_order_relaxed);
            access_unit_bytes_dropped.fetch_add(bytes, std::memory_order_relaxed);
        }
    }

    void reset_receiver() {
        RtpH264Receiver::Callbacks callbacks;
        callbacks.on_access_unit =
            [this](RtpH264Receiver::AccessUnit&& unit) {
                queue_access_unit(std::move(unit));
            };
        callbacks.on_nack =
            [this](const std::vector<uint16_t>& sequences) {
                send_nack(sequences);
            };
        // A packet missed its repair deadline: the timeline asks for a key
        // frame through its KeyframeNeeded decision.
        callbacks.on_loss = [this](int64_t sequence) {
            MELLO_LOG_INFO("transport/video-rx", "packet %lld missed its repair deadline: a reference is lost",
                           static_cast<long long>(sequence));
            timeline->on_loss();
        };
        callbacks.render_time =
            [this](uint32_t rtp_timestamp) -> std::optional<SteadyClock::time_point> {
                const auto render = timeline->render_time_us(rtp_timestamp);
                if (!render) return std::nullopt;
                return SteadyClock::time_point(std::chrono::microseconds(*render));
            };

        RtpH264Receiver::Config receiver_config;
        receiver_config.payload_type = config.payload_type;
        const uint32_t rtt_us = rtt_hint_us.load(std::memory_order_relaxed);
        if (rtt_us != 0) {
            receiver_config.rtt = std::chrono::microseconds(rtt_us);
        }
        applied_rtt_us = rtt_us;
        receiver = std::make_unique<RtpH264Receiver>(
            std::move(callbacks),
            receiver_config
        );
        receiver_has_bound_ssrc = false;
        publish_core_stats();
    }

    void observe_accepted_rtp(
        uint16_t sequence,
        uint32_t ssrc,
        uint64_t accepted_before,
        uint64_t accepted_after
    ) noexcept {
        if (accepted_after == accepted_before) {
            return;
        }

        if (!has_remote_media_ssrc) {
            remote_media_ssrc = ssrc;
            has_remote_media_ssrc = true;
        }
        receiver_has_bound_ssrc = true;

        if (!have_global_extended_sequence) {
            global_extended_sequence = sequence;
            have_global_extended_sequence = true;
            return;
        }

        int64_t candidate =
            static_cast<int64_t>(global_extended_sequence & ~uint64_t{0xffff})
            + sequence;
        const int64_t current =
            static_cast<int64_t>(global_extended_sequence);
        const int64_t delta = candidate - current;
        if (delta > 32'768) {
            candidate -= 65'536;
        } else if (delta < -32'768) {
            candidate += 65'536;
        }
        if (candidate > current) {
            global_extended_sequence = static_cast<uint64_t>(candidate);
        }
    }

    void record_twcc_arrival(
        const uint8_t* data,
        size_t size,
        SteadyClock::time_point now
    ) noexcept {
        if (!config.twcc_enabled) {
            return;
        }
        uint16_t twcc_sequence = 0;
        if (extract_twcc_sequence(data, size, twcc_sequence)) {
            const int64_t arrival_us =
                std::chrono::duration_cast<std::chrono::microseconds>(
                    now.time_since_epoch())
                    .count();
            twcc_generator.on_packet(twcc_sequence, arrival_us);
        }
    }

    // Eager parity repair: a freshly buffered FEC block (or a sibling media
    // packet that completes a group) can rebuild covered sequences still
    // missing from the media buffer. Recovered packets are queued for
    // injection outside any receiver callback.
    void queue_eager_fec_recovery() noexcept {
        try {
            for (const uint16_t sequence :
                 fec_recovery.uncovered_mask_sequences()) {
                std::vector<uint8_t> recovered;
                if (fec_recovery.recover(sequence, recovered)) {
                    fec_recovered_injections.push_back(
                        std::move(recovered)
                    );
                }
            }
            sync_fec_stats();
        } catch (...) {
            feedback_send_failures.fetch_add(1, std::memory_order_relaxed);
        }
    }

    void inject_queued_fec_recoveries(
        SteadyClock::time_point now
    ) noexcept {
        if (fec_recovered_injections.empty()) {
            return;
        }
        std::vector<std::vector<uint8_t>> injections;
        injections.swap(fec_recovered_injections);
        for (const auto& injection : injections) {
            receiver->on_rtp_packet(
                injection.data(),
                injection.size(),
                now
            );
        }
    }

    void process_rtp(
        const uint8_t* data,
        size_t size,
        SteadyClock::time_point now
    ) {
        uint16_t sequence = 0;
        uint32_t ssrc = 0;
        uint8_t payload_type = 0;
        bool has_basic_header = false;
        if (data != nullptr && size >= 12 && (data[0] >> 6) == 2) {
            sequence = read_u16_be(data + 2);
            payload_type = static_cast<uint8_t>(data[1] & 0x7fU);
            ssrc = read_u32_be(data + 8);
            has_basic_header = true;
        }

        // Parity packets ride their own SSRC (media + 1) and payload type;
        // they feed the recovery buffer and never enter the H.264 core.
        if (has_basic_header && config.fec_enabled && has_remote_media_ssrc
            && ssrc == remote_media_ssrc + 1
            && payload_type == kUlpfecPayloadType) {
            fec_recovery.add_fec_packet(data, size);
            record_twcc_arrival(data, size, now);
            queue_eager_fec_recovery();
            return;
        }

        if (has_basic_header && has_remote_media_ssrc
            && !receiver_has_bound_ssrc
            && ssrc != remote_media_ssrc) {
            wrong_ssrc_packets_after_recovery.fetch_add(
                1,
                std::memory_order_relaxed
            );
            return;
        }

        const uint64_t accepted_before = receiver->accepted_packets();
        receiver->on_rtp_packet(data, size, now);
        if (has_basic_header) {
            observe_accepted_rtp(sequence, ssrc, accepted_before,
                                 receiver->accepted_packets());
        }
        if (config.fec_enabled) {
            fec_recovery.add_media_packet(data, size);
        }
        record_twcc_arrival(data, size, now);
    }

    RtpH264Receiver::Stats combined_core_stats() const noexcept {
        RtpH264Receiver::Stats result;
        if (receiver) {
            result = receiver->stats();
        }
        // The C ABI fields of the old IDR gate now describe the timeline's
        // wait for a key frame.
        const TimelineStats t = timeline->stats();
        result.pli_requests = t.keyframe_requests;
        result.gate_dropped_access_units = t.frames_dropped;
        result.gate_entries = t.losses;
        result.gated = t.keyframe_required;
        result.has_ssrc = has_remote_media_ssrc;
        result.ssrc = has_remote_media_ssrc ? remote_media_ssrc : 0;
        result.extended_highest_sequence = have_global_extended_sequence
            ? global_extended_sequence
            : 0;
        return result;
    }

    void publish_core_stats() noexcept {
        try {
            const auto snapshot = combined_core_stats();
            std::lock_guard<std::mutex> lock(core_stats_mutex);
            published_core_stats = snapshot;
        } catch (...) {
        }
    }

    void worker_main() noexcept {
        auto next_tick = SteadyClock::now() + kReceiverTickInterval;
        auto next_report =
            SteadyClock::now() + config.receiver_report_interval;
        auto next_twcc = SteadyClock::now() + kTwccReportInterval;
        auto next_av_sync = SteadyClock::now() + kAvSyncTickInterval;

        try {
            reset_receiver();
            auto next_publish = SteadyClock::now() + kStatsPublishInterval;
            for (;;) {
                IngressPacket packet;
                bool have_packet = false;
                {
                    std::unique_lock<std::mutex> lock(ingress_mutex);
                    const auto wake_at = config.twcc_enabled
                        ? std::min({next_tick, next_report, next_twcc})
                        : std::min(next_tick, next_report);
                    worker_cv.wait_until(lock, wake_at, [this]() {
                        return stopping.load(std::memory_order_acquire)
                            || !ingress_queue.empty()
                            || keyframe_requested.load(std::memory_order_acquire);
                    });
                    if (stopping.load(std::memory_order_acquire)) {
                        break;
                    }

                    if (!ingress_queue.empty()) {
                        packet = std::move(ingress_queue.front());
                        ingress_queued_bytes_value -= packet.bytes.size();
                        ingress_queue.pop_front();
                        ingress_queued_packets.store(
                            static_cast<uint64_t>(ingress_queue.size()),
                            std::memory_order_relaxed
                        );
                        ingress_queued_bytes.store(
                            static_cast<uint64_t>(ingress_queued_bytes_value),
                            std::memory_order_relaxed
                        );
                        have_packet = true;
                    }
                }

                // The RTT applies at once (set_rtt_hint), not at a rebuild.
                const uint32_t rtt_us = rtt_hint_us.load(std::memory_order_relaxed);
                if (rtt_us != applied_rtt_us) {
                    applied_rtt_us = rtt_us;
                    if (rtt_us != 0) {
                        receiver->set_rtt(std::chrono::microseconds(rtt_us));
                    }
                }
                // The timeline's RTT is the time a repair takes on this
                // link when that is longer than the ping (a repair crosses
                // the viewer leg's jitter): its delay then leaves room for it.
                {
                    const int64_t repair_us = receiver->repair_latency().count();
                    const int64_t want = std::max<int64_t>(rtt_us, repair_us);
                    if (want > 0 && std::abs(want - timeline_rtt_us) > 2'000) {
                        timeline_rtt_us = want;
                        timeline->set_rtt_us(want);
                    }
                }

                auto now = SteadyClock::now();
                if (have_packet) {
                    if (packet.kind == IngressKind::Rtp) {
                        process_rtp(
                            packet.bytes.data(),
                            packet.bytes.size(),
                            now
                        );
                        if (config.fec_enabled) {
                            queue_eager_fec_recovery();
                        }
                    } else {
                        process_rtcp(
                            packet.bytes.data(),
                            packet.bytes.size(),
                            now
                        );
                    }
                }

                now = SteadyClock::now();
                if (now >= next_tick) {
                    receiver->tick(now);
                    next_tick = now + kReceiverTickInterval;
                    if (now >= next_publish) {
                        publish_core_stats();
                        next_publish = now + kStatsPublishInterval;
                    }
                    if (config.fec_enabled) {
                        queue_eager_fec_recovery();
                    }
                }

                inject_queued_fec_recoveries(now);

                // Stage 7: the A/V sync sets the timeline's minimum delay.
                if (now >= next_av_sync) {
                    next_av_sync = now + kAvSyncTickInterval;
                    std::shared_ptr<AvSyncInput> sync;
                    {
                        std::lock_guard<std::mutex> lock(av_sync_mutex);
                        sync = av_sync;
                    }
                    if (sync) {
                        sync->tick(*timeline, timeline->now_us());
                    }
                }

                const uint64_t requested_generation =
                    receive_target_generation.load(std::memory_order_acquire);
                if (requested_generation != sent_receive_target_generation) {
                    const uint32_t target =
                        receive_target_bps.load(std::memory_order_relaxed);
                    if (send_remb(target)) {
                        sent_receive_target_generation =
                            requested_generation;
                    }
                }
                take_keyframe_request(now);
                flush_pending_pli(now);

                if (config.twcc_enabled && now >= next_twcc) {
                    if (has_remote_media_ssrc
                        && twcc_generator.pending() > 0) {
                        const auto report = twcc_generator.build_feedback(
                            local_feedback_ssrc,
                            remote_media_ssrc
                        );
                        if (!report.empty()
                            && send_control(report.data(), report.size(), false)) {
                            twcc_packets_sent.fetch_add(
                                1,
                                std::memory_order_relaxed
                            );
                        }
                    }
                    next_twcc = now + kTwccReportInterval;
                }

                if (now >= next_report) {
                    send_receiver_report(now);
                    next_report = now + config.receiver_report_interval;
                }
            }
        } catch (...) {
            feedback_send_failures.fetch_add(1, std::memory_order_relaxed);
        }

        publish_core_stats();
        receiver.reset();
    }

    void shutdown() noexcept {
        std::lock_guard<std::mutex> shutdown_lock(shutdown_mutex);
        if (shutdown_started) {
            return;
        }
        shutdown_started = true;
        stopping.store(true, std::memory_order_release);
        attached.store(false, std::memory_order_release);
        timeline->stop();

        try {
            if (track && root_handler
                && track->getMediaHandler() == root_handler) {
                track->setMediaHandler(previous_handler);
            }
        } catch (...) {
        }

        worker_cv.notify_all();
        if (worker.joinable()) {
            try {
                if (worker.get_id() == std::this_thread::get_id()) {
                    worker.detach();
                } else {
                    worker.join();
                }
            } catch (...) {
            }
        }

        try {
            std::lock_guard<std::mutex> lock(send_mutex);
            send_callback = {};
        } catch (...) {
        }
        root_handler.reset();
        previous_handler.reset();
        track.reset();
    }

    bool track_is_open() const noexcept {
        try {
            return attached.load(std::memory_order_acquire)
                && !stopping.load(std::memory_order_acquire)
                && track
                && !track->isClosed()
                && track->isOpen()
                && is_receive_direction(track->direction());
        } catch (...) {
            return false;
        }
    }

    RtpVideoReceiverSessionConfig config;
    const uint32_t local_feedback_ssrc;
    std::shared_ptr<rtc::Track> track;
    std::shared_ptr<rtc::MediaHandler> root_handler;
    std::shared_ptr<rtc::MediaHandler> previous_handler;
    std::atomic<bool> attached{false};
    std::atomic<bool> stopping{false};

    std::mutex ingress_mutex;
    std::condition_variable worker_cv;
    std::deque<IngressPacket> ingress_queue;
    size_t ingress_queued_bytes_value = 0;

    // The viewer timeline. Shared with the viewer pipeline, which polls it on
    // its decode thread (PeerConnectionImpl::video_receive_timeline).
    std::shared_ptr<VideoReceiveTimeline> timeline;
    // Stage 7: the A/V sync input (set_av_sync), read by the worker.
    mutable std::mutex av_sync_mutex;
    std::shared_ptr<AvSyncInput> av_sync;
    std::atomic<bool> keyframe_requested{false};
    std::atomic<uint64_t> keyframe_requests_absorbed{0};
    uint32_t applied_rtt_us = 0;  // worker thread
    int64_t timeline_rtt_us = 0;  // worker thread

    // pop_access_unit (the pull API of the tests and tools): a frame that did
    // not fit the caller's buffer waits here for the next call.
    std::mutex output_mutex;
    std::optional<TimelineFrame> held_frame;

    std::mutex send_mutex;
    rtc::message_callback send_callback;

    std::mutex shutdown_mutex;
    bool shutdown_started = false;
    std::thread worker;

    std::unique_ptr<RtpH264Receiver> receiver;
    mutable std::mutex core_stats_mutex;
    RtpH264Receiver::Stats published_core_stats;

    bool has_remote_media_ssrc = false;
    uint32_t remote_media_ssrc = 0;
    bool receiver_has_bound_ssrc = false;
    bool have_global_extended_sequence = false;
    uint64_t global_extended_sequence = 0;

    bool pending_pli = false;
    bool have_last_pli = false;
    SteadyClock::time_point last_pli{};
    bool have_last_sr = false;
    uint32_t last_sr_sender_ssrc = 0;
    uint64_t last_sr_ntp = 0;
    SteadyClock::time_point last_sr_arrival{};
    uint64_t rr_expected_prior = 0;
    uint64_t rr_received_prior = 0;
    uint64_t sent_receive_target_generation = 0;

    std::atomic<uint32_t> receive_target_bps{0};
    std::atomic<uint64_t> receive_target_generation{0};

    // RTT hint in microseconds (0 = unmeasured): sizes the NACK retry budget
    // so high-RTT links still get repairs inside the AU stall deadline.
    std::atomic<uint32_t> rtt_hint_us{0};

    // TWCC feedback generation (worker thread only).
    TwccFeedbackGenerator twcc_generator;
    std::atomic<uint64_t> twcc_packets_sent{0};

    // ULPFEC recovery (worker thread only): buffers media + parity packets
    // and rebuilds single losses per group. Recovered packets queued from
    // inside receiver callbacks wait in fec_recovered_injections for the
    // worker loop to inject them outside any receiver call.
    UlpfecRecovery fec_recovery;
    std::vector<std::vector<uint8_t>> fec_recovered_injections;
    std::atomic<uint64_t> rx_fec_recovered{0};
    std::atomic<uint64_t> rx_fec_unrecoverable{0};

    std::atomic<uint64_t> ingress_packets{0};
    std::atomic<uint64_t> ingress_bytes{0};
    std::atomic<uint64_t> ingress_dropped_packets{0};
    std::atomic<uint64_t> ingress_dropped_bytes{0};
    std::atomic<uint64_t> ingress_overflows{0};
    std::atomic<uint64_t> ingress_queued_packets{0};
    std::atomic<uint64_t> ingress_queued_bytes{0};
    std::atomic<uint64_t> peak_ingress_queued_packets{0};
    std::atomic<uint64_t> peak_ingress_queued_bytes{0};
    std::atomic<uint64_t> wrong_ssrc_packets_after_recovery{0};

    std::atomic<uint64_t> access_units_queued_total{0};
    std::atomic<uint64_t> access_unit_bytes_queued_total{0};
    std::atomic<uint64_t> access_units_dropped{0};
    std::atomic<uint64_t> access_unit_bytes_dropped{0};

    std::atomic<uint64_t> nack_packets_sent{0};
    std::atomic<uint64_t> nack_sequences_sent{0};
    std::atomic<uint64_t> pli_requests{0};
    std::atomic<uint64_t> pli_packets_sent{0};
    std::atomic<uint64_t> remb_packets_sent{0};
    std::atomic<uint64_t> receiver_reports_sent{0};
    std::atomic<uint64_t> sender_reports_received{0};
    std::atomic<uint64_t> invalid_rtcp_packets{0};
    std::atomic<uint64_t> feedback_send_failures{0};
};

RtpVideoReceiverSession::RtpVideoReceiverSession(
    std::shared_ptr<rtc::Track> track,
    uint8_t payload_type
) noexcept
    : RtpVideoReceiverSession(
          std::move(track),
          RtpVideoReceiverSessionConfig{payload_type, 0,
                                        std::chrono::milliseconds(1000)}
      ) {}

RtpVideoReceiverSession::RtpVideoReceiverSession(
    std::shared_ptr<rtc::Track> track,
    RtpVideoReceiverSessionConfig config
) noexcept {
    try {
        auto state = std::make_shared<State>(std::move(config));
        state_ = state;
        if (!track
            || state->config.payload_type > 127
            || state->config.receiver_report_interval
                <= std::chrono::milliseconds::zero()
            || track->isClosed()
            || !is_valid_receiver_track(*track, state->config.payload_type)) {
            return;
        }

        state->track = std::move(track);
        state->previous_handler = state->track->getMediaHandler();
        const std::weak_ptr<State> weak_state = state;
        state->root_handler = std::make_shared<ReceiverMediaHandler>(
            [weak_state](
                rtc::message_vector& messages,
                const rtc::message_callback& send
            ) {
                if (const auto locked = weak_state.lock()) {
                    locked->on_incoming(messages, send);
                }
            }
        );

        // KeyframeNeeded from whoever polls the timeline becomes a PLI.
        state->timeline->set_keyframe_request_handler([weak_state]() {
            if (const auto locked = weak_state.lock()) {
                locked->request_keyframe();
            }
        });
        state->start_worker();
        state->track->setMediaHandler(state->root_handler);
        state->attached.store(true, std::memory_order_release);
    } catch (...) {
        if (state_) {
            state_->shutdown();
        }
    }
}

RtpVideoReceiverSession::~RtpVideoReceiverSession() {
    const auto state = std::move(state_);
    if (state) {
        state->shutdown();
    }
}

RtpVideoReceiverSession::RtpVideoReceiverSession(
    RtpVideoReceiverSession&&
) noexcept = default;

RtpVideoReceiverSession& RtpVideoReceiverSession::operator=(
    RtpVideoReceiverSession&& other
) noexcept {
    if (this == &other) {
        return *this;
    }

    const auto old_state = std::move(state_);
    state_ = std::move(other.state_);
    if (old_state) {
        old_state->shutdown();
    }
    return *this;
}

namespace {

// The pull API for tests and tools: the next frame the timeline releases
// now. A key-frame request on the way goes to the worker.
std::optional<TimelineFrame> release_now(VideoReceiveTimeline& timeline) {
    for (int i = 0; i < 4; ++i) {
        TimelineDecision d = timeline.poll();
        switch (d.kind) {
        case TimelineDecision::Kind::Release:
            return std::move(d.frame);
        case TimelineDecision::Kind::KeyframeNeeded:
            // The timeline's handler sent it to the worker already.
            continue;
        default:
            return std::nullopt;
        }
    }
    return std::nullopt;
}

} // namespace

std::optional<RtpVideoReceiverAccessUnit>
RtpVideoReceiverSession::pop_access_unit() noexcept {
    const auto state = state_;
    if (!state) {
        return std::nullopt;
    }

    try {
        std::lock_guard<std::mutex> lock(state->output_mutex);
        std::optional<TimelineFrame> frame;
        if (state->held_frame) {
            frame = std::move(state->held_frame);
            state->held_frame.reset();
        } else {
            frame = release_now(*state->timeline);
        }
        if (!frame) {
            return std::nullopt;
        }
        RtpVideoReceiverAccessUnit access_unit;
        access_unit.annex_b = std::move(frame->annex_b);
        access_unit.is_idr = frame->keyframe;
        access_unit.rtp_timestamp = frame->rtp_timestamp;
        return access_unit;
    } catch (...) {
        return std::nullopt;
    }
}

RtpVideoReceiverPopResult RtpVideoReceiverSession::pop_access_unit(
    uint8_t* buffer,
    size_t capacity,
    size_t& size,
    bool& is_idr,
    uint32_t& rtp_timestamp
) noexcept {
    const auto state = state_;
    if (!state) {
        size = 0;
        return RtpVideoReceiverPopResult::Empty;
    }

    try {
        std::lock_guard<std::mutex> lock(state->output_mutex);
        if (!state->held_frame) {
            state->held_frame = release_now(*state->timeline);
        }
        if (!state->held_frame) {
            size = 0;
            return RtpVideoReceiverPopResult::Empty;
        }

        const TimelineFrame& frame = *state->held_frame;
        size = frame.annex_b.size();
        is_idr = frame.keyframe;
        rtp_timestamp = frame.rtp_timestamp;
        if (buffer == nullptr || capacity < size) {
            return RtpVideoReceiverPopResult::BufferTooSmall;
        }

        std::memcpy(buffer, frame.annex_b.data(), size);
        state->held_frame.reset();
        return RtpVideoReceiverPopResult::Ok;
    } catch (...) {
        size = 0;
        return RtpVideoReceiverPopResult::Empty;
    }
}

std::shared_ptr<VideoReceiveTimeline>
RtpVideoReceiverSession::timeline() const noexcept {
    const auto state = state_;
    return state ? state->timeline : nullptr;
}

void RtpVideoReceiverSession::set_av_sync(std::shared_ptr<AvSyncInput> input) noexcept {
    if (!state_) {
        return;
    }
    std::lock_guard<std::mutex> lock(state_->av_sync_mutex);
    state_->av_sync = std::move(input);
}

void RtpVideoReceiverSession::request_keyframe() noexcept {
    const auto state = state_;
    if (state) {
        state->request_keyframe();
    }
}

bool RtpVideoReceiverSession::set_receive_target(
    uint32_t bitrate_bps
) noexcept {
    const auto state = state_;
    if (!state || bitrate_bps == 0
        || !state->attached.load(std::memory_order_acquire)
        || state->stopping.load(std::memory_order_acquire)) {
        return false;
    }

    state->receive_target_bps.store(bitrate_bps, std::memory_order_relaxed);
    state->receive_target_generation.fetch_add(1, std::memory_order_release);
    state->worker_cv.notify_one();
    return true;
}

void RtpVideoReceiverSession::set_rtt_hint(float rtt_ms) noexcept {
    const auto state = state_;
    if (!state) {
        return;
    }
    const uint32_t micros =
        (rtt_ms > 0.0f && rtt_ms < 10'000.0f)
            ? static_cast<uint32_t>(rtt_ms * 1'000.0f)
            : 0u;
    state->rtt_hint_us.store(micros, std::memory_order_relaxed);
    if (micros != 0) {
        // At once: the timeline's late-frame rule and jitter estimate, and
        // (on the worker's next pass) the NACK interval and repair deadlines.
        state->timeline->set_rtt_us(static_cast<int64_t>(micros));
    }
    state->worker_cv.notify_one();
}

bool RtpVideoReceiverSession::is_open() const noexcept {
    const auto state = state_;
    return state && state->track_is_open();
}

RtpVideoReceiverSessionStats RtpVideoReceiverSession::stats() const noexcept {
    RtpVideoReceiverSessionStats result;
    const auto state = state_;
    if (!state) {
        return result;
    }

    result.ingress_packets =
        state->ingress_packets.load(std::memory_order_relaxed);
    result.ingress_bytes =
        state->ingress_bytes.load(std::memory_order_relaxed);
    result.ingress_dropped_packets =
        state->ingress_dropped_packets.load(std::memory_order_relaxed);
    result.ingress_dropped_bytes =
        state->ingress_dropped_bytes.load(std::memory_order_relaxed);
    result.ingress_overflows =
        state->ingress_overflows.load(std::memory_order_relaxed);
    result.ingress_queued_packets =
        state->ingress_queued_packets.load(std::memory_order_relaxed);
    result.ingress_queued_bytes =
        state->ingress_queued_bytes.load(std::memory_order_relaxed);
    result.peak_ingress_queued_packets =
        state->peak_ingress_queued_packets.load(std::memory_order_relaxed);
    result.peak_ingress_queued_bytes =
        state->peak_ingress_queued_bytes.load(std::memory_order_relaxed);
    result.wrong_ssrc_packets_after_recovery =
        state->wrong_ssrc_packets_after_recovery.load(
            std::memory_order_relaxed
        );

    result.access_units_queued_total =
        state->access_units_queued_total.load(std::memory_order_relaxed);
    result.access_unit_bytes_queued_total =
        state->access_unit_bytes_queued_total.load(std::memory_order_relaxed);
    result.access_units_dropped =
        state->access_units_dropped.load(std::memory_order_relaxed);
    result.access_unit_bytes_dropped =
        state->access_unit_bytes_dropped.load(std::memory_order_relaxed);
    result.timeline = state->timeline->stats();
    result.output_queued_access_units = result.timeline.frames_buffered;
    result.peak_output_queued_access_units = result.timeline.frames_buffered;
    result.keyframe_requests_absorbed =
        state->keyframe_requests_absorbed.load(std::memory_order_relaxed);

    result.nack_packets_sent =
        state->nack_packets_sent.load(std::memory_order_relaxed);
    result.nack_sequences_sent =
        state->nack_sequences_sent.load(std::memory_order_relaxed);
    result.pli_requests =
        state->pli_requests.load(std::memory_order_relaxed);
    result.pli_packets_sent =
        state->pli_packets_sent.load(std::memory_order_relaxed);
    result.remb_packets_sent =
        state->remb_packets_sent.load(std::memory_order_relaxed);
    result.twcc_packets_sent =
        state->twcc_packets_sent.load(std::memory_order_relaxed);
    result.rx_fec_recovered =
        state->rx_fec_recovered.load(std::memory_order_relaxed);
    result.rx_fec_unrecoverable =
        state->rx_fec_unrecoverable.load(std::memory_order_relaxed);
    result.receiver_reports_sent =
        state->receiver_reports_sent.load(std::memory_order_relaxed);
    result.sender_reports_received =
        state->sender_reports_received.load(std::memory_order_relaxed);
    result.invalid_rtcp_packets =
        state->invalid_rtcp_packets.load(std::memory_order_relaxed);
    result.feedback_send_failures =
        state->feedback_send_failures.load(std::memory_order_relaxed);

    result.payload_type = state->config.payload_type;
    result.local_feedback_ssrc = state->local_feedback_ssrc;
    result.receive_target_bps =
        state->receive_target_bps.load(std::memory_order_relaxed);
    result.awaiting_output_idr = result.timeline.keyframe_required;
    try {
        std::lock_guard<std::mutex> lock(state->core_stats_mutex);
        result.core = state->published_core_stats;
        result.has_remote_media_ssrc = result.core.has_ssrc;
        result.remote_media_ssrc = result.core.ssrc;
    } catch (...) {
    }
    return result;
}

} // namespace mello::transport
