#include "rtp_video_receiver_session.hpp"

#include "twcc.hpp"

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

void add_core_counters(
    RtpH264Receiver::Stats& destination,
    const RtpH264Receiver::Stats& source
) noexcept {
    destination.packets += source.packets;
    destination.bytes_received += source.bytes_received;
    destination.accepted_packets += source.accepted_packets;
    destination.accepted_bytes += source.accepted_bytes;
    destination.duplicates += source.duplicates;
    destination.late_packets += source.late_packets;
    destination.invalid_rtp_packets += source.invalid_rtp_packets;
    destination.invalid_h264_packets += source.invalid_h264_packets;
    destination.wrong_payload_type_packets +=
        source.wrong_payload_type_packets;
    destination.wrong_ssrc_packets += source.wrong_ssrc_packets;
    destination.backwards_time_inputs += source.backwards_time_inputs;
    destination.missing_sequences_detected +=
        source.missing_sequences_detected;
    destination.repaired_packets += source.repaired_packets;
    destination.nacks += source.nacks;
    destination.nack_callbacks += source.nack_callbacks;
    destination.complete_access_units += source.complete_access_units;
    destination.incomplete_access_units += source.incomplete_access_units;
    destination.emitted_access_units += source.emitted_access_units;
    destination.pli_requests += source.pli_requests;
    destination.gate_dropped_access_units +=
        source.gate_dropped_access_units;
    destination.gate_entries += source.gate_entries;
    destination.gate_exits += source.gate_exits;
    destination.buffer_evictions += source.buffer_evictions;
    destination.sequence_discontinuities +=
        source.sequence_discontinuities;
    destination.cumulative_loss += source.cumulative_loss;
    destination.peak_buffered_access_units = std::max(
        destination.peak_buffered_access_units,
        source.peak_buffered_access_units
    );
    destination.peak_buffered_packets = std::max(
        destination.peak_buffered_packets,
        source.peak_buffered_packets
    );
    destination.peak_buffered_bytes = std::max(
        destination.peak_buffered_bytes,
        source.peak_buffered_bytes
    );
}

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
          ) {
        completed_core.gate_entries = 0;
    }

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

    void signal_ingress_overflow(size_t rejected_bytes) noexcept {
        uint64_t dropped_packets = 1;
        uint64_t dropped_bytes = static_cast<uint64_t>(rejected_bytes);
        {
            std::lock_guard<std::mutex> lock(ingress_mutex);
            dropped_packets += static_cast<uint64_t>(ingress_queue.size());
            dropped_bytes += static_cast<uint64_t>(ingress_queued_bytes_value);
            ingress_queue.clear();
            ingress_queued_bytes_value = 0;
            ++recovery_generation;
            ingress_queued_packets.store(0, std::memory_order_relaxed);
            ingress_queued_bytes.store(0, std::memory_order_relaxed);
        }
        ingress_dropped_packets.fetch_add(
            dropped_packets,
            std::memory_order_relaxed
        );
        ingress_dropped_bytes.fetch_add(
            dropped_bytes,
            std::memory_order_relaxed
        );
        ingress_overflows.fetch_add(1, std::memory_order_relaxed);
        worker_cv.notify_one();
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
                dropped_packets =
                    static_cast<uint64_t>(ingress_queue.size()) + 1;
                dropped_bytes =
                    static_cast<uint64_t>(ingress_queued_bytes_value)
                    + static_cast<uint64_t>(size);
                ingress_queue.clear();
                ingress_queued_bytes_value = 0;
                ++recovery_generation;
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

    void request_pli() noexcept {
        pli_requests.fetch_add(1, std::memory_order_relaxed);
        pending_pli = true;
        flush_pending_pli(SteadyClock::now());
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
            const auto packet = detail::make_generic_nack_packet(
                local_feedback_ssrc,
                remote_media_ssrc,
                sequences
            );
            if (send_control(packet.data(), packet.size(), true)) {
                nack_packets_sent.fetch_add(1, std::memory_order_relaxed);
                nack_sequences_sent.fetch_add(
                    static_cast<uint64_t>(sequences.size()),
                    std::memory_order_relaxed
                );
            }
        } catch (...) {
            feedback_send_failures.fetch_add(1, std::memory_order_relaxed);
        }
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

    void queue_access_unit(
        const std::vector<uint8_t>& annex_b,
        bool is_idr,
        uint32_t rtp_timestamp
    ) noexcept {
        uint64_t dropped_count = 0;
        uint64_t dropped_bytes = 0;
        bool request_recovery = false;
        uint64_t queue_count = 0;
        uint64_t queue_bytes = 0;

        try {
            RtpVideoReceiverAccessUnit access_unit;
            access_unit.annex_b = annex_b;
            access_unit.is_idr = is_idr;
            access_unit.rtp_timestamp = rtp_timestamp;

            std::lock_guard<std::mutex> lock(output_mutex);
            const auto clear_output = [&]() noexcept {
                dropped_count += static_cast<uint64_t>(output_queue.size());
                dropped_bytes += static_cast<uint64_t>(
                    output_queued_bytes_value
                );
                output_queue.clear();
                output_queued_bytes_value = 0;
            };

            if (output_awaiting_idr && !is_idr) {
                dropped_count = 1;
                dropped_bytes = static_cast<uint64_t>(annex_b.size());
            } else if (annex_b.size() > kMaxOutputBytes) {
                clear_output();
                ++dropped_count;
                dropped_bytes += static_cast<uint64_t>(annex_b.size());
                output_awaiting_idr = true;
                request_recovery = true;
            } else {
                const bool exceeds_bounds =
                    output_queue.size() >= kMaxOutputAccessUnits
                    || annex_b.size()
                        > kMaxOutputBytes - output_queued_bytes_value;
                if (exceeds_bounds) {
                    clear_output();
                    if (!is_idr) {
                        ++dropped_count;
                        dropped_bytes +=
                            static_cast<uint64_t>(annex_b.size());
                        output_awaiting_idr = true;
                        request_recovery = true;
                    }
                }

                if (!output_awaiting_idr || is_idr) {
                    output_queue.push_back(std::move(access_unit));
                    output_queued_bytes_value += annex_b.size();
                    output_awaiting_idr = false;
                    access_units_queued_total.fetch_add(
                        1,
                        std::memory_order_relaxed
                    );
                    access_unit_bytes_queued_total.fetch_add(
                        static_cast<uint64_t>(annex_b.size()),
                        std::memory_order_relaxed
                    );
                }
            }

            queue_count = static_cast<uint64_t>(output_queue.size());
            queue_bytes = static_cast<uint64_t>(output_queued_bytes_value);
            output_queued_access_units.store(
                queue_count,
                std::memory_order_relaxed
            );
            output_queued_bytes.store(
                queue_bytes,
                std::memory_order_relaxed
            );
            awaiting_output_idr.store(
                output_awaiting_idr,
                std::memory_order_relaxed
            );
        } catch (...) {
            dropped_count = 1;
            dropped_bytes = static_cast<uint64_t>(annex_b.size());
            request_recovery = true;
            {
                std::lock_guard<std::mutex> lock(output_mutex);
                output_awaiting_idr = true;
                awaiting_output_idr.store(true, std::memory_order_relaxed);
            }
        }

        access_units_dropped.fetch_add(
            dropped_count,
            std::memory_order_relaxed
        );
        access_unit_bytes_dropped.fetch_add(
            dropped_bytes,
            std::memory_order_relaxed
        );
        update_max(peak_output_queued_access_units, queue_count);
        update_max(peak_output_queued_bytes, queue_bytes);
        if (request_recovery) {
            request_pli();
        }
    }

    void reset_receiver(bool recovery) {
        if (receiver) {
            add_core_counters(completed_core, receiver->stats());
            receiver.reset();
        }

        RtpH264Receiver::Callbacks callbacks;
        callbacks.on_access_unit =
            [this](const std::vector<uint8_t>& bytes,
                   bool is_idr,
                   uint32_t timestamp) {
                queue_access_unit(bytes, is_idr, timestamp);
            };
        callbacks.on_nack =
            [this](const std::vector<uint16_t>& sequences) {
                send_nack(sequences);
            };
        callbacks.on_pli = [this]() {
            request_pli();
        };

        RtpH264Receiver::Config receiver_config;
        receiver_config.payload_type = config.payload_type;
        // RTT-adaptive NACK budget: one attempt per ~20ms of RTT (bounded),
        // so a 100ms link gets ~6 repair chances instead of the static 2
        // (whose ~30ms repair window could never beat a 100ms RTT).
        const uint32_t rtt_us = rtt_hint_us.load(std::memory_order_relaxed);
        if (rtt_us != 0) {
            const size_t attempts = static_cast<size_t>(
                std::clamp((rtt_us + 10'000) / 20'000 + 1, 2u, 8u));
            receiver_config.nack_max_attempts = attempts;
        }
        receiver = std::make_unique<RtpH264Receiver>(
            std::move(callbacks),
            receiver_config
        );
        receiver_has_bound_ssrc = false;
        if (recovery) {
            core_restarts.fetch_add(1, std::memory_order_relaxed);
            request_pli();
        }
        publish_core_stats();
    }

    void observe_accepted_rtp(
        uint16_t sequence,
        uint32_t ssrc,
        const RtpH264Receiver::Stats& before,
        const RtpH264Receiver::Stats& after
    ) noexcept {
        if (after.accepted_packets == before.accepted_packets) {
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

    void process_rtp(
        const uint8_t* data,
        size_t size,
        SteadyClock::time_point now
    ) {
        uint16_t sequence = 0;
        uint32_t ssrc = 0;
        bool has_basic_header = false;
        if (data != nullptr && size >= 12 && (data[0] >> 6) == 2) {
            sequence = read_u16_be(data + 2);
            ssrc = read_u32_be(data + 8);
            has_basic_header = true;
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

        const auto before = receiver->stats();
        receiver->on_rtp_packet(data, size, now);
        const auto after = receiver->stats();
        if (has_basic_header) {
            observe_accepted_rtp(sequence, ssrc, before, after);
        }

        if (config.twcc_enabled) {
            uint16_t twcc_sequence = 0;
            if (extract_twcc_sequence(data, size, twcc_sequence)) {
                const int64_t arrival_us =
                    std::chrono::duration_cast<std::chrono::microseconds>(
                        now.time_since_epoch())
                        .count();
                twcc_generator.on_packet(twcc_sequence, arrival_us);
            }
        }
    }

    RtpH264Receiver::Stats combined_core_stats() const noexcept {
        RtpH264Receiver::Stats result = completed_core;
        if (receiver) {
            const auto current = receiver->stats();
            add_core_counters(result, current);
            result.buffered_access_units = current.buffered_access_units;
            result.buffered_packets = current.buffered_packets;
            result.buffered_bytes = current.buffered_bytes;
            result.interarrival_jitter = current.interarrival_jitter;
            result.gated = current.gated;
        }
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
        uint64_t handled_recovery_generation = 0;
        auto next_tick = SteadyClock::now() + kReceiverTickInterval;
        auto next_report =
            SteadyClock::now() + config.receiver_report_interval;
        auto next_twcc = SteadyClock::now() + kTwccReportInterval;

        try {
            reset_receiver(false);
            for (;;) {
                IngressPacket packet;
                bool have_packet = false;
                bool recover = false;
                {
                    std::unique_lock<std::mutex> lock(ingress_mutex);
                    const auto wake_at = config.twcc_enabled
                        ? std::min({next_tick, next_report, next_twcc})
                        : std::min(next_tick, next_report);
                    worker_cv.wait_until(lock, wake_at, [this,
                                                        handled_recovery_generation]() {
                        return stopping.load(std::memory_order_acquire)
                            || !ingress_queue.empty()
                            || recovery_generation
                                != handled_recovery_generation;
                    });
                    if (stopping.load(std::memory_order_acquire)) {
                        break;
                    }

                    if (recovery_generation
                        != handled_recovery_generation) {
                        handled_recovery_generation = recovery_generation;
                        recover = true;
                    } else if (!ingress_queue.empty()) {
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

                if (recover) {
                    reset_receiver(true);
                }

                auto now = SteadyClock::now();
                if (have_packet) {
                    if (packet.kind == IngressKind::Rtp) {
                        process_rtp(
                            packet.bytes.data(),
                            packet.bytes.size(),
                            now
                        );
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
                    publish_core_stats();
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
    uint64_t recovery_generation = 0;

    std::mutex output_mutex;
    std::deque<RtpVideoReceiverAccessUnit> output_queue;
    size_t output_queued_bytes_value = 0;
    bool output_awaiting_idr = true;

    std::mutex send_mutex;
    rtc::message_callback send_callback;

    std::mutex shutdown_mutex;
    bool shutdown_started = false;
    std::thread worker;

    std::unique_ptr<RtpH264Receiver> receiver;
    RtpH264Receiver::Stats completed_core;
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
    std::atomic<uint64_t> output_queued_access_units{0};
    std::atomic<uint64_t> output_queued_bytes{0};
    std::atomic<uint64_t> peak_output_queued_access_units{0};
    std::atomic<uint64_t> peak_output_queued_bytes{0};
    std::atomic<bool> awaiting_output_idr{true};

    std::atomic<uint64_t> nack_packets_sent{0};
    std::atomic<uint64_t> nack_sequences_sent{0};
    std::atomic<uint64_t> pli_requests{0};
    std::atomic<uint64_t> pli_packets_sent{0};
    std::atomic<uint64_t> remb_packets_sent{0};
    std::atomic<uint64_t> receiver_reports_sent{0};
    std::atomic<uint64_t> sender_reports_received{0};
    std::atomic<uint64_t> invalid_rtcp_packets{0};
    std::atomic<uint64_t> feedback_send_failures{0};
    std::atomic<uint64_t> core_restarts{0};
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

std::optional<RtpVideoReceiverAccessUnit>
RtpVideoReceiverSession::pop_access_unit() noexcept {
    const auto state = state_;
    if (!state) {
        return std::nullopt;
    }

    try {
        std::lock_guard<std::mutex> lock(state->output_mutex);
        if (state->output_queue.empty()) {
            return std::nullopt;
        }

        auto access_unit = std::move(state->output_queue.front());
        state->output_queued_bytes_value -= access_unit.annex_b.size();
        state->output_queue.pop_front();
        state->output_queued_access_units.store(
            static_cast<uint64_t>(state->output_queue.size()),
            std::memory_order_relaxed
        );
        state->output_queued_bytes.store(
            static_cast<uint64_t>(state->output_queued_bytes_value),
            std::memory_order_relaxed
        );
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
        if (state->output_queue.empty()) {
            size = 0;
            return RtpVideoReceiverPopResult::Empty;
        }

        const auto& access_unit = state->output_queue.front();
        size = access_unit.annex_b.size();
        is_idr = access_unit.is_idr;
        rtp_timestamp = access_unit.rtp_timestamp;
        if (buffer == nullptr || capacity < size) {
            return RtpVideoReceiverPopResult::BufferTooSmall;
        }

        std::memcpy(buffer, access_unit.annex_b.data(), size);
        state->output_queued_bytes_value -= size;
        state->output_queue.pop_front();
        state->output_queued_access_units.store(
            static_cast<uint64_t>(state->output_queue.size()),
            std::memory_order_relaxed
        );
        state->output_queued_bytes.store(
            static_cast<uint64_t>(state->output_queued_bytes_value),
            std::memory_order_relaxed
        );
        return RtpVideoReceiverPopResult::Ok;
    } catch (...) {
        size = 0;
        return RtpVideoReceiverPopResult::Empty;
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
    result.output_queued_access_units =
        state->output_queued_access_units.load(std::memory_order_relaxed);
    result.output_queued_bytes =
        state->output_queued_bytes.load(std::memory_order_relaxed);
    result.peak_output_queued_access_units =
        state->peak_output_queued_access_units.load(
            std::memory_order_relaxed
        );
    result.peak_output_queued_bytes =
        state->peak_output_queued_bytes.load(std::memory_order_relaxed);

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
    result.receiver_reports_sent =
        state->receiver_reports_sent.load(std::memory_order_relaxed);
    result.sender_reports_received =
        state->sender_reports_received.load(std::memory_order_relaxed);
    result.invalid_rtcp_packets =
        state->invalid_rtcp_packets.load(std::memory_order_relaxed);
    result.feedback_send_failures =
        state->feedback_send_failures.load(std::memory_order_relaxed);
    result.core_restarts =
        state->core_restarts.load(std::memory_order_relaxed);

    result.payload_type = state->config.payload_type;
    result.local_feedback_ssrc = state->local_feedback_ssrc;
    result.receive_target_bps =
        state->receive_target_bps.load(std::memory_order_relaxed);
    result.awaiting_output_idr =
        state->awaiting_output_idr.load(std::memory_order_relaxed);
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
