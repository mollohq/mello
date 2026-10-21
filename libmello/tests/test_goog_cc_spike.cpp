// Stage 4 spike: WebRTC goog_cc (M131) against the current send-side
// estimator (GccEstimator in src/transport/twcc.cpp).
//
// A deterministic link simulator drives both controllers with the same
// traces. The sender has an encoder model (60 fps, 1130-byte packets at the
// controller target) and a pacer (the controller pacing rate, plus probe
// clusters for goog_cc). The link has a capacity cap, a tail-drop queue of
// 100 ms and a fixed one-way delay of 20 ms. The receiver sends TWCC feedback
// every 100 ms in the shape that parse_twcc_feedback gives (TwccFeedback:
// per-packet sequence, received flag and arrival time). Feedback takes 20 ms
// back to the sender.
//
// goog_cc runs through its NetworkControllerInterface (OnSentPacket,
// OnTransportPacketsFeedback, OnProcessInterval every 25 ms) with a mello
// FieldTrialsView. The current estimator gets each feedback packet through
// GccEstimator::on_packet, as rtp_video_sender.cpp does, and its pacer runs at
// 1.08 times its target (the manager headroom of pacer.rs). Nothing here
// touches the stream sender.
//
// Each scenario prints its numbers with the prefix "[goog-cc-spike]" and
// fails when goog_cc misbehaves. Run one scenario alone with
//   mello_goog_cc_spike_tests --gtest_filter=GoogCcSpike.<Name>
//
// Thread model: single thread. The simulation calls each controller from the
// test thread in simulated-time order.

#include <gtest/gtest.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <deque>
#include <functional>
#include <limits>
#include <map>
#include <memory>
#include <new>
#include <optional>
#include <string>
#include <utility>
#include <vector>

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
#include "modules/congestion_controller/goog_cc/inter_arrival_delta.h"

#include "transport/twcc.hpp"

// ---------------------------------------------------------------------------
// Heap counter. This executable replaces the global operator new and delete
// so the RAM measurement can count the C++ heap that one controller holds.
// It counts live bytes only.
// ---------------------------------------------------------------------------
namespace {
std::atomic<int64_t> g_live_heap_bytes{0};
constexpr size_t kHeapHeader = 16;  // keeps the user block 16-byte aligned
}  // namespace

void* operator new(size_t size) {
    void* raw = std::malloc(size + kHeapHeader);
    if (!raw) throw std::bad_alloc();
    *static_cast<size_t*>(raw) = size;
    g_live_heap_bytes.fetch_add(static_cast<int64_t>(size), std::memory_order_relaxed);
    return static_cast<char*>(raw) + kHeapHeader;
}

void operator delete(void* p) noexcept {
    if (!p) return;
    void* raw = static_cast<char*>(p) - kHeapHeader;
    g_live_heap_bytes.fetch_sub(static_cast<int64_t>(*static_cast<size_t*>(raw)),
                                std::memory_order_relaxed);
    std::free(raw);
}

namespace {

using mello::transport::GccEstimator;
using mello::transport::TwccFeedback;
using mello::transport::TwccPacketResult;
using webrtc::DataRate;
using webrtc::DataSize;
using webrtc::TimeDelta;
using webrtc::Timestamp;

// ---------------------------------------------------------------------------
// Simulation constants
// ---------------------------------------------------------------------------
constexpr int kPacketBytes = 1130;
constexpr int64_t kOneWayDelayUs = 20'000;
constexpr int64_t kQueueLimitUs = 100'000;
constexpr int64_t kFeedbackIntervalUs = 100'000;
constexpr int64_t kProcessIntervalUs = 25'000;
constexpr int64_t kFramesPerSecond = 60;
constexpr int64_t kStartBps = 1'500'000;
constexpr int64_t kMinBps = 300'000;
constexpr int64_t kMaxBps = 12'000'000;
constexpr int64_t kLinkBps = 6'000'000;
// The current pacer runs at the manager target, which is 1.08 times the
// encoder bitrate (pacer.rs). The encoder follows the GCC target.
constexpr double kCurrentPacingFactor = 1.08;
// Encoder queue limit: frames older than this are dropped by the encoder
// model (the AU queue overflow of the real sender).
constexpr int64_t kMaxQueuedBytes = 3'000'000;
// webrtc::Timestamp needs a finite base. Simulated time 0 maps to 100 s.
constexpr int64_t kEpochUs = 100'000'000;

constexpr int64_t sec(double s) { return static_cast<int64_t>(s * 1e6); }
Timestamp to_ts(int64_t us) { return Timestamp::Micros(kEpochUs + us); }

// xorshift64*: deterministic on every platform and standard library.
class Rng {
public:
    explicit Rng(uint64_t seed) : state_(seed ? seed : 1) {}
    uint64_t next() {
        state_ ^= state_ >> 12;
        state_ ^= state_ << 25;
        state_ ^= state_ >> 27;
        return state_ * 2685821657736338717ULL;
    }
    // Uniform in [0, 1).
    double uniform() { return static_cast<double>(next() >> 11) * (1.0 / 9007199254740992.0); }

private:
    uint64_t state_;
};

// ---------------------------------------------------------------------------
// Mello field trials: an explicit key-value map. Empty means upstream
// defaults (loss-based v2 on, probes at 3x and 6x the start rate, congestion
// window pushback on).
// ---------------------------------------------------------------------------
class MelloFieldTrials : public webrtc::FieldTrialsView {
public:
    MelloFieldTrials() = default;
    explicit MelloFieldTrials(std::map<std::string, std::string> trials)
        : trials_(std::move(trials)) {}
    std::string Lookup(absl::string_view key) const override {
        auto it = trials_.find(std::string(key));
        return it == trials_.end() ? std::string() : it->second;
    }

private:
    std::map<std::string, std::string> trials_;
};

// ---------------------------------------------------------------------------
// Link: capacity cap, tail-drop queue (100 ms), fixed one-way delay, optional
// outage window and random loss.
// ---------------------------------------------------------------------------
struct LinkProfile {
    // Capacity steps: (start time us, bps), sorted by time.
    std::vector<std::pair<int64_t, int64_t>> capacity{{0, kLinkBps}};
    int64_t outage_start_us = -1;
    int64_t outage_end_us = -1;
    double random_loss = 0.0;
    // Extra one-way delay, uniform in [0, jitter_us), FIFO kept.
    int64_t jitter_us = 0;
    uint64_t seed = 0x5eed;

    int64_t capacity_at(int64_t t_us) const {
        int64_t bps = capacity.front().second;
        for (const auto& [at, value] : capacity) {
            if (at <= t_us) bps = value;
        }
        return bps;
    }
    bool in_outage(int64_t t_us) const {
        return outage_start_us >= 0 && t_us >= outage_start_us && t_us < outage_end_us;
    }
};

struct Arrival {
    int64_t seq = 0;
    int64_t at_us = 0;
};

class Link {
public:
    explicit Link(const LinkProfile& profile) : profile_(profile), rng_(profile.seed) {}

    // Returns the queue delay of the packet, or -1 when the link drops it.
    int64_t send(int64_t seq, int64_t now_us, int bytes) {
        if (profile_.in_outage(now_us)) return -1;
        if (profile_.random_loss > 0.0 && rng_.uniform() < profile_.random_loss) return -1;
        const int64_t start = std::max(now_us, free_at_us_);
        const int64_t queue_delay = start - now_us;
        if (queue_delay > kQueueLimitUs) return -1;
        const int64_t bps = profile_.capacity_at(now_us);
        free_at_us_ = start + (static_cast<int64_t>(bytes) * 8 * 1'000'000 + bps - 1) / bps;
        int64_t at = free_at_us_ + kOneWayDelayUs;
        if (profile_.jitter_us > 0) {
            at += static_cast<int64_t>(rng_.next() % static_cast<uint64_t>(profile_.jitter_us));
            if (!arrivals_.empty()) at = std::max(at, arrivals_.back().at_us);
            at = std::max(at, last_at_us_);
            last_at_us_ = at;
        }
        arrivals_.push_back({seq, at});
        return queue_delay;
    }

    std::deque<Arrival>& arrivals() { return arrivals_; }

private:
    LinkProfile profile_;
    Rng rng_;
    int64_t free_at_us_ = 0;
    int64_t last_at_us_ = 0;
    std::deque<Arrival> arrivals_;
};

// Receiver: one TWCC report every 100 ms covering every sequence from the
// last report up to the highest received one. The link is FIFO, so a missing
// sequence below the highest received one is lost. No report when nothing
// arrived (TwccFeedbackGenerator::build_feedback returns empty then).
class Receiver {
public:
    std::optional<TwccFeedback> report(int64_t now_us, Link& link) {
        auto& arrivals = link.arrivals();
        while (!arrivals.empty() && arrivals.front().at_us <= now_us) {
            got_.push_back(arrivals.front());
            arrivals.pop_front();
        }
        if (got_.empty()) return std::nullopt;
        TwccFeedback feedback;
        feedback.feedback_packet_count = count_++;
        const int64_t last = got_.back().seq;
        size_t index = 0;
        for (int64_t seq = next_seq_; seq <= last; ++seq) {
            TwccPacketResult result;
            result.sequence = static_cast<uint16_t>(seq & 0xffff);
            if (index < got_.size() && got_[index].seq == seq) {
                result.received = true;
                result.arrival_time_us = got_[index].at_us;
                ++index;
            }
            feedback.packets.push_back(result);
        }
        next_seq_ = last + 1;
        got_.clear();
        return feedback;
    }

private:
    int64_t next_seq_ = 0;
    uint8_t count_ = 0;
    std::vector<Arrival> got_;
};

// ---------------------------------------------------------------------------
// Sender-side records and the controller interface of the simulator.
// ---------------------------------------------------------------------------
struct SentRecord {
    int64_t seq = 0;
    int64_t send_us = 0;
    int bytes = 0;
    webrtc::PacedPacketInfo pacing_info;
    bool reported = false;
};

struct ProbeCluster {
    int id = 0;
    int64_t rate_bps = 0;
    int min_probes = 0;
    int64_t min_bytes = 0;
    int sent_probes = 0;
    int64_t sent_bytes = 0;
};

class SimController {
public:
    virtual ~SimController() = default;
    virtual const char* name() const = 0;
    virtual void start(int64_t now_us) = 0;
    virtual void on_sent(const SentRecord& record, int64_t in_flight_bytes) = 0;
    // `history` is indexed by the unwrapped transport sequence.
    virtual void on_feedback(const TwccFeedback& feedback,
                             const std::vector<SentRecord>& history,
                             const std::vector<int64_t>& unwrapped,
                             int64_t now_us,
                             int64_t in_flight_bytes) = 0;
    virtual void on_process(int64_t now_us, int64_t pacer_queue_bytes) = 0;

    // Outputs, read by the simulator after each call.
    int64_t target_bps = 0;
    int64_t pacing_bps = 0;
    int64_t padding_bps = 0;
    double cwnd_reduce_ratio = 0.0;
    std::optional<int64_t> congestion_window_bytes;
    bool ever_congestion_window = false;
    std::vector<ProbeCluster> new_probes;

    // CPU per call, in nanoseconds.
    std::vector<int64_t> feedback_ns;
    std::vector<int64_t> process_ns;
    std::vector<int64_t> sent_ns;
    std::vector<int64_t> feedback_packets;
};

int64_t elapsed_ns(std::chrono::steady_clock::time_point since) {
    return std::chrono::duration_cast<std::chrono::nanoseconds>(
               std::chrono::steady_clock::now() - since)
        .count();
}

// goog_cc through webrtc::NetworkControllerInterface. The wrapper is the
// stage 4 adapter in small: send records to SentPacket, TWCC feedback to
// TransportPacketsFeedback, NetworkControlUpdate to pacer and encoder.
class GoogCcController final : public SimController {
public:
    explicit GoogCcController(std::map<std::string, std::string> trials = {},
                              bool alr_probing = false)
        : trials_(std::move(trials)),
          env_(webrtc::CreateEnvironment(static_cast<const webrtc::FieldTrialsView*>(&trials_))),
          start_bps_(kStartBps),
          alr_probing_(alr_probing) {}

    const char* name() const override { return "goog_cc"; }

    void start(int64_t now_us) override {
        webrtc::NetworkControllerConfig config(env_);
        config.constraints.at_time = to_ts(now_us);
        config.constraints.min_data_rate = DataRate::BitsPerSec(kMinBps);
        config.constraints.max_data_rate = DataRate::BitsPerSec(kMaxBps);
        config.constraints.starting_rate = DataRate::BitsPerSec(start_bps_);
        if (alr_probing_) config.stream_based_config.requests_alr_probing = true;
        // feedback_only: loss and RTT come from TWCC feedback, as mello has
        // no RTCP receiver report path into the estimator.
        webrtc::GoogCcFactoryConfig factory_config;
        factory_config.feedback_only = true;
        webrtc::GoogCcNetworkControllerFactory factory(std::move(factory_config));
        controller_ = factory.Create(config);
        target_bps = start_bps_;
        pacing_bps = static_cast<int64_t>(start_bps_ * 2.5);
        webrtc::NetworkAvailability availability;
        availability.at_time = to_ts(now_us);
        availability.network_available = true;
        apply(controller_->OnNetworkAvailability(availability));
    }

    void on_sent(const SentRecord& record, int64_t in_flight_bytes) override {
        webrtc::SentPacket packet;
        packet.send_time = to_ts(record.send_us);
        packet.size = DataSize::Bytes(record.bytes);
        packet.pacing_info = record.pacing_info;
        packet.sequence_number = record.seq;
        packet.data_in_flight = DataSize::Bytes(in_flight_bytes);
        const auto begin = std::chrono::steady_clock::now();
        webrtc::NetworkControlUpdate update = controller_->OnSentPacket(packet);
        sent_ns.push_back(elapsed_ns(begin));
        apply(update);
    }

    void on_feedback(const TwccFeedback& feedback,
                     const std::vector<SentRecord>& history,
                     const std::vector<int64_t>& unwrapped,
                     int64_t now_us,
                     int64_t in_flight_bytes) override {
        webrtc::TransportPacketsFeedback report;
        report.feedback_time = to_ts(now_us);
        report.data_in_flight = DataSize::Bytes(in_flight_bytes);
        report.packet_feedbacks.reserve(feedback.packets.size());
        for (size_t i = 0; i < feedback.packets.size(); ++i) {
            const SentRecord& record = history[static_cast<size_t>(unwrapped[i])];
            webrtc::PacketResult result;
            result.sent_packet.send_time = to_ts(record.send_us);
            result.sent_packet.size = DataSize::Bytes(record.bytes);
            result.sent_packet.pacing_info = record.pacing_info;
            result.sent_packet.sequence_number = record.seq;
            if (feedback.packets[i].received) {
                result.receive_time = to_ts(feedback.packets[i].arrival_time_us);
            }
            report.packet_feedbacks.push_back(result);
        }
        const auto begin = std::chrono::steady_clock::now();
        webrtc::NetworkControlUpdate update = controller_->OnTransportPacketsFeedback(report);
        feedback_ns.push_back(elapsed_ns(begin));
        feedback_packets.push_back(static_cast<int64_t>(feedback.packets.size()));
        apply(update);
    }

    void on_process(int64_t now_us, int64_t pacer_queue_bytes) override {
        webrtc::ProcessInterval interval;
        interval.at_time = to_ts(now_us);
        interval.pacer_queue = DataSize::Bytes(pacer_queue_bytes);
        const auto begin = std::chrono::steady_clock::now();
        webrtc::NetworkControlUpdate update = controller_->OnProcessInterval(interval);
        process_ns.push_back(elapsed_ns(begin));
        apply(update);
    }

    void reset_controller() { controller_.reset(); }

    // The stage 4 QualityController analog of a new encoder allocation:
    // OnStreamsConfig with a new max total allocated bitrate. goog_cc probes
    // up to the new allocation (ProbeController::OnMaxTotalAllocatedBitrate).
    void set_max_allocated(int64_t now_us, int64_t bps) {
        webrtc::StreamsConfig streams;
        streams.at_time = to_ts(now_us);
        streams.max_total_allocated_bitrate = DataRate::BitsPerSec(bps);
        apply(controller_->OnStreamsConfig(streams));
    }

private:
    void apply(const webrtc::NetworkControlUpdate& update) {
        if (update.target_rate) {
            target_bps = update.target_rate->target_rate.bps();
            cwnd_reduce_ratio = update.target_rate->cwnd_reduce_ratio;
        }
        if (update.pacer_config) {
            pacing_bps = update.pacer_config->data_rate().bps();
            padding_bps = update.pacer_config->pad_rate().bps();
        }
        if (update.congestion_window) {
            congestion_window_bytes = update.congestion_window->IsFinite()
                                          ? std::optional<int64_t>(update.congestion_window->bytes())
                                          : std::nullopt;
            if (congestion_window_bytes) ever_congestion_window = true;
        }
        for (const auto& probe : update.probe_cluster_configs) {
            ProbeCluster cluster;
            cluster.id = probe.id;
            cluster.rate_bps = probe.target_data_rate.bps();
            cluster.min_probes = probe.target_probe_count;
            cluster.min_bytes = (probe.target_data_rate * probe.target_duration).bytes();
            new_probes.push_back(cluster);
        }
    }

    MelloFieldTrials trials_;
    webrtc::Environment env_;
    int64_t start_bps_;
    bool alr_probing_;
    std::unique_ptr<webrtc::NetworkControllerInterface> controller_;
};

// The current estimator, fed per feedback packet as
// RtpVideoSender::State::handle_twcc_feedback does.
class CurrentController final : public SimController {
public:
    explicit CurrentController(int64_t start_bps) : estimator_(GccEstimator::Config{}, static_cast<uint64_t>(start_bps)) {}

    const char* name() const override { return "current"; }

    void start(int64_t) override { publish(); }

    void on_sent(const SentRecord&, int64_t) override {}

    void on_feedback(const TwccFeedback& feedback,
                     const std::vector<SentRecord>& history,
                     const std::vector<int64_t>& unwrapped,
                     int64_t,
                     int64_t) override {
        const auto begin = std::chrono::steady_clock::now();
        for (size_t i = 0; i < feedback.packets.size(); ++i) {
            const auto& result = feedback.packets[i];
            const int64_t send_time =
                result.received ? history[static_cast<size_t>(unwrapped[i])].send_us : -1;
            estimator_.on_packet(result.sequence, result.received, send_time, result.arrival_time_us);
        }
        feedback_ns.push_back(elapsed_ns(begin));
        feedback_packets.push_back(static_cast<int64_t>(feedback.packets.size()));
        publish();
    }

    void on_process(int64_t, int64_t) override {}

    const GccEstimator& estimator() const { return estimator_; }

private:
    void publish() {
        target_bps = static_cast<int64_t>(estimator_.target_bps());
        pacing_bps = static_cast<int64_t>(static_cast<double>(target_bps) * kCurrentPacingFactor);
    }

    GccEstimator estimator_;
};

// ---------------------------------------------------------------------------
// The simulator.
// ---------------------------------------------------------------------------
struct Sample {
    int64_t t_us = 0;
    int64_t target_bps = 0;
    int64_t encoder_bps = 0;  // target after the congestion window reduce ratio
    int64_t pacing_bps = 0;
};

struct PacketLog {
    int64_t send_us = 0;
    int64_t queue_delay_us = 0;  // -1 when dropped
    int bytes = 0;
    bool probe = false;
};

struct TraceResult {
    std::vector<Sample> samples;
    std::vector<PacketLog> packets;
    int64_t encoder_dropped_bytes = 0;
    bool ever_congestion_window = false;
    double max_cwnd_reduce_ratio = 0.0;
};

class Simulation {
public:
    // `encoder_fill`: the share of the target that the encoder produces.
    // Below 1.0 the sender is application limited (ALR).
    Simulation(const LinkProfile& profile, SimController& controller, double encoder_fill = 1.0)
        : link_(profile), controller_(controller), encoder_fill_(encoder_fill) {}

    // Calls `hook` once, at the first process interval at or after `at_us`.
    void set_hook(int64_t at_us, std::function<void(int64_t)> hook) {
        hook_at_us_ = at_us;
        hook_ = std::move(hook);
    }

    TraceResult run(int64_t duration_us) {
        TraceResult result;
        controller_.start(0);
        take_probes();
        int64_t now = 0;
        int64_t frame_index = 0;
        int64_t next_frame = 0;
        int64_t next_report = kFeedbackIntervalUs;
        int64_t next_process = 0;
        constexpr int64_t kNever = std::numeric_limits<int64_t>::max();
        while (now < duration_us) {
            const int64_t next_delivery = in_transit_.empty() ? kNever : in_transit_.front().first;
            const int64_t next_pacer = pacer_ready_time(now);
            const int64_t t = std::min({next_frame, next_report, next_process, next_delivery, next_pacer});
            now = t;
            if (now >= duration_us) break;

            if (next_delivery == now) {
                TwccFeedback feedback = std::move(in_transit_.front().second);
                in_transit_.pop_front();
                deliver(feedback, now);
                continue;
            }
            if (next_process == now) {
                if (hook_ && now >= hook_at_us_) {
                    hook_(now);
                    hook_ = nullptr;
                    take_probes();
                }
                controller_.on_process(now, queued_bytes_);
                take_probes();
                result.samples.push_back({now, controller_.target_bps, encoder_bps(), controller_.pacing_bps});
                result.max_cwnd_reduce_ratio = std::max(result.max_cwnd_reduce_ratio, controller_.cwnd_reduce_ratio);
                next_process += kProcessIntervalUs;
                continue;
            }
            if (next_frame == now) {
                encode_frame(result);
                ++frame_index;
                next_frame = frame_index * 1'000'000 / kFramesPerSecond;
                continue;
            }
            if (next_report == now) {
                if (auto feedback = receiver_.report(now, link_)) {
                    in_transit_.emplace_back(now + kOneWayDelayUs, std::move(*feedback));
                }
                next_report += kFeedbackIntervalUs;
                continue;
            }
            if (next_pacer == now) {
                send_one(now, result);
                continue;
            }
        }
        result.ever_congestion_window = controller_.ever_congestion_window;
        return result;
    }

private:
    int64_t encoder_bps() const {
        return static_cast<int64_t>(static_cast<double>(controller_.target_bps) *
                                    (1.0 - controller_.cwnd_reduce_ratio));
    }

    void take_probes() {
        for (const auto& probe : controller_.new_probes) probes_.push_back(probe);
        controller_.new_probes.clear();
    }

    void encode_frame(TraceResult& result) {
        frame_carry_bytes_ += encoder_fill_ * static_cast<double>(encoder_bps()) / 8.0 / kFramesPerSecond;
        while (frame_carry_bytes_ >= kPacketBytes) {
            frame_carry_bytes_ -= kPacketBytes;
            if (queued_bytes_ + kPacketBytes > kMaxQueuedBytes) {
                result.encoder_dropped_bytes += kPacketBytes;
                continue;
            }
            queued_bytes_ += kPacketBytes;
        }
    }

    bool window_blocks() const {
        return controller_.congestion_window_bytes && in_flight_bytes_ >= *controller_.congestion_window_bytes;
    }

    int64_t pacer_ready_time(int64_t now) const {
        constexpr int64_t kNever = std::numeric_limits<int64_t>::max();
        if (window_blocks()) return kNever;
        const bool has_work = !probes_.empty() || queued_bytes_ >= kPacketBytes || controller_.padding_bps > 0;
        if (!has_work) return kNever;
        return std::max(next_send_us_, now);
    }

    void send_one(int64_t now, TraceResult& result) {
        SentRecord record;
        record.seq = static_cast<int64_t>(history_.size());
        record.send_us = now;
        record.bytes = kPacketBytes;
        int64_t rate = 0;
        if (!probes_.empty()) {
            ProbeCluster& cluster = probes_.front();
            rate = cluster.rate_bps;
            record.pacing_info = webrtc::PacedPacketInfo(cluster.id, cluster.min_probes,
                                                         static_cast<int>(cluster.min_bytes));
            if (queued_bytes_ >= kPacketBytes) queued_bytes_ -= kPacketBytes;  // media first, else padding
            cluster.sent_probes += 1;
            cluster.sent_bytes += kPacketBytes;
            if (cluster.sent_probes >= cluster.min_probes && cluster.sent_bytes >= cluster.min_bytes) {
                probes_.pop_front();
            }
        } else if (queued_bytes_ >= kPacketBytes) {
            rate = controller_.pacing_bps;
            queued_bytes_ -= kPacketBytes;
        } else {
            rate = controller_.padding_bps;
        }
        rate = std::max<int64_t>(rate, 10'000);
        next_send_us_ = std::max(next_send_us_, now) + static_cast<int64_t>(kPacketBytes) * 8 * 1'000'000 / rate;

        history_.push_back(record);
        in_flight_bytes_ += record.bytes;
        controller_.on_sent(record, in_flight_bytes_);
        take_probes();
        const int64_t queue_delay = link_.send(record.seq, now, record.bytes);
        result.packets.push_back({now, queue_delay, record.bytes,
                                  record.pacing_info.probe_cluster_id != webrtc::PacedPacketInfo::kNotAProbe});
    }

    void deliver(const TwccFeedback& feedback, int64_t now) {
        std::vector<int64_t> unwrapped;
        unwrapped.reserve(feedback.packets.size());
        for (const auto& packet : feedback.packets) {
            // Unwrap the 16-bit transport sequence against the last one.
            const int16_t delta = static_cast<int16_t>(
                static_cast<uint16_t>(packet.sequence - static_cast<uint16_t>(last_unwrapped_ & 0xffff)));
            const int64_t seq = last_unwrapped_ + delta;
            last_unwrapped_ = seq;
            unwrapped.push_back(seq);
            SentRecord& record = history_[static_cast<size_t>(seq)];
            if (!record.reported) {
                record.reported = true;
                in_flight_bytes_ -= record.bytes;
            }
        }
        controller_.on_feedback(feedback, history_, unwrapped, now, in_flight_bytes_);
        take_probes();
    }

    Link link_;
    Receiver receiver_;
    SimController& controller_;
    double encoder_fill_;
    int64_t hook_at_us_ = 0;
    std::function<void(int64_t)> hook_;
    std::vector<SentRecord> history_;
    std::deque<std::pair<int64_t, TwccFeedback>> in_transit_;
    std::deque<ProbeCluster> probes_;
    int64_t last_unwrapped_ = 0;
    int64_t in_flight_bytes_ = 0;
    int64_t queued_bytes_ = 0;
    double frame_carry_bytes_ = 0.0;
    int64_t next_send_us_ = 0;
};

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------
constexpr double kNotReached = -1.0;

// First time at or after `from_us` where the target is at or above `bps`,
// in seconds after `from_us`.
double time_to_reach(const TraceResult& r, int64_t from_us, int64_t bps) {
    for (const auto& s : r.samples) {
        if (s.t_us >= from_us && s.target_bps >= bps) return static_cast<double>(s.t_us - from_us) / 1e6;
    }
    return kNotReached;
}

// Time from `from_us` until the target is at or above `bps` again after it
// first went under `bps` at or after `from_us` (0 when it never went under).
double time_to_recover(const TraceResult& r, int64_t from_us, int64_t bps) {
    bool fell = false;
    for (const auto& s : r.samples) {
        if (s.t_us < from_us) continue;
        if (!fell && s.target_bps < bps) fell = true;
        if (fell && s.target_bps >= bps) return static_cast<double>(s.t_us - from_us) / 1e6;
    }
    return fell ? kNotReached : 0.0;
}

double time_to_fall(const TraceResult& r, int64_t from_us, int64_t bps) {
    for (const auto& s : r.samples) {
        if (s.t_us >= from_us && s.target_bps <= bps) return static_cast<double>(s.t_us - from_us) / 1e6;
    }
    return kNotReached;
}

int64_t target_at(const TraceResult& r, int64_t t_us) {
    int64_t value = 0;
    for (const auto& s : r.samples) {
        if (s.t_us > t_us) break;
        value = s.target_bps;
    }
    return value;
}

int64_t encoder_at(const TraceResult& r, int64_t t_us) {
    int64_t value = 0;
    for (const auto& s : r.samples) {
        if (s.t_us > t_us) break;
        value = s.encoder_bps;
    }
    return value;
}

double mean_target(const TraceResult& r, int64_t from_us, int64_t to_us) {
    double sum = 0;
    int n = 0;
    for (const auto& s : r.samples) {
        if (s.t_us >= from_us && s.t_us < to_us) {
            sum += static_cast<double>(s.target_bps);
            ++n;
        }
    }
    return n ? sum / n : 0.0;
}

double stddev_target(const TraceResult& r, int64_t from_us, int64_t to_us) {
    const double mean = mean_target(r, from_us, to_us);
    double sum = 0;
    int n = 0;
    for (const auto& s : r.samples) {
        if (s.t_us >= from_us && s.t_us < to_us) {
            const double d = static_cast<double>(s.target_bps) - mean;
            sum += d * d;
            ++n;
        }
    }
    return n ? std::sqrt(sum / n) : 0.0;
}

int64_t min_target(const TraceResult& r, int64_t from_us, int64_t to_us) {
    int64_t value = std::numeric_limits<int64_t>::max();
    for (const auto& s : r.samples) {
        if (s.t_us >= from_us && s.t_us < to_us) value = std::min(value, s.target_bps);
    }
    return value;
}

int64_t max_queue_delay_us(const TraceResult& r, int64_t from_us, int64_t to_us) {
    int64_t value = 0;
    for (const auto& p : r.packets) {
        if (p.send_us >= from_us && p.send_us < to_us) value = std::max(value, p.queue_delay_us);
    }
    return value;
}

int64_t bytes_sent(const TraceResult& r, int64_t from_us, int64_t to_us) {
    int64_t value = 0;
    for (const auto& p : r.packets) {
        if (p.send_us >= from_us && p.send_us < to_us) value += p.bytes;
    }
    return value;
}

// Mean queue delay of the packets that the link accepted.
double mean_queue_delay_ms(const TraceResult& r, int64_t from_us, int64_t to_us) {
    double sum = 0;
    int64_t n = 0;
    for (const auto& p : r.packets) {
        if (p.send_us >= from_us && p.send_us < to_us && p.queue_delay_us >= 0) {
            sum += static_cast<double>(p.queue_delay_us);
            ++n;
        }
    }
    return n ? sum / static_cast<double>(n) / 1000.0 : 0.0;
}

double loss_fraction(const TraceResult& r, int64_t from_us, int64_t to_us) {
    int64_t sent = 0;
    int64_t lost = 0;
    for (const auto& p : r.packets) {
        if (p.send_us >= from_us && p.send_us < to_us) {
            ++sent;
            if (p.queue_delay_us < 0) ++lost;
        }
    }
    return sent ? static_cast<double>(lost) / static_cast<double>(sent) : 0.0;
}

double mbps(double bps) { return bps / 1e6; }
double mbps(int64_t bps) { return static_cast<double>(bps) / 1e6; }

std::string fmt_time(double s) {
    if (s < 0) return "never";
    char buffer[32];
    std::snprintf(buffer, sizeof(buffer), "%.2f s", s);
    return buffer;
}

struct Percentiles {
    double mean_us = 0;
    double p99_us = 0;
};

Percentiles cpu_us(std::vector<int64_t> ns) {
    Percentiles p;
    if (ns.empty()) return p;
    double sum = 0;
    for (int64_t v : ns) sum += static_cast<double>(v);
    p.mean_us = sum / static_cast<double>(ns.size()) / 1000.0;
    std::sort(ns.begin(), ns.end());
    p.p99_us = static_cast<double>(ns[ns.size() * 99 / 100]) / 1000.0;
    return p;
}

TraceResult run_trace(const LinkProfile& profile, SimController& controller, int64_t duration_us,
                      double encoder_fill = 1.0) {
    Simulation sim(profile, controller, encoder_fill);
    return sim.run(duration_us);
}

// The current estimator starts at the manager target in production
// (RtpVideoSender::State, initial_pacing_target_bps), so the congestion,
// outage and loss traces start it at 90 % of the link. Trace 1 shows that it
// does not climb from 1.5 Mbps.
constexpr int64_t kCurrentProductionStartBps = kLinkBps * 9 / 10;

}  // namespace

// ---------------------------------------------------------------------------
// 1. Start-up on a 6 Mbps link from 1.5 Mbps.
// ---------------------------------------------------------------------------
TEST(GoogCcSpike, StartupRamp) {
    LinkProfile profile;
    const int64_t duration = sec(30);
    const int64_t ninety = kLinkBps * 9 / 10;

    GoogCcController goog;
    const TraceResult g = run_trace(profile, goog, duration);
    CurrentController current(kStartBps);
    const TraceResult c = run_trace(profile, current, duration);

    const double g_t90 = time_to_reach(g, 0, ninety);
    const double c_t90 = time_to_reach(c, 0, ninety);
    std::printf("[goog-cc-spike] startup: time to 90%% of 6 Mbps goog_cc=%s current=%s\n",
                fmt_time(g_t90).c_str(), fmt_time(c_t90).c_str());
    for (double t : {0.5, 1.0, 2.0, 5.0, 10.0, 29.9}) {
        std::printf("[goog-cc-spike] startup: t=%.1f s target goog_cc=%.2f current=%.2f Mbps\n", t,
                    mbps(target_at(g, sec(t))), mbps(target_at(c, sec(t))));
    }
    int probe_packets = 0;
    for (const auto& p : g.packets) probe_packets += p.probe ? 1 : 0;
    std::printf("[goog-cc-spike] startup: goog_cc probe packets=%d, queue delay peak first 5 s goog_cc=%.1f ms current=%.1f ms\n",
                probe_packets, max_queue_delay_us(g, 0, sec(5)) / 1000.0,
                max_queue_delay_us(c, 0, sec(5)) / 1000.0);
    std::printf("[goog-cc-spike] startup: mean target 10-30 s goog_cc=%.2f current=%.2f Mbps, loss 10-30 s goog_cc=%.2f%% current=%.2f%%\n",
                mbps(mean_target(g, sec(10), sec(30))), mbps(mean_target(c, sec(10), sec(30))),
                100 * loss_fraction(g, sec(10), sec(30)), 100 * loss_fraction(c, sec(10), sec(30)));

    EXPECT_GT(probe_packets, 0) << "goog_cc sent no start-up probes";
    ASSERT_GE(g_t90, 0.0) << "goog_cc never reached 90 % of the link";
    EXPECT_LT(g_t90, 3.0) << "stage 4 gate: the start reaches the preset within 3 s";
    EXPECT_TRUE(c_t90 < 0 || g_t90 < c_t90);
    EXPECT_LT(max_queue_delay_us(g, sec(10), sec(30)), 50'000) << "goog_cc keeps a standing queue";
}

// ---------------------------------------------------------------------------
// 2. Congestion: 6 Mbps, cap to 2.5 Mbps at 10 s for 20 s, then release.
// ---------------------------------------------------------------------------
TEST(GoogCcSpike, CongestionCap) {
    LinkProfile profile;
    constexpr int64_t kCapBps = 2'500'000;
    profile.capacity = {{0, kLinkBps}, {sec(10), kCapBps}, {sec(30), kLinkBps}};
    const int64_t duration = sec(60);
    const int64_t ninety = kLinkBps * 9 / 10;

    GoogCcController goog;
    const TraceResult g = run_trace(profile, goog, duration);
    CurrentController current(kCurrentProductionStartBps);
    const TraceResult c = run_trace(profile, current, duration);
    CurrentController current_low(kStartBps);
    const TraceResult cl = run_trace(profile, current_low, duration);

    struct Row {
        const char* name;
        const TraceResult* r;
    };
    for (const Row& row : {Row{"goog_cc", &g}, Row{"current(5.4)", &c}, Row{"current(1.5)", &cl}}) {
        const TraceResult& r = *row.r;
        std::printf("[goog-cc-spike] congestion %-12s: before cap %.2f Mbps, under cap after %s, queue peak %.1f ms, "
                    "mean queue 11-30 s %.1f ms, mean in cap %.2f Mbps, std 15-30 s %.0f kbps, loss in cap %.1f%%, "
                    "recover 90%% after %s, target 35/45/59 s %.2f/%.2f/%.2f Mbps\n",
                    row.name, mbps(target_at(r, sec(9.9))), fmt_time(time_to_fall(r, sec(10), kCapBps)).c_str(),
                    max_queue_delay_us(r, sec(10), sec(31)) / 1000.0, mean_queue_delay_ms(r, sec(11), sec(30)),
                    mbps(mean_target(r, sec(10), sec(30))), stddev_target(r, sec(15), sec(30)) / 1000.0,
                    100 * loss_fraction(r, sec(10), sec(30)), fmt_time(time_to_recover(r, sec(30), ninety)).c_str(),
                    mbps(target_at(r, sec(35))), mbps(target_at(r, sec(45))), mbps(target_at(r, sec(59))));
    }

    const double g_under = time_to_fall(g, sec(10), kCapBps);
    ASSERT_GE(g_under, 0.0) << "goog_cc never went under the cap";
    EXPECT_LT(g_under, 2.0) << "stage 4 gate: the bitrate reaches the cap within 2 s";
    EXPECT_LT(mean_target(g, sec(15), sec(30)), static_cast<double>(kCapBps));
    EXPECT_GT(mean_target(g, sec(15), sec(30)), 0.6 * kCapBps) << "goog_cc under-uses the cap";
    EXPECT_LT(mean_queue_delay_ms(g, sec(11), sec(30)), mean_queue_delay_ms(c, sec(11), sec(30)));
    EXPECT_LT(stddev_target(g, sec(15), sec(30)), stddev_target(c, sec(15), sec(30)));
    // Upstream defaults recover by AIMD alone (no probe outside ALR): about
    // 18 s from 2.4 to 5.4 Mbps on this trace. SweepFieldTrials has the
    // variants.
    const double g_recover = time_to_recover(g, sec(30), ninety);
    ASSERT_GE(g_recover, 0.0) << "goog_cc never recovered after the cap";
    EXPECT_LT(g_recover, 25.0);
}

// ---------------------------------------------------------------------------
// 3. Outage: 3 s of 100 % loss at 10 s on a 6 Mbps link.
// ---------------------------------------------------------------------------
TEST(GoogCcSpike, Outage3s) {
    LinkProfile profile;
    profile.outage_start_us = sec(10);
    profile.outage_end_us = sec(13);
    const int64_t duration = sec(40);
    const int64_t ninety = kLinkBps * 9 / 10;

    GoogCcController goog;
    const TraceResult g = run_trace(profile, goog, duration);
    CurrentController current(kCurrentProductionStartBps);
    const TraceResult c = run_trace(profile, current, duration);

    struct Row {
        const char* name;
        const TraceResult* r;
    };
    for (const Row& row : {Row{"goog_cc", &g}, Row{"current(5.4)", &c}}) {
        const TraceResult& r = *row.r;
        std::printf("[goog-cc-spike] outage %-12s: target 9.9 s %.2f, 11 s %.2f, 12.9 s %.2f, 13.2 s %.2f, 14 s %.2f, "
                    "min 13-16 s %.2f Mbps; encoder 12.9 s %.2f Mbps; sent into outage %.0f kB; back to 90%% after %s; "
                    "pacer window %s, max cwnd reduce %.2f\n",
                    row.name, mbps(target_at(r, sec(9.9))), mbps(target_at(r, sec(11))),
                    mbps(target_at(r, sec(12.9))), mbps(target_at(r, sec(13.2))), mbps(target_at(r, sec(14))),
                    mbps(min_target(r, sec(13), sec(16))), mbps(encoder_at(r, sec(12.9))),
                    bytes_sent(r, sec(10), sec(13)) / 1000.0, fmt_time(time_to_recover(r, sec(13), ninety)).c_str(),
                    r.ever_congestion_window ? "yes" : "no", r.max_cwnd_reduce_ratio);
    }

    // Upstream defaults: the first report after the outage carries about
    // 1800 lost packets and the estimate falls to about 0.4 Mbps. The climb
    // back takes about 19 s. SweepFieldTrials has the variants.
    const double g_back = time_to_recover(g, sec(13), ninety);
    ASSERT_GE(g_back, 0.0) << "goog_cc never came back after the outage";
    EXPECT_LT(g_back, 25.0);
    EXPECT_LT(bytes_sent(g, sec(10), sec(13)), bytes_sent(c, sec(10), sec(13)))
        << "goog_cc should send less into the outage than the current estimator";
}

// ---------------------------------------------------------------------------
// 4. Random loss 5 % without delay change.
// ---------------------------------------------------------------------------
TEST(GoogCcSpike, RandomLoss5Percent) {
    LinkProfile profile;
    profile.random_loss = 0.05;
    const int64_t duration = sec(60);

    GoogCcController goog;
    const TraceResult g = run_trace(profile, goog, duration);
    CurrentController current(kCurrentProductionStartBps);
    const TraceResult c = run_trace(profile, current, duration);
    CurrentController current_low(kStartBps);
    const TraceResult cl = run_trace(profile, current_low, duration);

    struct Row {
        const char* name;
        const TraceResult* r;
    };
    for (const Row& row : {Row{"goog_cc", &g}, Row{"current(5.4)", &c}, Row{"current(1.5)", &cl}}) {
        const TraceResult& r = *row.r;
        std::printf("[goog-cc-spike] loss5 %-12s: mean target 10-60 s %.2f Mbps, min %.2f Mbps, measured loss %.2f%%, "
                    "queue peak %.1f ms\n",
                    row.name, mbps(mean_target(r, sec(10), sec(60))), mbps(min_target(r, sec(10), sec(60))),
                    100 * loss_fraction(r, sec(10), sec(60)), max_queue_delay_us(r, sec(10), sec(60)) / 1000.0);
    }
    std::printf("[goog-cc-spike] loss5 current(5.4): smoothed loss %.3f, trend %.2f ms\n",
                current.estimator().loss_rate(), current.estimator().trend_ms());

    EXPECT_GT(mean_target(g, sec(10), sec(60)), 0.6 * kLinkBps)
        << "goog_cc loss-based estimator cut the rate on 5 % random loss";
}

// ---------------------------------------------------------------------------
// 5. Burst grouping under continuous pacing (gaps under 5 ms).
// ---------------------------------------------------------------------------
namespace {

struct GroupCount {
    double goog_groups_per_s = 0;
    double current_rule_groups_per_s = 0;
    uint64_t current_target_start = 0;
    uint64_t current_target_end = 0;
    double current_trend_end = 0;
};

// Sends a constant-rate stream (one packet every `gap_us`) over a clean
// 6 Mbps link and feeds the same TWCC feedback to both groupers.
GroupCount count_groups(int64_t gap_us, int64_t duration_us) {
    LinkProfile profile;
    Link link(profile);
    Receiver receiver;
    std::vector<int64_t> send_times;

    webrtc::InterArrivalDelta inter_arrival(TimeDelta::Millis(5));  // kSendTimeGroupLength
    int64_t goog_groups = 0;

    constexpr uint64_t kCurrentStart = 3'000'000;
    GccEstimator current(GccEstimator::Config{}, kCurrentStart);
    // Copy of the grouping rule of GccEstimator::on_packet (twcc.cpp:15 and
    // 524-533): a group closes when a received packet is sent more than 5 ms
    // after the LAST packet of the open group. GccEstimator has no group
    // counter, so the test counts with this copy and confirms through the
    // target, which moves on every closed group once ten have closed.
    int64_t rule_groups = 0;
    int64_t rule_last_send = -1;

    int64_t next_send = 0;
    int64_t next_report = kFeedbackIntervalUs;
    int64_t next_reported = 0;
    while (next_report <= duration_us) {
        while (next_send < next_report) {
            link.send(static_cast<int64_t>(send_times.size()), next_send, kPacketBytes);
            send_times.push_back(next_send);
            next_send += gap_us;
        }
        if (auto feedback = receiver.report(next_report, link)) {
            // The receiver reports one contiguous range per report.
            for (const auto& packet : feedback->packets) {
                const int64_t seq = next_reported++;
                const int64_t send_us = send_times[static_cast<size_t>(seq)];
                if (packet.received) {
                    TimeDelta send_delta = TimeDelta::Zero();
                    TimeDelta arrival_delta = TimeDelta::Zero();
                    int size_delta = 0;
                    if (inter_arrival.ComputeDeltas(to_ts(send_us), to_ts(packet.arrival_time_us),
                                                    to_ts(next_report + kOneWayDelayUs), kPacketBytes,
                                                    &send_delta, &arrival_delta, &size_delta)) {
                        ++goog_groups;
                    }
                    if (rule_last_send >= 0 && send_us - rule_last_send > 5'000) ++rule_groups;
                    rule_last_send = send_us;
                }
                current.on_packet(packet.sequence, packet.received, packet.received ? send_us : -1,
                                  packet.arrival_time_us);
            }
        }
        next_report += kFeedbackIntervalUs;
    }
    GroupCount out;
    const double seconds = static_cast<double>(duration_us) / 1e6;
    out.goog_groups_per_s = static_cast<double>(goog_groups) / seconds;
    out.current_rule_groups_per_s = static_cast<double>(rule_groups) / seconds;
    out.current_target_start = kCurrentStart;
    out.current_target_end = current.target_bps();
    out.current_trend_end = current.trend_ms();
    return out;
}

}  // namespace

TEST(GoogCcSpike, BurstGroupsCloseUnderContinuousPacing) {
    const int64_t duration = sec(20);
    // 4 Mbps of 1130-byte packets: one packet every 2.26 ms.
    const GroupCount paced = count_groups(2'260, duration);
    // 1.5 Mbps: one packet every 6.03 ms (gaps over 5 ms), as a control.
    const GroupCount sparse = count_groups(6'030, duration);
    std::printf("[goog-cc-spike] groups gap 2.26 ms: goog_cc %.1f /s, current rule %.1f /s, current target %.2f -> %.2f Mbps, trend %.2f ms\n",
                paced.goog_groups_per_s, paced.current_rule_groups_per_s,
                mbps(static_cast<int64_t>(paced.current_target_start)),
                mbps(static_cast<int64_t>(paced.current_target_end)), paced.current_trend_end);
    std::printf("[goog-cc-spike] groups gap 6.03 ms: goog_cc %.1f /s, current rule %.1f /s, current target %.2f -> %.2f Mbps\n",
                sparse.goog_groups_per_s, sparse.current_rule_groups_per_s,
                mbps(static_cast<int64_t>(sparse.current_target_start)),
                mbps(static_cast<int64_t>(sparse.current_target_end)));

    // goog_cc closes a group every 5 ms of send time: about 147 per second
    // with three packets per group.
    EXPECT_GT(paced.goog_groups_per_s, 100.0);
    // The current rule never closes a group under continuous pacing, and the
    // target does not move on a clean link with headroom.
    EXPECT_LT(paced.current_rule_groups_per_s, 1.0);
    EXPECT_EQ(paced.current_target_end, paced.current_target_start);
    // Control: with gaps over 5 ms both close a group per packet and the
    // current target moves.
    EXPECT_GT(sparse.current_rule_groups_per_s, 100.0);
    EXPECT_NE(sparse.current_target_end, sparse.current_target_start);
}

// ---------------------------------------------------------------------------
// Field trial sweep (input for the stage 4 decisions). Prints only.
// ---------------------------------------------------------------------------
TEST(GoogCcSpike, SweepFieldTrials) {
    struct Variant {
        const char* name;
        std::map<std::string, std::string> trials;
        bool alr_probing;
        double encoder_fill;
        int64_t jitter_us = 0;
        // When > 0: OnStreamsConfig(max_total_allocated_bitrate) at 31 s.
        int64_t allocation_at_31s_bps = 0;
    };
    const std::vector<Variant> variants = {
        {"defaults", {}, false, 1.0},
        {"loss-v2 off", {{"WebRTC-Bwe-LossBasedBweV2", "Enabled:false"}}, false, 1.0},
        {"defaults, encoder 50%", {}, false, 0.5},
        {"ALR probing, encoder 50%", {}, true, 0.5},
        {"ALR probing, loss-v2 off, enc 50%", {{"WebRTC-Bwe-LossBasedBweV2", "Enabled:false"}}, true, 0.5},
        {"ALR probing, encoder 50%, jitter 2 ms", {}, true, 0.5, 2'000},
        {"defaults, jitter 2 ms", {}, false, 1.0, 2'000},
        {"defaults, allocation 6 Mbps at 31 s", {}, false, 1.0, 0, kLinkBps},
        {"encoder 50%, allocation 6 Mbps at 31 s", {}, false, 0.5, 0, kLinkBps},
    };
    const int64_t ninety = kLinkBps * 9 / 10;

    for (const Variant& v : variants) {
        LinkProfile cap;
        cap.capacity = {{0, kLinkBps}, {sec(10), 2'500'000}, {sec(30), kLinkBps}};
        cap.jitter_us = v.jitter_us;
        LinkProfile outage;
        outage.outage_start_us = sec(10);
        outage.outage_end_us = sec(13);
        outage.jitter_us = v.jitter_us;
        LinkProfile loss;
        loss.random_loss = 0.05;
        loss.jitter_us = v.jitter_us;

        GoogCcController g1(v.trials, v.alr_probing);
        Simulation cap_sim(cap, g1, v.encoder_fill);
        if (v.allocation_at_31s_bps > 0) {
            const int64_t bps = v.allocation_at_31s_bps;
            cap_sim.set_hook(sec(31), [&g1, bps](int64_t now) { g1.set_max_allocated(now, bps); });
        }
        const TraceResult rc = cap_sim.run(sec(60));
        GoogCcController g2(v.trials, v.alr_probing);
        const TraceResult ro = run_trace(outage, g2, sec(40), v.encoder_fill);
        GoogCcController g3(v.trials, v.alr_probing);
        const TraceResult rl = run_trace(loss, g3, sec(60), v.encoder_fill);
        std::printf("[goog-cc-spike] sweep %-34s: startup 90%% %s | cap: under %s, mean %.2f Mbps, std %.0f kbps, "
                    "recover %s | outage: min after %.2f Mbps, back %s | loss5: mean %.2f Mbps\n",
                    v.name, fmt_time(time_to_reach(rc, 0, ninety)).c_str(),
                    fmt_time(time_to_fall(rc, sec(10), 2'500'000)).c_str(), mbps(mean_target(rc, sec(10), sec(30))),
                    stddev_target(rc, sec(15), sec(30)) / 1000.0, fmt_time(time_to_recover(rc, sec(30), ninety)).c_str(),
                    mbps(min_target(ro, sec(13), sec(16))), fmt_time(time_to_recover(ro, sec(13), ninety)).c_str(),
                    mbps(mean_target(rl, sec(10), sec(60))));
        int probes_after_start = 0;
        for (const auto& p : rc.packets) probes_after_start += (p.probe && p.send_us > sec(5)) ? 1 : 0;
        std::printf("[goog-cc-spike] sweep %-34s: cap trace probe packets after 5 s %d, target 40/50/59 s %.2f/%.2f/%.2f Mbps\n",
                    v.name, probes_after_start, mbps(target_at(rc, sec(40))), mbps(target_at(rc, sec(50))),
                    mbps(target_at(rc, sec(59))));
    }
}

// ---------------------------------------------------------------------------
// 6. Cost: CPU per call and heap per controller instance.
// ---------------------------------------------------------------------------
TEST(GoogCcSpike, CostPerController) {
    // Heap of one idle controller after start().
    const int64_t heap_before = g_live_heap_bytes.load();
    auto idle = std::make_unique<GoogCcController>();
    idle->start(0);
    const int64_t heap_idle = g_live_heap_bytes.load() - heap_before;
    idle.reset();

    // CPU and steady-state heap on the congestion trace.
    LinkProfile profile;
    profile.capacity = {{0, kLinkBps}, {sec(10), 2'500'000}, {sec(30), kLinkBps}};
    GoogCcController goog;
    run_trace(profile, goog, sec(60));
    const int64_t heap_with = g_live_heap_bytes.load();
    goog.reset_controller();
    const int64_t heap_running = heap_with - g_live_heap_bytes.load();

    // The same trace for 300 s: the heap must not grow with time.
    GoogCcController goog_long;
    run_trace(profile, goog_long, sec(300));
    const int64_t heap_long_with = g_live_heap_bytes.load();
    goog_long.reset_controller();
    const int64_t heap_long = heap_long_with - g_live_heap_bytes.load();

    CurrentController current(kCurrentProductionStartBps);
    run_trace(profile, current, sec(60));

    double mean_packets = 0;
    for (int64_t n : goog.feedback_packets) mean_packets += static_cast<double>(n);
    mean_packets /= std::max<size_t>(goog.feedback_packets.size(), 1);

    const Percentiles g_feedback = cpu_us(goog.feedback_ns);
    const Percentiles g_process = cpu_us(goog.process_ns);
    const Percentiles c_feedback = cpu_us(current.feedback_ns);
    const Percentiles g_sent = cpu_us(goog.sent_ns);
    std::printf("[goog-cc-spike] cost: goog_cc OnTransportPacketsFeedback mean %.2f us p99 %.2f us (%zu reports, %.1f packets each)\n",
                g_feedback.mean_us, g_feedback.p99_us, goog.feedback_ns.size(), mean_packets);
    std::printf("[goog-cc-spike] cost: goog_cc OnProcessInterval mean %.2f us p99 %.2f us (%zu calls)\n",
                g_process.mean_us, g_process.p99_us, goog.process_ns.size());
    std::printf("[goog-cc-spike] cost: goog_cc OnSentPacket mean %.2f us p99 %.2f us (%zu calls)\n",
                g_sent.mean_us, g_sent.p99_us, goog.sent_ns.size());
    std::printf("[goog-cc-spike] cost: current on_packet loop per report mean %.2f us p99 %.2f us\n",
                c_feedback.mean_us, c_feedback.p99_us);
    std::printf("[goog-cc-spike] cost: goog_cc heap idle %lld bytes, after 60 s trace %lld bytes, after 300 s %lld bytes; sizeof(GccEstimator) %zu bytes\n",
                static_cast<long long>(heap_idle), static_cast<long long>(heap_running),
                static_cast<long long>(heap_long), sizeof(GccEstimator));

    EXPECT_GT(heap_idle, 0);
    EXPECT_LT(heap_running, 1 << 20) << "goog_cc holds more than 1 MiB per instance";
    EXPECT_LT(heap_long, heap_running * 2) << "goog_cc heap grows with time";
#ifdef NDEBUG
    EXPECT_LT(g_feedback.mean_us, 500.0);
    EXPECT_LT(g_process.mean_us, 100.0);
#endif
}
