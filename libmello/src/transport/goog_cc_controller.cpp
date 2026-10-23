// GoogCcController (twcc.hpp): WebRTC goog_cc behind the stream sender.
//
// With MELLO_HAS_GOOG_CC this file compiles in the C++20 object library
// mello_goog_cc (cmake/webrtc-goog-cc.cmake), with the same NDEBUG state as
// the webrtc_goog_cc target. It is the only libmello file that includes a
// goog_cc header. Without MELLO_HAS_GOOG_CC it compiles in libmello as stubs:
// available() is false and the sender never makes a controller.

#include "twcc.hpp"

#if defined(MELLO_HAS_GOOG_CC)

#include <algorithm>
#include <deque>
#include <map>
#include <optional>
#include <string>

#include "absl/strings/string_view.h"
#include "api/environment/environment.h"
#include "api/environment/environment_factory.h"
#include "api/field_trials_view.h"
#include "api/transport/goog_cc_factory.h"
#include "api/transport/network_control.h"
#include "api/transport/network_types.h"
#include "api/units/data_rate.h"
#include "api/units/data_size.h"
#include "api/units/time_delta.h"
#include "api/units/timestamp.h"

namespace mello::transport {
namespace {

using webrtc::DataRate;
using webrtc::DataSize;
using webrtc::Timestamp;

// IPv4 (20) + UDP (8) + the SRTP authentication tag (10). goog_cc wants the
// packet size up to the IP layer.
constexpr size_t kPacketOverheadBytes = 38;
// Send history: 60 s, or half the 16-bit sequence space, so that unwrapping
// a feedback sequence against the newest record is never ambiguous.
constexpr int64_t kHistoryUs = 60'000'000;
constexpr size_t kMaxHistoryPackets = 32'768;
// One wrap of the 24-bit reference time (64 ms units).
constexpr int64_t kReferenceWrapUs = (int64_t{1} << 24) * 64'000;

// No field trials: the upstream M131 defaults (loss-based v2 on, initial
// probes at 3x and 6x the start rate, congestion window pushback on).
class NoFieldTrials final : public webrtc::FieldTrialsView {
public:
    std::string Lookup(absl::string_view /*key*/) const override { return {}; }
};

Timestamp at_us(int64_t us) { return Timestamp::Micros(us); }

struct Record {
    int64_t sequence = 0;
    int64_t send_time_us = 0;
    size_t bytes = 0;
    webrtc::PacedPacketInfo pacing_info;
    bool reported = false;
};

}  // namespace

struct GoogCcController::Impl {
    NoFieldTrials trials;
    webrtc::Environment env;
    std::unique_ptr<webrtc::NetworkControllerInterface> controller;
    uint64_t min_bps = 0;
    uint64_t max_bps = 0;

    std::deque<Record> history;
    int64_t newest_sequence = -1;
    // Every record before this sequence is reported, or never will be.
    int64_t scan_from = 0;
    uint64_t in_flight_bytes = 0;

    bool has_reference = false;
    uint32_t last_reference = 0;
    int64_t reference_offset_us = 0;

    // Copies the outputs of one update into the controller.
    static void apply(GoogCcController& self, const webrtc::NetworkControlUpdate& update);

    Impl() : env(webrtc::CreateEnvironment(static_cast<const webrtc::FieldTrialsView*>(&trials))) {}

    int64_t unwrap_sequence(uint16_t sequence) const {
        if (newest_sequence < 0) {
            return sequence;
        }
        const int16_t delta = static_cast<int16_t>(
            static_cast<uint16_t>(sequence - static_cast<uint16_t>(newest_sequence & 0xffff)));
        return newest_sequence + delta;
    }

    // Arrival times from the parser start at the 24-bit reference. Keep them
    // monotonic across its wrap (every 12.4 days of receiver uptime).
    int64_t unwrap_reference_offset(uint32_t reference) {
        if (has_reference) {
            int32_t step = static_cast<int32_t>((reference - last_reference) & 0x00ff'ffff);
            if (step >= 0x80'0000) {
                step -= 0x100'0000;
            }
            if (static_cast<int64_t>(last_reference) + step >= 0x100'0000) {
                reference_offset_us += kReferenceWrapUs;
            } else if (static_cast<int64_t>(last_reference) + step < 0) {
                reference_offset_us -= kReferenceWrapUs;
            }
        }
        has_reference = true;
        last_reference = reference;
        return reference_offset_us;
    }

    void evict(int64_t now_us) {
        while (!history.empty()
               && (history.size() > kMaxHistoryPackets
                   || history.front().send_time_us < now_us - kHistoryUs)) {
            if (!history.front().reported) {
                in_flight_bytes -= std::min<uint64_t>(in_flight_bytes, history.front().bytes);
            }
            history.pop_front();
        }
    }

    Record* find(int64_t sequence) {
        if (history.empty() || sequence < history.front().sequence
            || sequence > history.back().sequence) {
            return nullptr;
        }
        Record& record =
            history[static_cast<size_t>(sequence - history.front().sequence)];
        return record.sequence == sequence ? &record : nullptr;
    }
};

bool GoogCcController::available() noexcept { return true; }

GoogCcController::GoogCcController(Config config, int64_t now_us)
    : impl_(std::make_unique<Impl>()) {
    impl_->min_bps = config.min_bps;
    impl_->max_bps = std::max(config.max_bps, config.min_bps);
    const uint64_t start =
        std::clamp(config.start_bps, impl_->min_bps, impl_->max_bps);

    webrtc::NetworkControllerConfig controller_config(impl_->env);
    controller_config.constraints.at_time = at_us(now_us);
    controller_config.constraints.min_data_rate = DataRate::BitsPerSec(impl_->min_bps);
    controller_config.constraints.max_data_rate = DataRate::BitsPerSec(impl_->max_bps);
    controller_config.constraints.starting_rate = DataRate::BitsPerSec(start);
    controller_config.stream_based_config.at_time = at_us(now_us);
    controller_config.stream_based_config.max_total_allocated_bitrate =
        DataRate::BitsPerSec(impl_->max_bps);
    webrtc::GoogCcFactoryConfig factory_config;
    factory_config.feedback_only = true;
    webrtc::GoogCcNetworkControllerFactory factory(std::move(factory_config));
    impl_->controller = factory.Create(std::move(controller_config));

    target_bps_ = start;
    pacing_bps_ = start * 5 / 2;
    webrtc::NetworkAvailability availability;
    availability.at_time = at_us(now_us);
    availability.network_available = true;
    Impl::apply(*this, impl_->controller->OnNetworkAvailability(availability));
}

GoogCcController::~GoogCcController() = default;

void GoogCcController::Impl::apply(GoogCcController& self, const webrtc::NetworkControlUpdate& update) {
    if (update.target_rate) {
        self.target_bps_ = static_cast<uint64_t>(
            std::max<int64_t>(update.target_rate->target_rate.bps(), 0));
        self.cwnd_reduce_ratio_ = std::clamp(update.target_rate->cwnd_reduce_ratio, 0.0, 1.0);
    }
    if (update.pacer_config) {
        self.pacing_bps_ = static_cast<uint64_t>(
            std::max<int64_t>(update.pacer_config->data_rate().bps(), 0));
        self.padding_bps_ = static_cast<uint64_t>(
            std::max<int64_t>(update.pacer_config->pad_rate().bps(), 0));
    }
    for (const auto& probe : update.probe_cluster_configs) {
        SendProbeCluster cluster;
        cluster.id = probe.id;
        cluster.rate_bps = static_cast<uint64_t>(probe.target_data_rate.bps());
        cluster.min_probes = probe.target_probe_count;
        cluster.min_bytes = static_cast<uint64_t>(
            (probe.target_data_rate * probe.target_duration).bytes());
        self.probes_.push_back(cluster);
    }
}

// M131's default congestion window is "QueueSize:350,MinBitrate:30000,
// DropFrame:true": the target is not pushed back, and cwnd_reduce_ratio is
// the share of frames the encoder side may drop while the window is full.
// The encoder bitrate is the target; the sender's bounded queue drops frames
// (and asks for an IDR) when the link does not take them. Multiplying the
// target by (1 - ratio) took the encoder to the 300 kbps floor at every cap
// (the first stage 4 gate run).
uint64_t GoogCcController::encoder_target_bps() const noexcept {
    return target_bps_;
}

void GoogCcController::on_packet_sent(const SentPacketInfo& packet) {
    Impl& impl = *impl_;
    Record record;
    record.sequence = impl.newest_sequence < 0
        ? packet.transport_sequence
        : impl.unwrap_sequence(packet.transport_sequence);
    if (impl.newest_sequence >= 0 && record.sequence <= impl.newest_sequence) {
        // The stamper counts up by one; anything else is not a new packet.
        return;
    }
    record.send_time_us = packet.send_time_us;
    record.bytes = packet.rtp_bytes + kPacketOverheadBytes;
    if (packet.probe_cluster_id >= 0) {
        record.pacing_info = webrtc::PacedPacketInfo(
            packet.probe_cluster_id, packet.probe_min_probes, packet.probe_min_bytes);
    }
    impl.newest_sequence = record.sequence;
    impl.history.push_back(record);
    impl.in_flight_bytes += record.bytes;
    impl.evict(packet.send_time_us);
    in_flight_bytes_ = impl.in_flight_bytes;

    webrtc::SentPacket sent;
    sent.send_time = at_us(record.send_time_us);
    sent.size = DataSize::Bytes(static_cast<int64_t>(record.bytes));
    sent.pacing_info = record.pacing_info;
    sent.sequence_number = record.sequence;
    sent.data_in_flight = DataSize::Bytes(static_cast<int64_t>(impl.in_flight_bytes));
    Impl::apply(*this, impl.controller->OnSentPacket(sent));
}

size_t GoogCcController::on_feedback(const TwccFeedback& feedback, int64_t now_us) {
    Impl& impl = *impl_;
    const int64_t offset_us = impl.unwrap_reference_offset(feedback.reference_time_64ms);
    webrtc::TransportPacketsFeedback report;
    report.feedback_time = at_us(now_us);
    report.packet_feedbacks.reserve(feedback.packets.size());
    if (!feedback.packets.empty()) {
        const int64_t first = impl.unwrap_sequence(feedback.packets.front().sequence);
        const int64_t from = impl.history.empty()
            ? 0
            : std::max<int64_t>(0, impl.scan_from - impl.history.front().sequence);
        for (size_t i = static_cast<size_t>(from); i < impl.history.size(); ++i) {
            Record& record = impl.history[i];
            if (record.sequence >= first) {
                break;
            }
            if (!record.reported) {
                // A lost report: these packets are never reported. They
                // leave the in-flight count, or it grows for 60 s.
                record.reported = true;
                impl.in_flight_bytes -= std::min<uint64_t>(impl.in_flight_bytes, record.bytes);
            }
        }
        impl.scan_from = std::max(impl.scan_from, first);
    }
    for (const auto& packet : feedback.packets) {
        Record* record = impl.find(impl.unwrap_sequence(packet.sequence));
        if (record == nullptr || record->reported) {
            continue;
        }
        record->reported = true;
        impl.in_flight_bytes -= std::min<uint64_t>(impl.in_flight_bytes, record->bytes);
        webrtc::PacketResult result;
        result.sent_packet.send_time = at_us(record->send_time_us);
        result.sent_packet.size = DataSize::Bytes(static_cast<int64_t>(record->bytes));
        result.sent_packet.pacing_info = record->pacing_info;
        result.sent_packet.sequence_number = record->sequence;
        if (packet.received) {
            result.receive_time = at_us(
                packet.arrival_time_us + offset_us);
        }
        report.packet_feedbacks.push_back(result);
    }
    in_flight_bytes_ = impl.in_flight_bytes;
    if (report.packet_feedbacks.empty()) {
        return 0;
    }
    report.data_in_flight = DataSize::Bytes(static_cast<int64_t>(impl.in_flight_bytes));
    // goog_cc wants the packets in send order.
    std::sort(report.packet_feedbacks.begin(), report.packet_feedbacks.end(),
              [](const webrtc::PacketResult& a, const webrtc::PacketResult& b) {
                  return a.sent_packet.sequence_number < b.sent_packet.sequence_number;
              });
    const size_t matched = report.packet_feedbacks.size();
    Impl::apply(*this, impl.controller->OnTransportPacketsFeedback(report));
    return matched;
}

void GoogCcController::on_process_interval(int64_t now_us, uint64_t pacer_queue_bytes) {
    impl_->evict(now_us);
    in_flight_bytes_ = impl_->in_flight_bytes;
    webrtc::ProcessInterval interval;
    interval.at_time = at_us(now_us);
    interval.pacer_queue = DataSize::Bytes(static_cast<int64_t>(pacer_queue_bytes));
    Impl::apply(*this, impl_->controller->OnProcessInterval(interval));
}

void GoogCcController::on_route_change(int64_t now_us) {
    webrtc::NetworkRouteChange change;
    change.at_time = at_us(now_us);
    change.constraints.at_time = at_us(now_us);
    change.constraints.min_data_rate = DataRate::BitsPerSec(impl_->min_bps);
    change.constraints.max_data_rate = DataRate::BitsPerSec(impl_->max_bps);
    change.constraints.starting_rate = DataRate::BitsPerSec(
        std::clamp(target_bps_, impl_->min_bps, impl_->max_bps));
    // The packets of the outage will never be reported.
    for (auto& record : impl_->history) {
        record.reported = true;
    }
    impl_->in_flight_bytes = 0;
    in_flight_bytes_ = 0;
    Impl::apply(*this, impl_->controller->OnNetworkRouteChange(change));
}

void GoogCcController::set_bitrate_limits(int64_t now_us, uint64_t min_bps, uint64_t max_bps) {
    impl_->min_bps = min_bps;
    impl_->max_bps = std::max(max_bps, min_bps);
    webrtc::TargetRateConstraints constraints;
    constraints.at_time = at_us(now_us);
    constraints.min_data_rate = DataRate::BitsPerSec(impl_->min_bps);
    constraints.max_data_rate = DataRate::BitsPerSec(impl_->max_bps);
    Impl::apply(*this, impl_->controller->OnTargetRateConstraints(constraints));
}

void GoogCcController::set_max_total_allocated(int64_t now_us, uint64_t bps) {
    webrtc::StreamsConfig streams;
    streams.at_time = at_us(now_us);
    streams.max_total_allocated_bitrate = DataRate::BitsPerSec(static_cast<int64_t>(bps));
    Impl::apply(*this, impl_->controller->OnStreamsConfig(streams));
}

std::vector<SendProbeCluster> GoogCcController::take_probe_clusters() {
    std::vector<SendProbeCluster> out;
    out.swap(probes_);
    return out;
}

}  // namespace mello::transport

#else  // !MELLO_HAS_GOOG_CC

namespace mello::transport {

struct GoogCcController::Impl {};

bool GoogCcController::available() noexcept { return false; }
GoogCcController::GoogCcController(Config config, int64_t) : target_bps_(config.start_bps) {}
GoogCcController::~GoogCcController() = default;
uint64_t GoogCcController::encoder_target_bps() const noexcept { return target_bps_; }
void GoogCcController::on_packet_sent(const SentPacketInfo&) {}
size_t GoogCcController::on_feedback(const TwccFeedback&, int64_t) { return 0; }
void GoogCcController::on_process_interval(int64_t, uint64_t) {}
void GoogCcController::on_route_change(int64_t) {}
void GoogCcController::set_bitrate_limits(int64_t, uint64_t, uint64_t) {}
void GoogCcController::set_max_total_allocated(int64_t, uint64_t) {}
std::vector<SendProbeCluster> GoogCcController::take_probe_clusters() { return {}; }

}  // namespace mello::transport

#endif
