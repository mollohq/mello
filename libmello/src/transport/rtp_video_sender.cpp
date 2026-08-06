#include "rtp_video_sender.hpp"

#include "twcc.hpp"
#include "ulpfec.hpp"

#include <rtc/frameinfo.hpp>
#include <rtc/h264rtppacketizer.hpp>
#include <rtc/mediahandler.hpp>
#include <rtc/plihandler.hpp>
#include <rtc/rembhandler.hpp>
#include <rtc/rtcpsrreporter.hpp>
#include <rtc/rtp.hpp>
#include <rtc/track.hpp>

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <deque>
#include <mutex>
#include <stdexcept>
#include <thread>
#include <unordered_map>
#include <utility>
#include <vector>

namespace mello::transport {
namespace {

constexpr uint32_t kVideoClockRate = 90'000;
constexpr size_t kMaxFragmentPayload = 1'100;
constexpr size_t kNackCachePackets = 512;
// Repairs older than this cannot make the receiver's AU deadline; evicting
// them keeps the cache honest on high-RTT links instead of answering NACKs
// with packets the receiver has already gated on.
constexpr auto kNackCacheMaxAge = std::chrono::microseconds(1'000'000);
// Bound on retransmits queued for the pacing worker.
constexpr size_t kMaxRtxQueuePackets = 256;
constexpr uint64_t kMicrosPerSecond = 1'000'000;
constexpr uint64_t kRtpTimestampModulus = uint64_t{1} << 32;
constexpr uint64_t kNanosPerSecond = 1'000'000'000;

using SteadyClock = std::chrono::steady_clock;

uint16_t read_u16_be(const uint8_t* p) noexcept {
    return static_cast<uint16_t>((static_cast<uint16_t>(p[0]) << 8) | p[1]);
}

uint32_t read_u32_be(const uint8_t* p) noexcept {
    return (static_cast<uint32_t>(p[0]) << 24)
        | (static_cast<uint32_t>(p[1]) << 16)
        | (static_cast<uint32_t>(p[2]) << 8)
        | static_cast<uint32_t>(p[3]);
}

int64_t steady_now_us() noexcept {
    return std::chrono::duration_cast<std::chrono::microseconds>(
        SteadyClock::now().time_since_epoch()).count();
}

bool is_send_direction(rtc::Description::Direction direction) noexcept {
    return direction == rtc::Description::Direction::SendOnly
        || direction == rtc::Description::Direction::SendRecv;
}

bool is_h264_format(const std::string& format) noexcept {
    return format.size() == 4
        && (format[0] == 'H' || format[0] == 'h')
        && format[1] == '2'
        && format[2] == '6'
        && format[3] == '4';
}

bool is_valid_sender_track(const rtc::Track& track, uint8_t payload_type) {
    const auto description = track.description();
    if (description.type() != "video"
        || !is_send_direction(description.direction())
        || !description.hasPayloadType(payload_type)) {
        return false;
    }

    const auto* rtp_map = description.rtpMap(payload_type);
    return rtp_map != nullptr && is_h264_format(rtp_map->format);
}

size_t start_code_size_at(const uint8_t* data, size_t size, size_t offset) noexcept {
    if (offset + 4 <= size
        && data[offset] == 0
        && data[offset + 1] == 0
        && data[offset + 2] == 0
        && data[offset + 3] == 1) {
        return 4;
    }
    if (offset + 3 <= size
        && data[offset] == 0
        && data[offset + 1] == 0
        && data[offset + 2] == 1) {
        return 3;
    }
    return 0;
}

bool is_vcl_nal_type(uint8_t nal_type) noexcept {
    return (nal_type >= 1 && nal_type <= 5)
        || nal_type == 19
        || nal_type == 20
        || nal_type == 21;
}

struct AccessUnitInfo {
    bool valid = false;
    bool is_idr = false;
};

AccessUnitInfo inspect_annex_b_access_unit(
    const uint8_t* data,
    size_t size
) noexcept {
    if (data == nullptr || size < 4) {
        return {};
    }

    size_t offset = 0;
    size_t start_code_size = start_code_size_at(data, size, offset);
    if (start_code_size == 0) {
        return {};
    }

    bool has_vcl_nal = false;
    bool has_idr_nal = false;
    while (offset < size) {
        const size_t nal_start = offset + start_code_size;
        if (nal_start >= size || (data[nal_start] & 0x80) != 0) {
            return {};
        }

        const uint8_t nal_type = data[nal_start] & 0x1f;
        has_vcl_nal = has_vcl_nal || is_vcl_nal_type(nal_type);
        has_idr_nal = has_idr_nal || nal_type == 5;

        size_t next_start = nal_start;
        size_t next_start_code_size = 0;
        while (next_start < size) {
            next_start_code_size = start_code_size_at(data, size, next_start);
            if (next_start_code_size != 0) {
                break;
            }
            ++next_start;
        }

        if (next_start == nal_start) {
            return {};
        }
        if (next_start == size) {
            return {has_vcl_nal, has_idr_nal};
        }

        offset = next_start;
        start_code_size = next_start_code_size;
    }

    return {};
}

uint32_t capture_elapsed_to_rtp_ticks(uint64_t elapsed_us) noexcept {
    const uint64_t whole_seconds = elapsed_us / kMicrosPerSecond;
    const uint64_t fractional_us = elapsed_us % kMicrosPerSecond;

    const uint64_t whole_ticks =
        ((whole_seconds % kRtpTimestampModulus) * kVideoClockRate)
        % kRtpTimestampModulus;
    const uint64_t fractional_ticks =
        (fractional_us * kVideoClockRate + kMicrosPerSecond / 2)
        / kMicrosPerSecond;

    return static_cast<uint32_t>(
        (whole_ticks + fractional_ticks) % kRtpTimestampModulus
    );
}

void update_max(
    std::atomic<uint64_t>& destination,
    uint64_t value
) noexcept {
    uint64_t current = destination.load(std::memory_order_relaxed);
    while (current < value
           && !destination.compare_exchange_weak(
               current,
               value,
               std::memory_order_relaxed,
               std::memory_order_relaxed)) {
    }
}

uint64_t elapsed_micros(
    SteadyClock::time_point later,
    SteadyClock::time_point earlier
) noexcept {
    if (later <= earlier) {
        return 0;
    }
    return static_cast<uint64_t>(
        std::chrono::duration_cast<std::chrono::microseconds>(
            later - earlier
        ).count()
    );
}

SteadyClock::duration packet_interval(
    size_t wire_bytes,
    uint64_t bitrate_bps
) noexcept {
    const uint64_t numerator =
        static_cast<uint64_t>(wire_bytes) * 8 * kNanosPerSecond;
    uint64_t nanoseconds = numerator / bitrate_bps;
    if (numerator % bitrate_bps != 0) {
        ++nanoseconds;
    }
    return std::chrono::duration_cast<SteadyClock::duration>(
        std::chrono::nanoseconds(nanoseconds)
    );
}

class OutgoingRtpCapture final : public rtc::MediaHandler {
public:
    using CaptureCallback = std::function<void(
        rtc::message_vector&&,
        rtc::message_callback
    )>;

    explicit OutgoingRtpCapture(CaptureCallback callback)
        : callback_(std::move(callback)) {}

    void outgoing(
        rtc::message_vector& messages,
        const rtc::message_callback& send
    ) override {
        rtc::message_vector media_messages;
        rtc::message_vector remaining_messages;
        media_messages.reserve(messages.size());
        remaining_messages.reserve(messages.size());

        for (auto& message : messages) {
            if (message && message->type != rtc::Message::Control) {
                media_messages.push_back(std::move(message));
            } else {
                remaining_messages.push_back(std::move(message));
            }
        }

        if (!media_messages.empty()) {
            callback_(std::move(media_messages), send);
        }
        messages.swap(remaining_messages);
    }

private:
    CaptureCallback callback_;
};

// Feedback handler for the sender's inbound RTCP. Two duties:
//
// - Generic NACK (PT=205, FMT=1): unlike rtc::RtcpNackResponder, which
//   re-sends cached packets directly from the RTCP thread (bypassing the
//   pacing worker and its bandwidth accounting), retransmits are queued onto
//   the sender's pacing worker so repairs are paced and interleave safely
//   with fresh access units. Cache is count- and age-bounded.
// - TWCC (PT=205, FMT=15): parsed and forwarded to the delay-gradient
//   estimator on the sender State.
class MelloFeedbackHandler final : public rtc::MediaHandler {
public:
    using RtxCallback = std::function<void(rtc::message_ptr packet)>;
    using StatsCallback =
        std::function<void(uint64_t requests, uint64_t cache_misses)>;
    using TwccCallback = std::function<void(const TwccFeedback&)>;

    MelloFeedbackHandler(
        size_t max_packets,
        RtxCallback on_rtx,
        StatsCallback on_stats,
        TwccCallback on_twcc
    )
        : max_packets_(max_packets),
          on_rtx_(std::move(on_rtx)),
          on_stats_(std::move(on_stats)),
          on_twcc_(std::move(on_twcc)) {}

    void outgoing(
        rtc::message_vector& messages,
        const rtc::message_callback& /*send*/
    ) override {
        for (const auto& message : messages) {
            if (!message || message->type == rtc::Message::Control
                || message->size() < sizeof(rtc::RtpHeader)) {
                continue;
            }
            const auto* rtp =
                reinterpret_cast<const rtc::RtpHeader*>(message->data());
            const uint16_t seq = rtp->seqNumber();
            std::lock_guard<std::mutex> lock(cache_mutex_);
            evict_expired_locked();
            cache_order_.push_back(seq);
            cache_.emplace(seq, CacheEntry{message, SteadyClock::now()});
            while (cache_.size() > max_packets_) {
                cache_.erase(cache_order_.front());
                cache_order_.pop_front();
            }
        }
    }

    void incoming(
        rtc::message_vector& messages,
        const rtc::message_callback& /*send*/
    ) override {
        for (const auto& message : messages) {
            if (!message || message->type != rtc::Message::Control) {
                continue;
            }
            size_t offset = 0;
            const uint8_t* data =
                reinterpret_cast<const uint8_t*>(message->data());
            while (offset + 4 <= message->size()) {
                const uint8_t payload_type = data[offset + 1];
                const uint8_t fmt = data[offset] & 0x1f;
                const size_t packet_len =
                    (static_cast<size_t>(read_u16_be(data + offset + 2)) + 1)
                    * 4;
                if (packet_len < 8 || offset + packet_len > message->size()) {
                    break;
                }
                if (payload_type == 205 && fmt == 1
                    && packet_len >= sizeof(rtc::RtcpNack)) {
                    handle_nack(
                        *reinterpret_cast<rtc::RtcpNack*>(
                            message->data() + offset)
                    );
                } else if (payload_type == 205 && fmt == 15 && on_twcc_) {
                    TwccFeedback feedback;
                    if (parse_twcc_feedback(
                            data + offset,
                            packet_len,
                            feedback)) {
                        on_twcc_(feedback);
                    }
                }
                offset += packet_len;
            }
        }
    }

private:
    struct CacheEntry {
        rtc::message_ptr packet;
        SteadyClock::time_point stored_at;
    };

    void handle_nack(rtc::RtcpNack& nack) {
        uint64_t requests = 0;
        uint64_t misses = 0;
        const unsigned int field_count = nack.getSeqNoCount();
        for (unsigned int i = 0; i < field_count; ++i) {
            for (const uint16_t seq : nack.parts[i].getSequenceNumbers()) {
                ++requests;
                rtc::message_ptr packet;
                {
                    std::lock_guard<std::mutex> lock(cache_mutex_);
                    evict_expired_locked();
                    const auto it = cache_.find(seq);
                    if (it != cache_.end()) {
                        packet = it->second.packet;
                    }
                }
                if (packet) {
                    on_rtx_(std::move(packet));
                } else {
                    ++misses;
                }
            }
        }
        if (on_stats_) {
            on_stats_(requests, misses);
        }
    }

    void evict_expired_locked() {
        const auto cutoff = SteadyClock::now() - kNackCacheMaxAge;
        while (!cache_order_.empty()) {
            const auto it = cache_.find(cache_order_.front());
            if (it == cache_.end() || it->second.stored_at >= cutoff) {
                break;
            }
            cache_.erase(it);
            cache_order_.pop_front();
        }
    }

    const size_t max_packets_;
    RtxCallback on_rtx_;
    StatsCallback on_stats_;
    TwccCallback on_twcc_;
    std::mutex cache_mutex_;
    std::unordered_map<uint16_t, CacheEntry> cache_;
    std::deque<uint16_t> cache_order_;
};

} // namespace

struct RtpVideoSender::State
    : public std::enable_shared_from_this<RtpVideoSender::State> {
    struct AccessUnit {
        std::vector<uint8_t> bytes;
        uint32_t rtp_timestamp = 0;
        bool is_idr = false;
    };

    struct PacedBatch {
        rtc::message_vector packets;
        rtc::message_callback send;
        SteadyClock::time_point captured_at;
    };

    State(
        uint64_t initial_pacing_target_bps,
        PliCallback pli_callback,
        RembCallback remb_callback,
        LocalIdrNeededCallback local_idr_needed_callback,
        GccTargetCallback gcc_target_callback,
        bool twcc_enabled,
        bool fec_enabled,
        uint32_t media_ssrc
    )
        : pacing_target_bps(initial_pacing_target_bps),
          on_pli(std::move(pli_callback)),
          on_remb(std::move(remb_callback)),
          on_local_idr_needed(std::move(local_idr_needed_callback)),
          on_gcc_target(std::move(gcc_target_callback)),
          twcc_enabled(twcc_enabled),
          fec_enabled(fec_enabled),
          fec_media_ssrc(media_ssrc) {
        if (twcc_enabled) {
            estimator = std::make_unique<GccEstimator>(
                GccEstimator::Config{},
                initial_pacing_target_bps
            );
        }
    }

    ~State() {
        shutdown();
        attached.store(false, std::memory_order_release);
        try {
            if (track
                && root_handler
                && track->getMediaHandler() == root_handler) {
                track->setMediaHandler(nullptr);
            }
        } catch (...) {
        }
    }

    void start_worker() {
        const auto self = shared_from_this();
        worker = std::thread([self]() noexcept {
            self->worker_main();
        });
    }

    void shutdown() noexcept {
        stopping.store(true, std::memory_order_release);
        queue_cv.notify_all();
        pacing_cv.notify_all();

        std::lock_guard<std::mutex> lock(shutdown_mutex);
        if (!worker.joinable()) {
            return;
        }

        try {
            if (worker.get_id() == std::this_thread::get_id()) {
                worker.detach();
            } else {
                worker.join();
            }
        } catch (...) {
        }
    }

    bool track_is_open() const noexcept {
        try {
            return attached.load(std::memory_order_acquire)
                && !stopping.load(std::memory_order_acquire)
                && track
                && !track->isClosed()
                && track->isOpen()
                && is_send_direction(track->direction());
        } catch (...) {
            return false;
        }
    }

    void record_pli() noexcept {
        pli_requests.fetch_add(1, std::memory_order_relaxed);
        try {
            std::lock_guard<std::mutex> lock(callback_mutex);
            if (on_pli) {
                on_pli();
            }
        } catch (...) {
        }
    }

    void record_remb(uint32_t bitrate_bps) noexcept {
        latest_remb_bitrate_bps.store(bitrate_bps, std::memory_order_relaxed);
        remb_reports.fetch_add(1, std::memory_order_relaxed);
        try {
            std::lock_guard<std::mutex> lock(callback_mutex);
            if (on_remb) {
                on_remb(bitrate_bps);
            }
        } catch (...) {
        }
    }

    void record_local_idr_needed() noexcept {
        local_idr_requests.fetch_add(1, std::memory_order_relaxed);
        try {
            std::lock_guard<std::mutex> lock(callback_mutex);
            if (on_local_idr_needed) {
                on_local_idr_needed();
            }
        } catch (...) {
        }
    }

    void capture_batch(
        rtc::message_vector&& packets,
        rtc::message_callback send
    ) {
        PacedBatch next_batch;
        next_batch.packets = std::move(packets);
        next_batch.send = std::move(send);
        next_batch.captured_at = SteadyClock::now();

        std::lock_guard<std::mutex> lock(batch_mutex);
        if (batch_ready) {
            throw std::logic_error("RTP pacing batch is already occupied");
        }
        send_callback = next_batch.send;
        paced_batch = std::move(next_batch);
        batch_ready = true;
        batch_cv.notify_one();
    }

    void enqueue_rtx(rtc::message_ptr packet) {
        {
            std::lock_guard<std::mutex> lock(queue_mutex);
            if (rtx_queue.size() >= kMaxRtxQueuePackets) {
                rtx_queue.pop_front();
                rtx_queue_dropped.fetch_add(1, std::memory_order_relaxed);
            }
            rtx_queue.push_back(std::move(packet));
        }
        queue_cv.notify_one();
    }

    void record_rtx_stats(uint64_t requests, uint64_t cache_misses) noexcept {
        rtx_requests.fetch_add(requests, std::memory_order_relaxed);
        rtx_cache_misses.fetch_add(cache_misses, std::memory_order_relaxed);
    }

    // Pacing budget: min(manager ceiling, estimator target) when TWCC is on.
    uint64_t effective_target_bps() const noexcept {
        const uint64_t manager_target =
            pacing_target_bps.load(std::memory_order_relaxed);
        if (!twcc_enabled) {
            return manager_target;
        }
        const uint64_t gcc = gcc_target_bps.load(std::memory_order_relaxed);
        if (gcc == 0 || gcc >= manager_target) {
            return manager_target;
        }
        // When the estimator is pinned near its floor (startup false overuse
        // on low-latency hops such as localhost host→SFU), keep the manager
        // ceiling. Viewer-path REMB owns encoder adaptation upstream.
        if (gcc <= manager_target / 4) {
            return manager_target;
        }
        return gcc;
    }

    // Stamps a packet with a transport-wide sequence + send time. Returns a
    // stamped copy (the input may be shared with the NACK cache); falls back
    // to the original on failure or when TWCC is off. Worker thread only.
    rtc::message_ptr stamp_outgoing(const rtc::message_ptr& packet) {
        if (!twcc_enabled || !packet
            || packet->type == rtc::Message::Control) {
            return packet;
        }
        std::vector<uint8_t> bytes;
        bytes.reserve(packet->size() + 8);
        for (const std::byte b : *packet) {
            bytes.push_back(static_cast<uint8_t>(b));
        }
        if (!stamper.stamp(bytes, steady_now_us())) {
            return packet;
        }
        rtc::binary data;
        data.reserve(bytes.size());
        for (const uint8_t b : bytes) {
            data.push_back(static_cast<std::byte>(b));
        }
        return std::make_shared<rtc::Message>(std::move(data), packet->type);
    }

    // RTCP thread: feed one parsed TWCC report into the estimator and
    // publish significant target changes (pacer wake + Rust feedback).
    void handle_twcc_feedback(const TwccFeedback& feedback) {
        if (!estimator) {
            return;
        }
        {
            std::lock_guard<std::mutex> lock(estimator_mutex);
            for (const auto& result : feedback.packets) {
                int64_t send_time = -1;
                if (result.received) {
                    stamper.send_time_for(result.sequence, send_time);
                }
                estimator->on_packet(
                    result.sequence,
                    result.received,
                    send_time,
                    result.arrival_time_us
                );
            }
        }
        twcc_reports.fetch_add(1, std::memory_order_relaxed);

        const uint64_t target = estimator->target_bps();
        const uint64_t previous =
            gcc_target_bps.load(std::memory_order_relaxed);
        if (target == previous) {
            return;
        }
        gcc_target_bps.store(target, std::memory_order_relaxed);
        pacing_cv.notify_all();
        // Forward to the host manager on the first estimate and on changes
        // beyond ~3% — the manager applies GCC targets to the encoder.
        if (previous == 0
            || target > previous + previous / 32
            || target + previous / 32 < previous) {
            try {
                std::lock_guard<std::mutex> lock(callback_mutex);
                if (on_gcc_target) {
                    on_gcc_target(static_cast<uint32_t>(
                        std::min<uint64_t>(target, UINT32_MAX)));
                }
            } catch (...) {
            }
        }
    }

    bool take_batch(PacedBatch& destination) noexcept {
        std::unique_lock<std::mutex> lock(batch_mutex);
        batch_cv.wait_for(lock, std::chrono::milliseconds(250), [this]() {
            return batch_ready
                || stopping.load(std::memory_order_acquire);
        });
        if (!batch_ready) {
            return false;
        }
        destination = std::move(paced_batch);
        paced_batch = {};
        batch_ready = false;
        return true;
    }

    void discard_batch() noexcept {
        std::lock_guard<std::mutex> lock(batch_mutex);
        paced_batch = {};
        batch_ready = false;
    }

    // Per-packet leaky bucket: each packet waits for its own slot so a frame
    // of fragments is spread across its wire time instead of bursting. A
    // two-packet-interval lag allowance absorbs scheduler/timer granularity
    // without letting bursts grow unbounded. Returns false when stopping.
    bool wait_for_slot(uint64_t wire_bytes) noexcept {
        std::unique_lock<std::mutex> lock(pacing_mutex);
        for (;;) {
            if (stopping.load(std::memory_order_acquire)) {
                return false;
            }

            const uint64_t observed_target = effective_target_bps();
            const auto interval =
                packet_interval(static_cast<size_t>(wire_bytes), observed_target);
            const auto now = SteadyClock::now();

            if (!has_next_slot) {
                next_slot = now + interval;
                has_next_slot = true;
                return true;
            }

            const auto max_lag = interval * 2;
            if (next_slot + max_lag < now) {
                next_slot = now - max_lag;
            }
            if (now >= next_slot) {
                next_slot += interval;
                return true;
            }

            const auto deadline = next_slot;
            pacing_cv.wait_until(lock, deadline, [this, observed_target]() {
                return stopping.load(std::memory_order_acquire)
                    || effective_target_bps() != observed_target;
            });
        }
    }

    // Feeds one just-sent media packet (the ORIGINAL pre-TWCC-stamp bytes —
    // the stamped copy only exists on the wire) into the parity generator.
    // When the group closes, the FEC packet is paced through the same budget
    // and sent down the same path as media, TWCC-stamped like any egress
    // packet. Retransmits never reach this: they are not parity-protected.
    // Worker thread only.
    void feed_fec(
        const rtc::message_ptr& packet,
        const rtc::message_callback& send
    ) noexcept {
        try {
            const auto* const bytes =
                reinterpret_cast<const uint8_t*>(packet->data());
            const size_t size = packet->size();
            if (size >= 12) {
                fec_last_rtp_timestamp = read_u32_be(bytes + 4);
            }
            fec_generator.add_packet(bytes, size);
            if (fec_generator.pending() != 0) {
                return;
            }

            const auto fec = fec_generator.build_packet(
                fec_media_ssrc,
                fec_last_rtp_timestamp
            );
            if (fec.empty()) {
                // Non-contiguous group: discarded without emitting.
                return;
            }
            if (!wait_for_slot(static_cast<uint64_t>(fec.size()))) {
                return;
            }

            rtc::binary data;
            data.reserve(fec.size());
            for (const uint8_t byte : fec) {
                data.push_back(static_cast<std::byte>(byte));
            }
            send(stamp_outgoing(std::make_shared<rtc::Message>(
                std::move(data),
                rtc::Message::Binary
            )));
            fec_packets_sent.fetch_add(1, std::memory_order_relaxed);
            rtp_packets_sent.fetch_add(1, std::memory_order_relaxed);
            rtp_wire_bytes_sent.fetch_add(
                static_cast<uint64_t>(fec.size()),
                std::memory_order_relaxed
            );
        } catch (...) {
            send_failures.fetch_add(1, std::memory_order_relaxed);
        }
    }

    bool pace_batch(PacedBatch& batch) noexcept {
        bool all_packets_sent = true;

        for (const auto& packet : batch.packets) {
            if (stopping.load(std::memory_order_acquire)) {
                return false;
            }

            if (!packet || !batch.send) {
                send_failures.fetch_add(1, std::memory_order_relaxed);
                all_packets_sent = false;
                continue;
            }

            const uint64_t wire_bytes =
                static_cast<uint64_t>(packet->size());
            if (!wait_for_slot(wire_bytes)) {
                return false;
            }
            try {
                batch.send(stamp_outgoing(packet));
                rtp_packets_sent.fetch_add(1, std::memory_order_relaxed);
                rtp_wire_bytes_sent.fetch_add(
                    wire_bytes,
                    std::memory_order_relaxed
                );
                if (fec_enabled) {
                    feed_fec(packet, batch.send);
                }
            } catch (...) {
                send_failures.fetch_add(1, std::memory_order_relaxed);
                all_packets_sent = false;
            }
        }

        const uint64_t pacing_delay =
            elapsed_micros(SteadyClock::now(), batch.captured_at);
        current_pacing_delay_us.store(pacing_delay, std::memory_order_relaxed);
        update_max(max_pacing_delay_us, pacing_delay);
        return all_packets_sent;
    }

    // Retransmits are time-critical (receiver AU deadlines are short), so
    // they drain ahead of fresh access units — but through the same pacing
    // budget. Unpaced repair bursts amplify the congestion that caused the
    // loss.
    void send_pending_rtx() noexcept {
        for (;;) {
            rtc::message_ptr packet;
            {
                std::lock_guard<std::mutex> lock(queue_mutex);
                if (rtx_queue.empty()) {
                    return;
                }
                packet = std::move(rtx_queue.front());
                rtx_queue.pop_front();
            }
            if (!packet) {
                continue;
            }
            rtc::message_callback send;
            {
                std::lock_guard<std::mutex> lock(batch_mutex);
                send = send_callback;
            }
            if (!send || !track_is_open()) {
                send_failures.fetch_add(1, std::memory_order_relaxed);
                continue;
            }
            const uint64_t wire_bytes =
                static_cast<uint64_t>(packet->size());
            if (!wait_for_slot(wire_bytes)) {
                return;
            }
            try {
                // Retransmits are re-stamped with a fresh transport-wide
                // sequence (send time = now), like WebRTC RTX.
                send(stamp_outgoing(packet));
                rtx_sent.fetch_add(1, std::memory_order_relaxed);
                rtp_packets_sent.fetch_add(1, std::memory_order_relaxed);
                rtp_wire_bytes_sent.fetch_add(
                    wire_bytes,
                    std::memory_order_relaxed
                );
            } catch (...) {
                send_failures.fetch_add(1, std::memory_order_relaxed);
            }
        }
    }

    void worker_main() noexcept {
        for (;;) {
            {
                std::unique_lock<std::mutex> lock(queue_mutex);
                queue_cv.wait(lock, [this]() {
                    return stopping.load(std::memory_order_acquire)
                        || !access_unit_queue.empty()
                        || !rtx_queue.empty();
                });
            }
            if (stopping.load(std::memory_order_acquire)) {
                return;
            }

            // Time-critical repairs drain ahead of fresh access units.
            send_pending_rtx();

            AccessUnit access_unit;
            {
                std::lock_guard<std::mutex> lock(queue_mutex);
                if (access_unit_queue.empty()) {
                    continue;
                }

                access_unit = std::move(access_unit_queue.front());
                queued_bytes_value -= access_unit.bytes.size();
                access_unit_queue.pop_front();
                queued_access_units.store(
                    static_cast<uint64_t>(access_unit_queue.size()),
                    std::memory_order_relaxed
                );
                queued_bytes.store(
                    static_cast<uint64_t>(queued_bytes_value),
                    std::memory_order_relaxed
                );
            }

            if (!track_is_open()) {
                send_failures.fetch_add(1, std::memory_order_relaxed);
                continue;
            }

            try {
                track->sendFrame(
                    reinterpret_cast<const std::byte*>(
                        access_unit.bytes.data()
                    ),
                    access_unit.bytes.size(),
                    rtc::FrameInfo(access_unit.rtp_timestamp)
                );
            } catch (...) {
                discard_batch();
                send_failures.fetch_add(1, std::memory_order_relaxed);
                continue;
            }

            PacedBatch batch;
            if (!take_batch(batch)) {
                if (!stopping.load(std::memory_order_acquire)) {
                    send_failures.fetch_add(1, std::memory_order_relaxed);
                }
                continue;
            }

            if (pace_batch(batch)) {
                access_units_sent.fetch_add(1, std::memory_order_relaxed);
                bytes_sent.fetch_add(
                    static_cast<uint64_t>(access_unit.bytes.size()),
                    std::memory_order_relaxed
                );
            }
        }
    }

    std::shared_ptr<rtc::Track> track;
    std::shared_ptr<rtc::MediaHandler> root_handler;
    std::atomic<bool> attached{false};
    std::atomic<bool> stopping{false};
    uint32_t rtp_start_timestamp = 0;

    std::mutex queue_mutex;
    std::condition_variable queue_cv;
    std::deque<AccessUnit> access_unit_queue;
    size_t queued_bytes_value = 0;
    bool timestamp_initialized = false;
    uint64_t first_capture_timestamp_us = 0;
    uint64_t last_capture_timestamp_us = 0;
    bool awaiting_idr = false;

    std::mutex batch_mutex;
    std::condition_variable batch_cv;
    PacedBatch paced_batch;
    bool batch_ready = false;
    // Transport send path captured from the packetizer chain; the pacing
    // worker uses it to emit retransmits through the same socket as fresh
    // access units.
    rtc::message_callback send_callback;

    std::mutex pacing_mutex;
    std::condition_variable pacing_cv;
    std::atomic<uint64_t> pacing_target_bps{0};
    // Per-packet leaky-bucket state (worker thread only, guarded by
    // pacing_mutex because the target is updated cross-thread).
    SteadyClock::time_point next_slot{};
    bool has_next_slot = false;

    std::mutex shutdown_mutex;
    std::thread worker;

    std::mutex callback_mutex;
    PliCallback on_pli;
    RembCallback on_remb;
    LocalIdrNeededCallback on_local_idr_needed;
    GccTargetCallback on_gcc_target;

    // TWCC: stamps egress (worker thread) and runs the delay-gradient
    // estimator from feedback (RTCP thread, guarded by estimator_mutex).
    const bool twcc_enabled = false;
    TwccSendStamper stamper;
    std::unique_ptr<GccEstimator> estimator;
    std::mutex estimator_mutex;
    std::atomic<uint64_t> gcc_target_bps{0};
    std::atomic<uint64_t> twcc_reports{0};

    // ULPFEC parity generation (worker thread only): one parity packet per
    // completed group of kDefaultFecGroupSize media packets, on SSRC+1.
    const bool fec_enabled = false;
    const uint32_t fec_media_ssrc = 0;
    UlpfecGenerator fec_generator;
    uint32_t fec_last_rtp_timestamp = 0;
    std::atomic<uint64_t> fec_packets_sent{0};

    std::atomic<uint64_t> access_units_enqueued{0};
    std::atomic<uint64_t> access_units_sent{0};
    std::atomic<uint64_t> access_units_dropped{0};
    std::atomic<uint64_t> access_units_rejected{0};
    std::atomic<uint64_t> bytes_sent{0};
    std::atomic<uint64_t> send_failures{0};
    std::atomic<uint64_t> rtp_packets_sent{0};
    std::atomic<uint64_t> rtp_wire_bytes_sent{0};
    std::atomic<uint64_t> queued_access_units{0};
    std::atomic<uint64_t> peak_queued_access_units{0};
    std::atomic<uint64_t> queued_bytes{0};
    std::atomic<uint64_t> peak_queued_bytes{0};
    std::atomic<uint64_t> current_pacing_delay_us{0};
    std::atomic<uint64_t> max_pacing_delay_us{0};
    std::atomic<uint64_t> local_idr_requests{0};

    // Retransmits requested by inbound NACKs, drained by the pacing worker
    // ahead of fresh access units but through the same pacing budget.
    std::deque<rtc::message_ptr> rtx_queue; // under queue_mutex
    std::atomic<uint64_t> rtx_requests{0};
    std::atomic<uint64_t> rtx_sent{0};
    std::atomic<uint64_t> rtx_cache_misses{0};
    std::atomic<uint64_t> rtx_queue_dropped{0};
    std::atomic<uint64_t> pli_requests{0};
    std::atomic<uint64_t> remb_reports{0};
    std::atomic<uint32_t> latest_remb_bitrate_bps{0};
};

RtpVideoSender::RtpVideoSender(
    std::shared_ptr<rtc::Track> track,
    RtpVideoSenderConfig config,
    PliCallback on_pli,
    RembCallback on_remb,
    LocalIdrNeededCallback on_local_idr_needed,
    GccTargetCallback on_gcc_target
) noexcept {
    try {
        auto state = std::make_shared<State>(
            config.pacing_target_bps,
            std::move(on_pli),
            std::move(on_remb),
            std::move(on_local_idr_needed),
            std::move(on_gcc_target),
            config.twcc_enabled,
            config.fec_enabled,
            config.ssrc
        );
        state_ = state;

        if (!track
            || config.ssrc == 0
            || config.payload_type > 127
            || config.cname.empty()
            || config.cname.size() > 255
            || config.pacing_target_bps == 0
            || track->isClosed()
            || !is_valid_sender_track(*track, config.payload_type)) {
            return;
        }

        auto rtp_config = std::make_shared<rtc::RtpPacketizationConfig>(
            config.ssrc,
            std::move(config.cname),
            config.payload_type,
            kVideoClockRate
        );
        auto packetizer = std::make_shared<rtc::H264RtpPacketizer>(
            rtc::NalUnit::Separator::StartSequence,
            rtp_config,
            kMaxFragmentPayload
        );
        packetizer->addToChain(
            std::make_shared<rtc::RtcpSrReporter>(rtp_config)
        );

        const std::weak_ptr<State> weak_state = state;
        packetizer->addToChain(std::make_shared<MelloFeedbackHandler>(
            kNackCachePackets,
            [weak_state](rtc::message_ptr packet) {
                if (const auto locked = weak_state.lock()) {
                    locked->enqueue_rtx(std::move(packet));
                }
            },
            [weak_state](uint64_t requests, uint64_t cache_misses) {
                if (const auto locked = weak_state.lock()) {
                    locked->record_rtx_stats(requests, cache_misses);
                }
            },
            [weak_state](const TwccFeedback& feedback) {
                if (const auto locked = weak_state.lock()) {
                    locked->handle_twcc_feedback(feedback);
                }
            }
        ));
        packetizer->addToChain(std::make_shared<rtc::PliHandler>(
            [weak_state]() {
                if (const auto locked = weak_state.lock()) {
                    locked->record_pli();
                }
            }
        ));
        packetizer->addToChain(std::make_shared<rtc::RembHandler>(
            [weak_state](unsigned int bitrate_bps) {
                if (const auto locked = weak_state.lock()) {
                    locked->record_remb(static_cast<uint32_t>(bitrate_bps));
                }
            }
        ));
        packetizer->addToChain(std::make_shared<OutgoingRtpCapture>(
            [weak_state](
                rtc::message_vector&& packets,
                rtc::message_callback send
            ) {
                if (const auto locked = weak_state.lock()) {
                    locked->capture_batch(
                        std::move(packets),
                        std::move(send)
                    );
                }
            }
        ));

        state->track = std::move(track);
        state->root_handler = packetizer;
        state->rtp_start_timestamp = rtp_config->startTimestamp;
        state->start_worker();
        state->track->setMediaHandler(packetizer);
        state->attached.store(true, std::memory_order_release);
    } catch (...) {
        if (state_) {
            state_->shutdown();
        }
    }
}

RtpVideoSender::~RtpVideoSender() {
    const auto state = std::move(state_);
    if (state) {
        state->shutdown();
    }
}

RtpVideoSender::RtpVideoSender(RtpVideoSender&&) noexcept = default;

RtpVideoSender& RtpVideoSender::operator=(RtpVideoSender&& other) noexcept {
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

SendAccessUnitResult RtpVideoSender::send_access_unit(
    const uint8_t* annex_b,
    size_t size,
    uint64_t capture_timestamp_us
) noexcept {
    const auto state = state_;
    if (!state) {
        return SendAccessUnitResult::Failed;
    }

    const auto fail = [&state]() noexcept {
        state->send_failures.fetch_add(1, std::memory_order_relaxed);
        return SendAccessUnitResult::Failed;
    };

    const auto backpressure = [&state]() noexcept {
        state->access_units_rejected.fetch_add(1, std::memory_order_relaxed);
        return SendAccessUnitResult::Backpressure;
    };

    if (!state->track_is_open()) {
        return fail();
    }

    const AccessUnitInfo access_unit_info =
        inspect_annex_b_access_unit(annex_b, size);
    if (!access_unit_info.valid) {
        return fail();
    }

    State::AccessUnit access_unit;
    access_unit.is_idr = access_unit_info.is_idr;
    if (size <= kMaxQueuedBytes) {
        try {
            access_unit.bytes.assign(annex_b, annex_b + size);
        } catch (...) {
            return fail();
        }
    }

    bool notify_local_idr_needed = false;
    std::unique_lock<std::mutex> lock(state->queue_mutex);
    if (!state->track_is_open()) {
        return fail();
    }

    if (state->timestamp_initialized
        && capture_timestamp_us < state->last_capture_timestamp_us) {
        return fail();
    }

    const auto update_queue_stats = [&state]() noexcept {
        const uint64_t queue_size = static_cast<uint64_t>(
            state->access_unit_queue.size()
        );
        const uint64_t queue_bytes =
            static_cast<uint64_t>(state->queued_bytes_value);
        state->queued_access_units.store(
            queue_size,
            std::memory_order_relaxed
        );
        state->queued_bytes.store(
            queue_bytes,
            std::memory_order_relaxed
        );
        update_max(state->peak_queued_access_units, queue_size);
        update_max(state->peak_queued_bytes, queue_bytes);
    };

    const auto drop_queued_deltas = [&state]() noexcept {
        uint64_t dropped = 0;
        auto iterator = state->access_unit_queue.begin();
        while (iterator != state->access_unit_queue.end()) {
            if (iterator->is_idr) {
                ++iterator;
                continue;
            }
            state->queued_bytes_value -= iterator->bytes.size();
            iterator = state->access_unit_queue.erase(iterator);
            ++dropped;
        }
        state->access_units_dropped.fetch_add(
            dropped,
            std::memory_order_relaxed
        );
    };

    const auto enter_idr_gate = [&state, &notify_local_idr_needed]() noexcept {
        if (!state->awaiting_idr) {
            state->awaiting_idr = true;
            notify_local_idr_needed = true;
        }
    };

    if (!access_unit_info.is_idr && state->awaiting_idr) {
        lock.unlock();
        return backpressure();
    }

    if (!access_unit_info.is_idr) {
        if (state->access_unit_queue.size() >= kMaxQueuedAccessUnits
            || size > kMaxQueuedBytes - state->queued_bytes_value) {
            drop_queued_deltas();
            enter_idr_gate();
            update_queue_stats();
            lock.unlock();
            if (notify_local_idr_needed) {
                state->record_local_idr_needed();
            }
            return backpressure();
        }
    }

    if (size > kMaxQueuedBytes) {
        drop_queued_deltas();
        enter_idr_gate();
        update_queue_stats();
        lock.unlock();
        if (notify_local_idr_needed) {
            state->record_local_idr_needed();
        }
        return backpressure();
    }

    uint32_t rtp_timestamp = state->rtp_start_timestamp;
    if (state->timestamp_initialized) {
        const uint64_t elapsed_us =
            capture_timestamp_us - state->first_capture_timestamp_us;
        rtp_timestamp += capture_elapsed_to_rtp_ticks(elapsed_us);
    }
    access_unit.rtp_timestamp = rtp_timestamp;

    try {
        if (access_unit_info.is_idr) {
            std::deque<State::AccessUnit> replacement;
            replacement.push_back(std::move(access_unit));
            const uint64_t dropped = static_cast<uint64_t>(
                state->access_unit_queue.size()
            );
            state->access_unit_queue.swap(replacement);
            state->queued_bytes_value = size;
            state->access_units_dropped.fetch_add(
                dropped,
                std::memory_order_relaxed
            );
            state->awaiting_idr = false;
        } else {
            state->access_unit_queue.push_back(std::move(access_unit));
            state->queued_bytes_value += size;
        }
    } catch (...) {
        return fail();
    }

    if (!state->timestamp_initialized) {
        state->first_capture_timestamp_us = capture_timestamp_us;
        state->timestamp_initialized = true;
    }
    state->last_capture_timestamp_us = capture_timestamp_us;
    state->access_units_enqueued.fetch_add(1, std::memory_order_relaxed);
    update_queue_stats();
    lock.unlock();
    state->queue_cv.notify_one();
    return SendAccessUnitResult::Accepted;
}

bool RtpVideoSender::set_pacing_target_bps(
    uint64_t bitrate_bps
) noexcept {
    const auto state = state_;
    if (!state || bitrate_bps == 0
        || !state->attached.load(std::memory_order_acquire)
        || state->stopping.load(std::memory_order_acquire)) {
        if (state) {
            state->send_failures.fetch_add(1, std::memory_order_relaxed);
        }
        return false;
    }

    state->pacing_target_bps.store(bitrate_bps, std::memory_order_relaxed);
    state->pacing_cv.notify_all();
    return true;
}

bool RtpVideoSender::is_open() const noexcept {
    const auto state = state_;
    return state && state->track_is_open();
}

RtpVideoSenderStats RtpVideoSender::stats() const noexcept {
    RtpVideoSenderStats result;
    const auto state = state_;
    if (!state) {
        return result;
    }

    result.access_units_enqueued =
        state->access_units_enqueued.load(std::memory_order_relaxed);
    result.access_units_sent =
        state->access_units_sent.load(std::memory_order_relaxed);
    result.access_units_dropped =
        state->access_units_dropped.load(std::memory_order_relaxed);
    result.access_units_rejected =
        state->access_units_rejected.load(std::memory_order_relaxed);
    result.bytes_sent =
        state->bytes_sent.load(std::memory_order_relaxed);
    result.send_failures =
        state->send_failures.load(std::memory_order_relaxed);
    result.rtp_packets_sent =
        state->rtp_packets_sent.load(std::memory_order_relaxed);
    result.rtp_wire_bytes_sent =
        state->rtp_wire_bytes_sent.load(std::memory_order_relaxed);
    result.queued_access_units =
        state->queued_access_units.load(std::memory_order_relaxed);
    result.peak_queued_access_units =
        state->peak_queued_access_units.load(std::memory_order_relaxed);
    result.queued_bytes =
        state->queued_bytes.load(std::memory_order_relaxed);
    result.peak_queued_bytes =
        state->peak_queued_bytes.load(std::memory_order_relaxed);
    result.pacing_target_bps =
        state->pacing_target_bps.load(std::memory_order_relaxed);
    result.current_pacing_delay_us =
        state->current_pacing_delay_us.load(std::memory_order_relaxed);
    result.max_pacing_delay_us =
        state->max_pacing_delay_us.load(std::memory_order_relaxed);
    result.local_idr_requests =
        state->local_idr_requests.load(std::memory_order_relaxed);
    result.pli_requests =
        state->pli_requests.load(std::memory_order_relaxed);
    result.remb_reports =
        state->remb_reports.load(std::memory_order_relaxed);
    result.latest_remb_bitrate_bps =
        state->latest_remb_bitrate_bps.load(std::memory_order_relaxed);
    result.rtx_requests =
        state->rtx_requests.load(std::memory_order_relaxed);
    result.rtx_sent =
        state->rtx_sent.load(std::memory_order_relaxed);
    result.rtx_cache_misses =
        state->rtx_cache_misses.load(std::memory_order_relaxed);
    result.rtx_queue_dropped =
        state->rtx_queue_dropped.load(std::memory_order_relaxed);
    result.twcc_reports =
        state->twcc_reports.load(std::memory_order_relaxed);
    result.gcc_target_bps =
        state->gcc_target_bps.load(std::memory_order_relaxed);
    result.fec_packets_sent =
        state->fec_packets_sent.load(std::memory_order_relaxed);
    return result;
}

} // namespace mello::transport
