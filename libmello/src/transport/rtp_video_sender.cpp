#include "rtp_video_sender.hpp"

#include "twcc.hpp"

#include <rtc/frameinfo.hpp>
#include <rtc/h264rtppacketizer.hpp>
#include <rtc/mediahandler.hpp>
#include <rtc/plihandler.hpp>
#include <rtc/rembhandler.hpp>
#include <rtc/rtcpsrreporter.hpp>
#include <rtc/rtp.hpp>
#include <rtc/track.hpp>

#include <algorithm>
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
// Retransmit limits, libwebrtc's two rules (RtpPacketHistory and the
// retransmission RateLimiter of RtpTransportControllerSend):
// - a packet is resent at most once per round trip. A receiver repeats its
//   NACK until the repair arrives, and every repeat inside one round trip
//   asks for a repair that is already on its way. Until the first round
//   trip sample the interval is kDefaultResendIntervalUs.
// - retransmits use at most the target rate over a 500 ms window.
// Without them a cap change made 5 to 7 Mbps of retransmits into a 4.8 Mbps
// link (stage 4 gate, hw congested): the loss doubled and the goog_cc
// loss-based estimate went to 450 kbps.
constexpr int64_t kMinResendIntervalUs = 5'000;
constexpr int64_t kDefaultResendIntervalUs = 100'000;
constexpr int64_t kRetransmitRateWindowUs = 500'000;
constexpr size_t kMaxResendRecords = 4'096;
constexpr uint64_t kMicrosPerSecond = 1'000'000;
constexpr uint64_t kRtpTimestampModulus = uint64_t{1} << 32;

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
//   the sender's pacing worker, which sends them before the next media
//   packet (one packet at a time, so repairs interleave with fresh media).
//   Cache is count- and age-bounded.
// - TWCC (PT=205, FMT=15): parsed and queued for goog_cc on the pacing
//   worker.
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

// Pacing constants (libwebrtc's pacer and the goog_cc spike, handoff
// section 4).
//
// The pacer may run ahead of its rate by this much: a frame of up to 40 ms
// of pacing budget leaves at once (libwebrtc PacingController
// kDefaultBurstInterval). At the 1080p60 preset (8 Mbps, pacing 20 Mbps)
// that is 100 kB, so an IDR of 170 kB leaves in about 28 ms.
constexpr int64_t kBurstWindowUs = 40'000;
// Without goog_cc (no TWCC) the pacer runs at this factor of the manager
// ceiling, the same factor goog_cc uses over its target.
constexpr uint64_t kPacingFactorNumerator = 5;
constexpr uint64_t kPacingFactorDenominator = 2;
constexpr int64_t kProcessIntervalUs = 25'000;
// Feedback outage: media stops after this long of sending without any
// feedback, and one keepalive goes out every kKeepaliveIntervalUs.
constexpr int64_t kFeedbackTimeoutUs = 1'000'000;
constexpr int64_t kKeepaliveIntervalUs = 500'000;
// A probe cluster that started and cannot finish (the media ran out, no RTX
// padding) is dropped after kProbeClusterTimeoutUs. A cluster that never had
// enough media to start is dropped after kProbeStartTimeoutUs.
constexpr int64_t kProbeClusterTimeoutUs = 1'000'000;
constexpr int64_t kProbeStartTimeoutUs = 3'000'000;
constexpr size_t kMaxQueuedFeedbackReports = 128;
// RTP padding: at most 255 bytes, the last byte holds the count.
constexpr size_t kPaddingBytes = 255;
constexpr size_t kRtpHeaderBytes = 12;

SteadyClock::time_point from_us(int64_t us) noexcept {
    return SteadyClock::time_point(std::chrono::microseconds(us));
}

rtc::message_ptr make_message(std::vector<uint8_t>&& bytes) {
    rtc::binary data;
    data.reserve(bytes.size());
    for (const uint8_t b : bytes) {
        data.push_back(static_cast<std::byte>(b));
    }
    return std::make_shared<rtc::Message>(std::move(data), rtc::Message::Binary);
}

std::vector<uint8_t> message_bytes(const rtc::Message& message) {
    std::vector<uint8_t> bytes;
    bytes.reserve(message.size() + 16);
    for (const std::byte b : message) {
        bytes.push_back(static_cast<uint8_t>(b));
    }
    return bytes;
}

// RFC 4588 section 4: the RTX packet copies the header of the original
// (marker, timestamp, CSRC, extensions) with the RTX SSRC, payload type and
// sequence number, and its payload is the original sequence number then
// the original payload. Padding of the original is not copied.
std::vector<uint8_t> make_rtx_packet(
    const std::vector<uint8_t>& original,
    uint32_t rtx_ssrc,
    uint8_t rtx_payload_type,
    uint16_t rtx_sequence
) {
    if (original.size() < kRtpHeaderBytes || (original[0] >> 6) != 2) {
        return {};
    }
    size_t header = kRtpHeaderBytes + (original[0] & 0x0f) * 4;
    if ((original[0] & 0x10) != 0) {
        if (header + 4 > original.size()) {
            return {};
        }
        header += 4 + static_cast<size_t>(read_u16_be(original.data() + header + 2)) * 4;
    }
    size_t end = original.size();
    if ((original[0] & 0x20) != 0) {
        const size_t padding = original.back();
        if (padding == 0 || padding > end - header) {
            return {};
        }
        end -= padding;
    }
    if (header > end) {
        return {};
    }
    std::vector<uint8_t> rtx;
    rtx.reserve(end + 2);
    rtx.insert(rtx.end(), original.begin(), original.begin() + static_cast<ptrdiff_t>(header));
    rtx[0] &= static_cast<uint8_t>(~0x20);
    rtx[1] = static_cast<uint8_t>((original[1] & 0x80) | (rtx_payload_type & 0x7f));
    rtx[2] = static_cast<uint8_t>(rtx_sequence >> 8);
    rtx[3] = static_cast<uint8_t>(rtx_sequence & 0xff);
    rtx[8] = static_cast<uint8_t>(rtx_ssrc >> 24);
    rtx[9] = static_cast<uint8_t>((rtx_ssrc >> 16) & 0xff);
    rtx[10] = static_cast<uint8_t>((rtx_ssrc >> 8) & 0xff);
    rtx[11] = static_cast<uint8_t>(rtx_ssrc & 0xff);
    rtx.push_back(original[2]);
    rtx.push_back(original[3]);
    rtx.insert(rtx.end(), original.begin() + static_cast<ptrdiff_t>(header),
               original.begin() + static_cast<ptrdiff_t>(end));
    return rtx;
}

// A padding-only packet on the RTX stream (libwebrtc's padding when RTX is
// negotiated). Pion drops it after its interceptors count it for TWCC
// ("BWE probe packet").
std::vector<uint8_t> make_padding_packet(
    uint32_t rtx_ssrc,
    uint8_t rtx_payload_type,
    uint16_t rtx_sequence,
    uint32_t rtp_timestamp
) {
    std::vector<uint8_t> packet(kRtpHeaderBytes + kPaddingBytes, 0);
    packet[0] = 0x80 | 0x20;
    packet[1] = static_cast<uint8_t>(rtx_payload_type & 0x7f);
    packet[2] = static_cast<uint8_t>(rtx_sequence >> 8);
    packet[3] = static_cast<uint8_t>(rtx_sequence & 0xff);
    packet[4] = static_cast<uint8_t>(rtp_timestamp >> 24);
    packet[5] = static_cast<uint8_t>((rtp_timestamp >> 16) & 0xff);
    packet[6] = static_cast<uint8_t>((rtp_timestamp >> 8) & 0xff);
    packet[7] = static_cast<uint8_t>(rtp_timestamp & 0xff);
    packet[8] = static_cast<uint8_t>(rtx_ssrc >> 24);
    packet[9] = static_cast<uint8_t>((rtx_ssrc >> 16) & 0xff);
    packet[10] = static_cast<uint8_t>((rtx_ssrc >> 8) & 0xff);
    packet[11] = static_cast<uint8_t>(rtx_ssrc & 0xff);
    packet.back() = static_cast<uint8_t>(kPaddingBytes);
    return packet;
}

} // namespace

struct RtpVideoSender::State
    : public std::enable_shared_from_this<RtpVideoSender::State> {
    struct AccessUnit {
        std::vector<uint8_t> bytes;
        uint32_t rtp_timestamp = 0;
        bool is_idr = false;
        int64_t enqueued_us = 0;
    };

    struct PacedBatch {
        rtc::message_vector packets;
        rtc::message_callback send;
        SteadyClock::time_point captured_at;
    };

    // One media packet waiting in the pacer.
    struct MediaPacket {
        rtc::message_ptr packet;
        bool idr = false;
        bool first_of_au = false;
        bool last_of_au = false;
        size_t au_bytes = 0;
        int64_t enqueued_us = 0;
    };

    struct ActiveProbe {
        SendProbeCluster cluster;
        int sent_probes = 0;
        uint64_t sent_bytes = 0;
        int64_t created_us = 0;
        int64_t next_send_us = 0;
        bool started = false;
    };

    enum class PacketKind { None, Retransmit, Media, Padding };

    State(
        const RtpVideoSenderConfig& config,
        PliCallback pli_callback,
        RembCallback remb_callback,
        LocalIdrNeededCallback local_idr_needed_callback,
        GccTargetCallback gcc_target_callback
    )
        : pacing_target_bps(config.pacing_target_bps),
          on_pli(std::move(pli_callback)),
          on_remb(std::move(remb_callback)),
          on_local_idr_needed(std::move(local_idr_needed_callback)),
          on_gcc_target(std::move(gcc_target_callback)),
          twcc_enabled(config.twcc_enabled),
          rtx_ssrc(config.rtx_payload_type != 0 ? config.rtx_ssrc : 0),
          rtx_payload_type(config.rtx_ssrc != 0 ? config.rtx_payload_type : 0) {
        const uint64_t ceiling = config.pacing_target_bps;
        controller_config.min_bps = config.min_bps;
        controller_config.max_bps = config.max_bps != 0 ? config.max_bps : ceiling;
        controller_config.start_bps =
            config.start_bps != 0 ? config.start_bps : ceiling * 3 / 5;
        min_limit_bps.store(controller_config.min_bps, std::memory_order_relaxed);
        max_limit_bps.store(controller_config.max_bps, std::memory_order_relaxed);
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
        {
            std::lock_guard<std::mutex> lock(queue_mutex);
            stopping.store(true, std::memory_order_release);
            wake_pending = true;
        }
        queue_cv.notify_all();
        batch_cv.notify_all();

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

    // Any thread: wake the worker for new work (an access unit, a
    // retransmit, feedback, a limit change).
    void wake() noexcept {
        {
            std::lock_guard<std::mutex> lock(queue_mutex);
            wake_pending = true;
        }
        queue_cv.notify_one();
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

    // An IDR request (PLI from the SFU, or the sender's own local IDR need):
    // the retransmits queued for the old picture are useless once the IDR
    // goes out, so they are dropped (streaming-quality stage 4, outage).
    void flush_rtx_queue() noexcept {
        size_t dropped = 0;
        {
            std::lock_guard<std::mutex> lock(queue_mutex);
            dropped = rtx_queue.size();
            rtx_queue.clear();
        }
        rtx_queue_flushes.fetch_add(1, std::memory_order_relaxed);
        rtx_queue_dropped.fetch_add(dropped, std::memory_order_relaxed);
    }

    void record_pli() noexcept {
        pli_requests.fetch_add(1, std::memory_order_relaxed);
        flush_rtx_queue();
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
        flush_rtx_queue();
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

    // RTCP thread: a NACK found this packet in the cache.
    void enqueue_rtx(rtc::message_ptr packet) {
        if (!packet || packet->size() < kRtpHeaderBytes) {
            return;
        }
        const auto* bytes = reinterpret_cast<const uint8_t*>(packet->data());
        const uint16_t sequence = read_u16_be(bytes + 2);
        const int64_t now_us = steady_now_us();
        {
            std::lock_guard<std::mutex> lock(queue_mutex);
            // A packet older than the newest IDR is useless to a receiver
            // that has (or will have) the IDR.
            if (has_repair_floor
                && static_cast<int16_t>(sequence - repair_floor_sequence) < 0) {
                rtx_queue_dropped.fetch_add(1, std::memory_order_relaxed);
                return;
            }
            if (!admit_resend_locked(sequence, packet->size(), now_us)) {
                rtx_suppressed.fetch_add(1, std::memory_order_relaxed);
                return;
            }
            if (rtx_queue.size() >= kMaxRtxQueuePackets) {
                rtx_queue.pop_front();
                rtx_queue_dropped.fetch_add(1, std::memory_order_relaxed);
            }
            rtx_queue.push_back(std::move(packet));
            wake_pending = true;
        }
        queue_cv.notify_one();
    }

    // Under queue_mutex. The two retransmit limits (kMinResendIntervalUs
    // above); records the resend when both allow it.
    bool admit_resend_locked(uint16_t sequence, size_t bytes, int64_t now_us) {
        const int64_t rtt = static_cast<int64_t>(
            feedback_rtt_us.load(std::memory_order_relaxed));
        const int64_t interval = std::max(
            kMinResendIntervalUs, rtt > 0 ? rtt : kDefaultResendIntervalUs);
        const auto last = resent_at_us.find(sequence);
        if (last != resent_at_us.end() && now_us - last->second < interval) {
            return false;
        }
        while (!resend_window.empty()
               && now_us - resend_window.front().first >= kRetransmitRateWindowUs) {
            resend_window_bytes -= resend_window.front().second;
            resend_window.pop_front();
        }
        uint64_t limit_bps = gcc_target_bps.load(std::memory_order_relaxed);
        if (limit_bps == 0) {
            limit_bps = pacing_target_bps.load(std::memory_order_relaxed);
        }
        const uint64_t limit_bytes =
            limit_bps * static_cast<uint64_t>(kRetransmitRateWindowUs) / 8'000'000;
        if (resend_window_bytes + bytes > limit_bytes) {
            return false;
        }
        resend_window.emplace_back(now_us, bytes);
        resend_window_bytes += bytes;
        if (resent_at_us.size() >= kMaxResendRecords) {
            // The NACK cache keeps a packet for 1 s; older records are dead.
            for (auto it = resent_at_us.begin(); it != resent_at_us.end();) {
                it = now_us - it->second >= 1'000'000 ? resent_at_us.erase(it) : std::next(it);
            }
            if (resent_at_us.size() >= kMaxResendRecords) {
                resent_at_us.clear();
            }
        }
        resent_at_us[sequence] = now_us;
        return true;
    }

    void record_rtx_stats(uint64_t requests, uint64_t cache_misses) noexcept {
        rtx_requests.fetch_add(requests, std::memory_order_relaxed);
        rtx_cache_misses.fetch_add(cache_misses, std::memory_order_relaxed);
    }

    // RTCP thread: queue one parsed report for the worker, which owns
    // goog_cc (not thread safe).
    void handle_twcc_feedback(const TwccFeedback& feedback) {
        if (!twcc_enabled) {
            return;
        }
        {
            std::lock_guard<std::mutex> lock(queue_mutex);
            if (feedback_queue.size() >= kMaxQueuedFeedbackReports) {
                feedback_queue.pop_front();
            }
            feedback_queue.push_back({feedback, steady_now_us()});
            wake_pending = true;
        }
        queue_cv.notify_one();
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

    // ---------------------------------------------------------------------
    // Pacing worker. Everything below runs on the worker thread only.
    // ---------------------------------------------------------------------

    uint64_t current_pacing_rate() const noexcept {
        if (controller) {
            return std::max<uint64_t>(controller->pacing_bps(), 50'000);
        }
        const uint64_t ceiling = pacing_target_bps.load(std::memory_order_relaxed);
        return std::max<uint64_t>(
            ceiling * kPacingFactorNumerator / kPacingFactorDenominator, 50'000);
    }

    // The retransmit bytes the worker sent in the last kRetransmitRateWindowUs.
    uint64_t retransmit_rate_bps(int64_t now_us) {
        while (!rtx_sent_window.empty()
               && now_us - rtx_sent_window.front().first >= kRetransmitRateWindowUs) {
            rtx_sent_window_bytes -= rtx_sent_window.front().second;
            rtx_sent_window.pop_front();
        }
        return rtx_sent_window_bytes * 8'000'000 / static_cast<uint64_t>(kRetransmitRateWindowUs);
    }

    // Publishes the goog_cc target (stats, retransmit limit) and the encoder
    // target to Rust: on the first value and on a change over 1/32 from the
    // last value that Rust got.
    //
    // The encoder target is the goog_cc target less the retransmit rate of
    // the last 500 ms (libwebrtc's protection bitrate: the encoder gets
    // what repairs leave), at least a quarter of the target. Retransmits
    // then take the place of media under a cap instead of adding to it.
    void publish_target(int64_t now_us) noexcept {
        // Before the first feedback the target is only goog_cc's start
        // value: no information about the link, so the encoder keeps the
        // stream manager's start value.
        if (!controller || twcc_reports.load(std::memory_order_relaxed) == 0) {
            return;
        }
        const uint64_t target = controller->encoder_target_bps();
        pacing_rate_bps.store(current_pacing_rate(), std::memory_order_relaxed);
        gcc_target_bps.store(target, std::memory_order_relaxed);
        const uint64_t repairs = retransmit_rate_bps(now_us);
        const uint64_t encoder = std::max(target / 4, target > repairs ? target - repairs : 0);
        encoder_target_bps.store(encoder, std::memory_order_relaxed);
        const uint64_t previous = published_encoder_bps;
        if (previous != 0
            && encoder <= previous + previous / 32
            && encoder + previous / 32 >= previous) {
            return;
        }
        published_encoder_bps = encoder;
        try {
            std::lock_guard<std::mutex> lock(callback_mutex);
            if (on_gcc_target) {
                on_gcc_target(static_cast<uint32_t>(std::min<uint64_t>(encoder, UINT32_MAX)));
            }
        } catch (...) {
        }
    }

    void take_probes(int64_t now_us) {
        if (!controller) {
            return;
        }
        for (const auto& cluster : controller->take_probe_clusters()) {
            ActiveProbe probe;
            probe.cluster = cluster;
            probe.created_us = now_us;
            probe.next_send_us = now_us;
            probes.push_back(probe);
            probe_clusters_started.fetch_add(1, std::memory_order_relaxed);
        }
    }

    void apply_limit_changes(int64_t now_us) {
        if (!controller) {
            return;
        }
        if (limits_changed.exchange(false, std::memory_order_acq_rel)) {
            const uint64_t min_bps = min_limit_bps.load(std::memory_order_relaxed);
            const uint64_t max_bps = max_limit_bps.load(std::memory_order_relaxed);
            const uint64_t start_bps = start_limit_bps.load(std::memory_order_relaxed);
            if (start_bps != 0 && twcc_reports.load(std::memory_order_relaxed) == 0) {
                // No feedback yet: start goog_cc again at the stream's
                // start value, so its start probes are relative to it.
                controller_config.min_bps = min_bps;
                controller_config.max_bps = max_bps;
                controller_config.start_bps = start_bps;
                controller = std::make_unique<GoogCcController>(controller_config, now_us);
                probes.clear();
                take_probes(now_us);
                last_allocated_bps = max_bps;
            } else {
                controller->set_bitrate_limits(now_us, min_bps, max_bps);
            }
        }
        if (allocation_changed.exchange(false, std::memory_order_acq_rel)) {
            const uint64_t allocation = max_allocated_bps.load(std::memory_order_relaxed);
            controller->set_max_total_allocated(now_us, allocation);
            // A raise is a rung step up. goog_cc probes on a new allocation
            // only in ALR, and a sender on a rung sends about its target, so
            // the raise alone sends no probe and the target climbs back in
            // 5 to 13 s. The route change restarts the estimators at the
            // current target and runs the initial probes on the IDR of the
            // new rung: back in 0.1 s in SendController
            // .ARungStepUpWithAReprobeFindsTheReleasedLink, and at the cap on
            // a link that is still capped.
            if (allocation > last_allocated_bps) {
                controller->on_route_change(now_us);
                reprobes.fetch_add(1, std::memory_order_relaxed);
            }
            last_allocated_bps = allocation;
            take_probes(now_us);
            publish_target(now_us);
        }
    }

    void drain_feedback(int64_t now_us) {
        std::deque<QueuedFeedback> reports;
        {
            std::lock_guard<std::mutex> lock(queue_mutex);
            reports.swap(feedback_queue);
        }
        for (const auto& report : reports) {
            twcc_reports.fetch_add(1, std::memory_order_relaxed);
            first_unfed_send_us = 0;
            if (!controller) {
                continue;
            }
            if (paused) {
                resume_after_outage(now_us);
                // The first report after the outage holds the packets lost
                // in it. The route change replaces it (handoff section 4).
                continue;
            }
            controller->on_feedback(report.feedback, report.arrived_us);
        }
        if (controller) {
            feedback_rtt_us.store(
                static_cast<uint64_t>(std::max<int64_t>(controller->rtt_us(), 0)),
                std::memory_order_relaxed);
        }
        if (!reports.empty()) {
            take_probes(now_us);
            publish_target(now_us);
        }
    }

    // Feedback timeout: after 1 s of sending without feedback, media stops
    // and a keepalive goes out every 500 ms until feedback returns. A link
    // in an outage gets no stale bytes and no stale repairs.
    void check_feedback_timeout(int64_t now_us) {
        if (!controller || paused || first_unfed_send_us == 0
            || now_us - first_unfed_send_us <= kFeedbackTimeoutUs) {
            return;
        }
        paused = true;
        send_paused.store(true, std::memory_order_relaxed);
        send_pauses.fetch_add(1, std::memory_order_relaxed);
        drop_media_packets();
        probes.clear();
        {
            std::lock_guard<std::mutex> lock(queue_mutex);
            rtx_queue.clear();
            paused_for_producer = true;
            // Let deltas in: the keepalive without RTX is the newest access
            // unit. The resume asks for an IDR.
            awaiting_idr = false;
        }
        next_keepalive_us = now_us;
    }

    void resume_after_outage(int64_t now_us) {
        paused = false;
        controller->on_route_change(now_us);
        take_probes(now_us);
        drop_media_packets();
        bool request_idr = false;
        {
            std::lock_guard<std::mutex> lock(queue_mutex);
            paused_for_producer = false;
            // The viewers lost the reference pictures in the outage: hold
            // deltas until the IDR that the local request below produces.
            const uint64_t dropped = drop_queued_access_units_locked(false);
            access_units_dropped.fetch_add(dropped, std::memory_order_relaxed);
            if (!awaiting_idr) {
                awaiting_idr = true;
                request_idr = true;
            }
        }
        if (request_idr) {
            record_local_idr_needed();
        }
        budget_debt_bytes = 0.0;
        budget_updated_us = now_us;
        send_paused.store(false, std::memory_order_relaxed);
    }

    // Under queue_mutex. Returns the number dropped.
    uint64_t drop_queued_access_units_locked(bool keep_newest) {
        uint64_t dropped = 0;
        // A dropped IDR does not drain (kMaxQueuedAccessUnits).
        idr_in_sender.store(
            keep_newest && !access_unit_queue.empty() && access_unit_queue.back().is_idr,
            std::memory_order_relaxed);
        while (access_unit_queue.size() > (keep_newest ? 1u : 0u)) {
            queued_bytes_value -= access_unit_queue.front().bytes.size();
            access_unit_queue.pop_front();
            ++dropped;
        }
        queued_access_units.store(access_unit_queue.size(), std::memory_order_relaxed);
        queued_bytes.store(queued_bytes_value, std::memory_order_relaxed);
        return dropped;
    }

    void drop_media_packets() {
        if (!media_packets.empty() && media_packets.front().idr) {
            idr_in_sender.store(false, std::memory_order_relaxed);
        }
        if (!media_packets.empty()) {
            access_units_dropped.fetch_add(1, std::memory_order_relaxed);
        }
        media_packets.clear();
        media_queue_bytes = 0;
    }

    // Packetizes the next access unit into media_packets. Returns false when
    // the queue had nothing.
    bool packetize_next(int64_t now_us) {
        AccessUnit access_unit;
        {
            std::lock_guard<std::mutex> lock(queue_mutex);
            if (access_unit_queue.empty()) {
                return false;
            }
            access_unit = std::move(access_unit_queue.front());
            queued_bytes_value -= access_unit.bytes.size();
            access_unit_queue.pop_front();
            queued_access_units.store(access_unit_queue.size(), std::memory_order_relaxed);
            queued_bytes.store(queued_bytes_value, std::memory_order_relaxed);
        }
        if (!track_is_open()) {
            send_failures.fetch_add(1, std::memory_order_relaxed);
            if (access_unit.is_idr) {
                idr_in_sender.store(false, std::memory_order_relaxed);
            }
            return true;
        }
        try {
            track->sendFrame(
                reinterpret_cast<const std::byte*>(access_unit.bytes.data()),
                access_unit.bytes.size(),
                rtc::FrameInfo(access_unit.rtp_timestamp));
        } catch (...) {
            discard_batch();
            send_failures.fetch_add(1, std::memory_order_relaxed);
            if (access_unit.is_idr) {
                idr_in_sender.store(false, std::memory_order_relaxed);
            }
            return true;
        }
        PacedBatch batch;
        if (!take_batch(batch)) {
            if (!stopping.load(std::memory_order_acquire)) {
                send_failures.fetch_add(1, std::memory_order_relaxed);
            }
            if (access_unit.is_idr) {
                idr_in_sender.store(false, std::memory_order_relaxed);
            }
            return true;
        }
        last_rtp_timestamp = access_unit.rtp_timestamp;
        size_t index = 0;
        const size_t count = batch.packets.size();
        for (auto& packet : batch.packets) {
            if (!packet) {
                ++index;
                continue;
            }
            MediaPacket media;
            media.packet = std::move(packet);
            media.idr = access_unit.is_idr;
            media.first_of_au = index == 0;
            media.last_of_au = index + 1 == count;
            media.au_bytes = access_unit.bytes.size();
            media.enqueued_us = access_unit.enqueued_us;
            media_queue_bytes += media.packet->size();
            if (media.idr && media.first_of_au && media.packet->size() >= 4) {
                const auto* bytes = reinterpret_cast<const uint8_t*>(media.packet->data());
                std::lock_guard<std::mutex> lock(queue_mutex);
                repair_floor_sequence = read_u16_be(bytes + 2);
                has_repair_floor = true;
                // Repairs of the pictures before this IDR are useless now.
                rtx_queue_dropped.fetch_add(rtx_queue.size(), std::memory_order_relaxed);
                rtx_queue.clear();
            }
            media_packets.push_back(std::move(media));
            ++index;
        }
        (void)now_us;
        return true;
    }

    rtc::message_ptr pop_rtx() {
        std::lock_guard<std::mutex> lock(queue_mutex);
        if (rtx_queue.empty()) {
            return {};
        }
        auto packet = std::move(rtx_queue.front());
        rtx_queue.pop_front();
        return packet;
    }

    bool has_rtx() {
        std::lock_guard<std::mutex> lock(queue_mutex);
        return !rtx_queue.empty();
    }

    rtc::message_callback current_send() {
        std::lock_guard<std::mutex> lock(batch_mutex);
        return send_callback;
    }

    // Stamps (TWCC), sends and accounts for one packet. Returns the bytes on
    // the wire, or 0 on failure.
    size_t transmit(
        const rtc::message_ptr& packet,
        const ActiveProbe* probe,
        int64_t now_us
    ) {
        const rtc::message_callback send = current_send();
        if (!packet || !send || !track_is_open()) {
            send_failures.fetch_add(1, std::memory_order_relaxed);
            return 0;
        }
        rtc::message_ptr wire = packet;
        uint16_t transport_sequence = 0;
        bool stamped = false;
        if (twcc_enabled) {
            std::vector<uint8_t> bytes = message_bytes(*packet);
            if (stamper.stamp(bytes, transport_sequence)) {
                wire = make_message(std::move(bytes));
                stamped = true;
            }
        }
        try {
            send(wire);
        } catch (...) {
            send_failures.fetch_add(1, std::memory_order_relaxed);
            return 0;
        }
        const size_t size = wire->size();
        rtp_packets_sent.fetch_add(1, std::memory_order_relaxed);
        rtp_wire_bytes_sent.fetch_add(size, std::memory_order_relaxed);
        if (first_unfed_send_us == 0) {
            first_unfed_send_us = now_us;
        }
        if (controller && stamped) {
            SentPacketInfo info;
            info.transport_sequence = transport_sequence;
            info.send_time_us = now_us;
            info.rtp_bytes = size;
            if (probe != nullptr) {
                info.probe_cluster_id = probe->cluster.id;
                info.probe_min_probes = probe->cluster.min_probes;
                info.probe_min_bytes = static_cast<int>(
                    std::min<uint64_t>(probe->cluster.min_bytes, INT32_MAX));
            }
            controller->on_packet_sent(info);
        }
        return size;
    }

    // The retransmit as it goes on the wire: an RFC 4588 packet when RTX is
    // negotiated, else the original bytes.
    rtc::message_ptr retransmit_packet(const rtc::message_ptr& original) {
        if (rtx_ssrc == 0) {
            return original;
        }
        auto rtx = make_rtx_packet(
            message_bytes(*original), rtx_ssrc, rtx_payload_type, rtx_sequence);
        if (rtx.empty()) {
            return {};
        }
        ++rtx_sequence;
        return make_message(std::move(rtx));
    }

    rtc::message_ptr padding_packet() {
        if (rtx_ssrc == 0) {
            return {};
        }
        auto padding = make_padding_packet(
            rtx_ssrc, rtx_payload_type, rtx_sequence, last_rtp_timestamp);
        ++rtx_sequence;
        return make_message(std::move(padding));
    }

    void account_media_sent(const MediaPacket& media, int64_t now_us) {
        if (!media.last_of_au) {
            return;
        }
        access_units_sent.fetch_add(1, std::memory_order_relaxed);
        bytes_sent.fetch_add(media.au_bytes, std::memory_order_relaxed);
        const uint64_t delay = now_us > media.enqueued_us
            ? static_cast<uint64_t>(now_us - media.enqueued_us)
            : 0;
        current_pacing_delay_us.store(delay, std::memory_order_relaxed);
        update_max(max_pacing_delay_us, delay);
        if (media.idr) {
            idr_in_sender.store(false, std::memory_order_relaxed);
            idr_access_units_sent.fetch_add(1, std::memory_order_relaxed);
            last_idr_send_us.store(delay, std::memory_order_relaxed);
            update_max(max_idr_send_us, delay);
            if (delay > 50'000) {
                idr_over_50ms.fetch_add(1, std::memory_order_relaxed);
            }
        }
    }

    // The retransmit rate: the goog_cc target (without goog_cc the manager
    // ceiling). Retransmits leave at most at this rate, with the same 40 ms
    // burst as the pacer; the pacer rate (2.5 x the target) is for media
    // bursts. A cap change loses a burst of packets, and their repairs at
    // 2.5 x the target went into the link that had just dropped them: 5 to
    // 7 Mbps of repairs into 4.8 Mbps (stage 4 gate, hw congested).
    uint64_t retransmit_pacing_bps() const noexcept {
        uint64_t rate = gcc_target_bps.load(std::memory_order_relaxed);
        if (rate == 0) {
            rate = pacing_target_bps.load(std::memory_order_relaxed);
        }
        return std::max<uint64_t>(rate, 50'000);
    }

    // When the retransmit budget allows the next repair.
    int64_t rtx_ready_us(int64_t now_us) {
        const uint64_t rate = retransmit_pacing_bps();
        if (now_us > rtx_budget_updated_us) {
            rtx_debt_bytes = std::max(
                0.0,
                rtx_debt_bytes
                    - static_cast<double>(now_us - rtx_budget_updated_us)
                        * static_cast<double>(rate) / 8e6);
        }
        rtx_budget_updated_us = now_us;
        const double burst =
            static_cast<double>(rate) * static_cast<double>(kBurstWindowUs) / 8e6;
        if (rtx_debt_bytes <= burst) {
            return now_us;
        }
        return now_us + static_cast<int64_t>(
            (rtx_debt_bytes - burst) * 8e6 / static_cast<double>(rate)) + 1;
    }

    // Sends one packet of the next kind in priority order: retransmits
    // (inside their own budget, retransmit_pacing_bps), then media, then
    // padding (probes only). Returns false when there was nothing to send.
    bool send_one(int64_t now_us, ActiveProbe* probe) {
        rtc::message_ptr original;
        if (probe != nullptr || rtx_ready_us(now_us) <= now_us) {
            original = pop_rtx();
        }
        if (original) {
            rtc::message_ptr packet = retransmit_packet(original);
            const size_t size = packet ? transmit(packet, probe, now_us) : 0;
            if (size > 0) {
                rtx_sent.fetch_add(1, std::memory_order_relaxed);
                rtx_bytes_sent.fetch_add(size, std::memory_order_relaxed);
                rtx_sent_window.emplace_back(now_us, size);
                rtx_sent_window_bytes += size;
                (void)retransmit_rate_bps(now_us);
                rtx_debt_bytes += static_cast<double>(size);
            }
            charge(size, probe, now_us);
            return true;
        }
        if (media_packets.empty()) {
            packetize_next(now_us);
        }
        if (!media_packets.empty()) {
            MediaPacket media = std::move(media_packets.front());
            media_packets.pop_front();
            media_queue_bytes -= std::min(media_queue_bytes, media.packet->size());
            const size_t size = transmit(media.packet, probe, now_us);
            if (size > 0) {
                account_media_sent(media, now_us);
            }
            charge(size, probe, now_us);
            return true;
        }
        if (probe != nullptr) {
            if (rtc::message_ptr padding = padding_packet()) {
                const size_t size = transmit(padding, probe, now_us);
                if (size > 0) {
                    padding_packets_sent.fetch_add(1, std::memory_order_relaxed);
                }
                charge(size, probe, now_us);
                return true;
            }
        }
        return false;
    }

    void drain_budget(int64_t now_us) {
        const uint64_t rate = current_pacing_rate();
        if (now_us > budget_updated_us) {
            budget_debt_bytes = std::max(
                0.0,
                budget_debt_bytes
                    - static_cast<double>(now_us - budget_updated_us)
                        * static_cast<double>(rate) / 8e6);
        }
        budget_updated_us = now_us;
    }

    void charge(size_t bytes, ActiveProbe* probe, int64_t now_us) {
        drain_budget(now_us);
        budget_debt_bytes += static_cast<double>(bytes);
        if (probe != nullptr) {
            probe->sent_probes += 1;
            probe->sent_bytes += bytes;
            const uint64_t rate = std::max<uint64_t>(probe->cluster.rate_bps, 50'000);
            probe->next_send_us = std::max(probe->next_send_us, now_us - 2'000)
                + static_cast<int64_t>(bytes * 8'000'000 / rate);
        }
    }

    // When the normal budget allows the next packet.
    int64_t budget_ready_us(int64_t now_us) {
        drain_budget(now_us);
        const uint64_t rate = current_pacing_rate();
        const double burst =
            static_cast<double>(rate) * static_cast<double>(kBurstWindowUs) / 8e6;
        if (budget_debt_bytes <= burst) {
            return now_us;
        }
        return now_us + static_cast<int64_t>(
            (budget_debt_bytes - burst) * 8e6 / static_cast<double>(rate)) + 1;
    }

    bool has_media_work() {
        return has_fresh_media() || has_rtx();
    }

    bool has_fresh_media() {
        if (!media_packets.empty()) {
            return true;
        }
        std::lock_guard<std::mutex> lock(queue_mutex);
        return !access_unit_queue.empty();
    }

    // While paused: keep only the newest access unit and send one keepalive
    // every 500 ms. With RTX the keepalive is one padding packet; without,
    // it is the newest access unit (the viewers lost the pictures before it
    // anyway, and the resume asks for an IDR).
    void keepalive(int64_t now_us) {
        {
            std::lock_guard<std::mutex> lock(queue_mutex);
            access_units_dropped.fetch_add(
                drop_queued_access_units_locked(true), std::memory_order_relaxed);
        }
        if (now_us < next_keepalive_us) {
            return;
        }
        next_keepalive_us = now_us + kKeepaliveIntervalUs;
        if (rtc::message_ptr padding = padding_packet()) {
            if (transmit(padding, nullptr, now_us) > 0) {
                padding_packets_sent.fetch_add(1, std::memory_order_relaxed);
            }
            return;
        }
        drop_media_packets();
        if (!packetize_next(now_us)) {
            return;
        }
        while (!media_packets.empty()) {
            MediaPacket media = std::move(media_packets.front());
            media_packets.pop_front();
            if (transmit(media.packet, nullptr, now_us) > 0) {
                account_media_sent(media, now_us);
            }
        }
        media_queue_bytes = 0;
    }

    void wait_until_us(int64_t deadline_us) {
        std::unique_lock<std::mutex> lock(queue_mutex);
        queue_cv.wait_until(lock, from_us(deadline_us), [this]() {
            return wake_pending || stopping.load(std::memory_order_acquire);
        });
        wake_pending = false;
    }

    uint64_t pacer_queue_bytes() {
        std::lock_guard<std::mutex> lock(queue_mutex);
        return media_queue_bytes + queued_bytes_value;
    }

    void worker_main() noexcept {
        try {
            run_worker();
        } catch (...) {
            send_failures.fetch_add(1, std::memory_order_relaxed);
        }
    }

    void run_worker() {
        int64_t now_us = steady_now_us();
        if (twcc_enabled && GoogCcController::available()) {
            controller = std::make_unique<GoogCcController>(controller_config, now_us);
            last_allocated_bps = controller_config.max_bps;
            take_probes(now_us);
            publish_target(now_us);
        }
        pacing_rate_bps.store(current_pacing_rate(), std::memory_order_relaxed);
        budget_updated_us = now_us;
        int64_t next_process_us = now_us + kProcessIntervalUs;

        while (!stopping.load(std::memory_order_acquire)) {
            now_us = steady_now_us();
            drain_feedback(now_us);
            apply_limit_changes(now_us);
            if (controller && now_us >= next_process_us) {
                controller->on_process_interval(now_us, pacer_queue_bytes());
                take_probes(now_us);
                publish_target(now_us);
                next_process_us = now_us + kProcessIntervalUs;
            }
            pacing_rate_bps.store(current_pacing_rate(), std::memory_order_relaxed);
            check_feedback_timeout(now_us);
            const int64_t process_deadline =
                controller ? next_process_us : now_us + 250'000;

            if (paused) {
                keepalive(now_us);
                wait_until_us(std::min(next_keepalive_us, process_deadline));
                continue;
            }

            // Probe clusters first, at their own rate. Without RTX padding a
            // probe can only use media, and a probe spread over two frames
            // measures the encoder, not the link. So a cluster starts only
            // when the pacer holds at least its bytes (the first IDR, the
            // IDR after an outage, a large frame); until then media goes at
            // the normal rate. With RTX, padding fills it at once.
            while (!probes.empty()) {
                ActiveProbe& front = probes.front();
                if (front.started) {
                    if (now_us - front.created_us <= kProbeClusterTimeoutUs) {
                        break;
                    }
                } else if (rtx_ssrc != 0
                           || pacer_queue_bytes() >= front.cluster.min_bytes) {
                    front.started = true;
                    front.created_us = now_us;
                    front.next_send_us = now_us;
                    break;
                } else if (now_us - front.created_us <= kProbeStartTimeoutUs) {
                    break;
                }
                probes.pop_front();
            }
            if (!probes.empty() && probes.front().started) {
                ActiveProbe& probe = probes.front();
                if (now_us < probe.next_send_us) {
                    wait_until_us(std::min(probe.next_send_us, process_deadline));
                    continue;
                }
                if (send_one(now_us, &probe)) {
                    if (probe.sent_probes >= probe.cluster.min_probes
                        && probe.sent_bytes >= probe.cluster.min_bytes) {
                        probe_clusters_completed.fetch_add(1, std::memory_order_relaxed);
                        probes.pop_front();
                    }
                    continue;
                }
                // The media ran out inside the cluster: wait for the next
                // access unit, or the cluster times out.
                wait_until_us(process_deadline);
                continue;
            }

            if (!has_media_work()) {
                wait_until_us(process_deadline);
                continue;
            }
            int64_t ready_us = budget_ready_us(now_us);
            if (!has_fresh_media()) {
                // Only repairs wait: they also need their own budget.
                ready_us = std::max(ready_us, rtx_ready_us(now_us));
            }
            if (ready_us > now_us) {
                wait_until_us(std::min(ready_us, process_deadline));
                continue;
            }
            send_one(now_us, nullptr);
        }
    }

    struct QueuedFeedback {
        TwccFeedback feedback;
        int64_t arrived_us = 0;
    };

    std::shared_ptr<rtc::Track> track;
    std::shared_ptr<rtc::MediaHandler> root_handler;
    std::atomic<bool> attached{false};
    std::atomic<bool> stopping{false};
    uint32_t rtp_start_timestamp = 0;

    // queue_mutex guards the access unit queue, the retransmit queue, the
    // feedback queue, the repair floor and the wake flag.
    std::mutex queue_mutex;
    std::condition_variable queue_cv;
    bool wake_pending = false;
    std::deque<AccessUnit> access_unit_queue;
    size_t queued_bytes_value = 0;
    size_t media_queue_bytes = 0;
    bool timestamp_initialized = false;
    uint64_t first_capture_timestamp_us = 0;
    uint64_t last_capture_timestamp_us = 0;
    bool awaiting_idr = false;
    // Media is stopped for a feedback outage: the producer drops instead of
    // asking for local IDRs.
    bool paused_for_producer = false;
    std::deque<rtc::message_ptr> rtx_queue;
    // The retransmit limits (admit_resend_locked), under queue_mutex.
    std::unordered_map<uint16_t, int64_t> resent_at_us;
    std::deque<std::pair<int64_t, size_t>> resend_window;
    uint64_t resend_window_bytes = 0;
    std::deque<QueuedFeedback> feedback_queue;
    uint16_t repair_floor_sequence = 0;
    bool has_repair_floor = false;

    std::mutex batch_mutex;
    std::condition_variable batch_cv;
    PacedBatch paced_batch;
    bool batch_ready = false;
    // Transport send path captured from the packetizer chain; the pacing
    // worker sends every packet through it.
    rtc::message_callback send_callback;

    std::atomic<uint64_t> pacing_target_bps{0};

    std::mutex shutdown_mutex;
    std::thread worker;

    std::mutex callback_mutex;
    PliCallback on_pli;
    RembCallback on_remb;
    LocalIdrNeededCallback on_local_idr_needed;
    GccTargetCallback on_gcc_target;

    const bool twcc_enabled = false;
    const uint32_t rtx_ssrc = 0;
    const uint8_t rtx_payload_type = 0;
    GoogCcController::Config controller_config;
    std::atomic<uint64_t> min_limit_bps{0};
    std::atomic<uint64_t> max_limit_bps{0};
    std::atomic<uint64_t> start_limit_bps{0};
    std::atomic<bool> limits_changed{false};
    std::atomic<uint64_t> max_allocated_bps{0};
    std::atomic<bool> allocation_changed{false};

    // Worker thread only.
    TwccSendStamper stamper;
    std::unique_ptr<GoogCcController> controller;
    std::deque<MediaPacket> media_packets;
    std::deque<ActiveProbe> probes;
    double budget_debt_bytes = 0.0;
    int64_t budget_updated_us = 0;
    // The retransmit budget (rtx_ready_us).
    double rtx_debt_bytes = 0.0;
    int64_t rtx_budget_updated_us = 0;
    int64_t first_unfed_send_us = 0;
    bool paused = false;
    int64_t next_keepalive_us = 0;
    uint16_t rtx_sequence = 0;
    uint32_t last_rtp_timestamp = 0;
    uint64_t last_allocated_bps = 0;

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
    std::atomic<uint64_t> rtx_requests{0};
    std::atomic<uint64_t> rtx_sent{0};
    std::atomic<uint64_t> rtx_bytes_sent{0};
    std::atomic<uint64_t> rtx_cache_misses{0};
    std::atomic<uint64_t> rtx_queue_dropped{0};
    std::atomic<uint64_t> rtx_queue_flushes{0};
    std::atomic<uint64_t> rtx_suppressed{0};
    // An IDR is queued or partly on the wire (kMaxQueuedAccessUnits).
    std::atomic<bool> idr_in_sender{false};
    // The encoder target that Rust gets (publish_target); worker only,
    // except the stats copy.
    std::atomic<uint64_t> encoder_target_bps{0};
    uint64_t published_encoder_bps = 0;
    std::deque<std::pair<int64_t, size_t>> rtx_sent_window;
    uint64_t rtx_sent_window_bytes = 0;
    std::atomic<uint64_t> feedback_rtt_us{0};
    std::atomic<uint64_t> pli_requests{0};
    std::atomic<uint64_t> remb_reports{0};
    std::atomic<uint32_t> latest_remb_bitrate_bps{0};
    std::atomic<uint64_t> twcc_reports{0};
    std::atomic<uint64_t> gcc_target_bps{0};
    std::atomic<uint64_t> pacing_rate_bps{0};
    std::atomic<uint64_t> probe_clusters_started{0};
    std::atomic<uint64_t> probe_clusters_completed{0};
    std::atomic<uint64_t> padding_packets_sent{0};
    std::atomic<uint64_t> send_pauses{0};
    std::atomic<bool> send_paused{false};
    std::atomic<uint64_t> idr_access_units_sent{0};
    std::atomic<uint64_t> last_idr_send_us{0};
    std::atomic<uint64_t> max_idr_send_us{0};
    std::atomic<uint64_t> reprobes{0};
    std::atomic<uint64_t> idr_over_50ms{0};
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
            config,
            std::move(on_pli),
            std::move(on_remb),
            std::move(on_local_idr_needed),
            std::move(on_gcc_target)
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

std::optional<uint32_t> RtpVideoSender::rtp_timestamp_for_capture_us(
    uint64_t capture_us
) const noexcept {
    const auto state = state_;
    if (!state) {
        return std::nullopt;
    }
    std::lock_guard<std::mutex> lock(state->queue_mutex);
    if (!state->timestamp_initialized) {
        return std::nullopt;
    }
    // The rule of send_access_unit, also for a time before the first frame.
    const uint64_t first = state->first_capture_timestamp_us;
    if (capture_us >= first) {
        return static_cast<uint32_t>(
            state->rtp_start_timestamp + capture_elapsed_to_rtp_ticks(capture_us - first));
    }
    return static_cast<uint32_t>(
        state->rtp_start_timestamp - capture_elapsed_to_rtp_ticks(first - capture_us));
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
    access_unit.enqueued_us = steady_now_us();
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

    // During a feedback outage the worker drops media anyway; the IDR comes
    // from the resume, so no local IDR request here.
    const auto enter_idr_gate = [&state, &notify_local_idr_needed]() noexcept {
        if (!state->awaiting_idr) {
            state->awaiting_idr = true;
            notify_local_idr_needed = !state->paused_for_producer;
        }
    };

    if (!access_unit_info.is_idr && state->awaiting_idr) {
        lock.unlock();
        return backpressure();
    }

    if (!access_unit_info.is_idr) {
        // While an IDR drains, only the byte bound applies (kMaxQueuedAccessUnits).
        const bool idr_draining = state->idr_in_sender.load(std::memory_order_relaxed);
        if ((!idr_draining && state->access_unit_queue.size() >= kMaxQueuedAccessUnits)
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
            // An IDR goes behind the queued access units, as libwebrtc's
            // pacer sends every encoded frame. It replaced the queue before
            // stage 4: on hardware clean each periodic IDR, then the IDR of
            // the viewer's PLI right after it, dropped 3 to 6 queued access
            // units (18 in 20 s). Only when the byte bound leaves no room for
            // the IDR are the queued units dropped.
            if (size > kMaxQueuedBytes - state->queued_bytes_value) {
                const uint64_t dropped = static_cast<uint64_t>(
                    state->access_unit_queue.size()
                );
                state->access_unit_queue.clear();
                state->queued_bytes_value = 0;
                state->access_units_dropped.fetch_add(
                    dropped,
                    std::memory_order_relaxed
                );
            }
            state->access_unit_queue.push_back(std::move(access_unit));
            state->queued_bytes_value += size;
            state->awaiting_idr = false;
            state->idr_in_sender.store(true, std::memory_order_relaxed);
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
    state->wake_pending = true;
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
    state->wake();
    return true;
}

bool RtpVideoSender::set_bitrate_limits(
    uint64_t min_bps,
    uint64_t start_bps,
    uint64_t max_bps
) noexcept {
    const auto state = state_;
    if (!state || max_bps == 0 || min_bps > max_bps
        || state->stopping.load(std::memory_order_acquire)) {
        return false;
    }
    state->min_limit_bps.store(min_bps, std::memory_order_relaxed);
    state->max_limit_bps.store(max_bps, std::memory_order_relaxed);
    state->start_limit_bps.store(start_bps, std::memory_order_relaxed);
    state->limits_changed.store(true, std::memory_order_release);
    state->wake();
    return true;
}

bool RtpVideoSender::set_max_allocated_bps(uint64_t bitrate_bps) noexcept {
    const auto state = state_;
    if (!state || bitrate_bps == 0
        || state->stopping.load(std::memory_order_acquire)) {
        return false;
    }
    state->max_allocated_bps.store(bitrate_bps, std::memory_order_relaxed);
    state->allocation_changed.store(true, std::memory_order_release);
    state->wake();
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
    const auto load = [](const std::atomic<uint64_t>& value) {
        return value.load(std::memory_order_relaxed);
    };

    result.access_units_enqueued = load(state->access_units_enqueued);
    result.access_units_sent = load(state->access_units_sent);
    result.access_units_dropped = load(state->access_units_dropped);
    result.access_units_rejected = load(state->access_units_rejected);
    result.bytes_sent = load(state->bytes_sent);
    result.send_failures = load(state->send_failures);
    result.rtp_packets_sent = load(state->rtp_packets_sent);
    result.rtp_wire_bytes_sent = load(state->rtp_wire_bytes_sent);
    result.queued_access_units = load(state->queued_access_units);
    result.peak_queued_access_units = load(state->peak_queued_access_units);
    result.queued_bytes = load(state->queued_bytes);
    result.peak_queued_bytes = load(state->peak_queued_bytes);
    result.pacing_target_bps = load(state->pacing_target_bps);
    result.current_pacing_delay_us = load(state->current_pacing_delay_us);
    result.max_pacing_delay_us = load(state->max_pacing_delay_us);
    result.local_idr_requests = load(state->local_idr_requests);
    result.pli_requests = load(state->pli_requests);
    result.remb_reports = load(state->remb_reports);
    result.latest_remb_bitrate_bps =
        state->latest_remb_bitrate_bps.load(std::memory_order_relaxed);
    result.rtx_requests = load(state->rtx_requests);
    result.rtx_sent = load(state->rtx_sent);
    result.rtx_cache_misses = load(state->rtx_cache_misses);
    result.rtx_queue_dropped = load(state->rtx_queue_dropped);
    result.rtx_bytes_sent = load(state->rtx_bytes_sent);
    result.twcc_reports = load(state->twcc_reports);
    result.gcc_target_bps = load(state->gcc_target_bps);
    result.pacing_rate_bps = load(state->pacing_rate_bps);
    result.probe_clusters_started = load(state->probe_clusters_started);
    result.probe_clusters_completed = load(state->probe_clusters_completed);
    result.padding_packets_sent = load(state->padding_packets_sent);
    result.send_pauses = load(state->send_pauses);
    result.send_paused = state->send_paused.load(std::memory_order_relaxed);
    result.idr_access_units_sent = load(state->idr_access_units_sent);
    result.last_idr_send_us = load(state->last_idr_send_us);
    result.max_idr_send_us = load(state->max_idr_send_us);
    result.rtx_queue_flushes = load(state->rtx_queue_flushes);
    result.reprobes = load(state->reprobes);
    result.idr_access_units_over_50ms = load(state->idr_over_50ms);
    result.rtx_suppressed = load(state->rtx_suppressed);
    result.encoder_target_bps = load(state->encoder_target_bps);
    result.feedback_rtt_us = load(state->feedback_rtt_us);
    return result;
}

} // namespace mello::transport
