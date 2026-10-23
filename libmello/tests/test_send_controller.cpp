// GoogCcController (twcc.hpp) against a simulated link.
//
// A deterministic, single-threaded simulation of the stream host's send path
// as RtpVideoSender runs it: an encoder at the controller's encoder target
// (capped at the preset), a pacer at the controller's pacing rate with the
// 40 ms burst window, probe clusters first with media only (no RTX padding),
// a link with a capacity cap, a 100 ms tail-drop queue and 10 ms one way, and
// a receiver that sends real TWCC feedback every 50 ms: the bytes of
// TwccFeedbackGenerator, parsed back with parse_twcc_feedback.
//
// The scenarios follow the streaming-quality stage 4 gate: the start reaches
// the preset, a cap at 60 % is found, a 3 s outage recovers. Each prints its
// numbers with the prefix "[send-controller]".
//
// These tests need goog_cc (MELLO_WEBRTC_GOOG_CC); without it they skip.

#include <gtest/gtest.h>

#include "transport/twcc.hpp"

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <deque>
#include <functional>
#include <limits>
#include <optional>
#include <vector>

namespace {

using mello::transport::GoogCcController;
using mello::transport::SendProbeCluster;
using mello::transport::SentPacketInfo;
using mello::transport::TwccFeedback;
using mello::transport::TwccFeedbackGenerator;
using mello::transport::parse_twcc_feedback;

constexpr int64_t kPacketBytes = 1'150;
constexpr int64_t kOneWayUs = 10'000;
constexpr int64_t kFeedbackIntervalUs = 50'000;
constexpr int64_t kProcessIntervalUs = 25'000;
constexpr int64_t kFrameIntervalUs = 16'667;
constexpr int64_t kBurstWindowUs = 40'000;
constexpr int64_t kFeedbackTimeoutUs = 1'000'000;
constexpr int64_t kKeepaliveUs = 500'000;
// A large positive base, like the steady clock of a running machine.
constexpr int64_t kEpochUs = 3'600'000'000;

struct LinkProfile {
    // (start us, bps), sorted.
    std::vector<std::pair<int64_t, int64_t>> capacity{{0, 20'000'000}};
    int64_t outage_start_us = -1;
    int64_t outage_end_us = -1;
    int64_t queue_limit_us = 100'000;

    int64_t capacity_at(int64_t t) const {
        int64_t bps = capacity.front().second;
        for (const auto& [at, value] : capacity) {
            if (at <= t) {
                bps = value;
            }
        }
        return bps;
    }
    bool down(int64_t t) const {
        return outage_start_us >= 0 && t >= outage_start_us && t < outage_end_us;
    }
};

struct Sample {
    int64_t t_us = 0;
    uint64_t target_bps = 0;
    uint64_t encoder_bps = 0;
    uint64_t sent_bps = 0;  // over the last 500 ms
};

struct Result {
    std::vector<Sample> samples;
    uint64_t sent_packets = 0;
    uint64_t lost_packets = 0;
    uint64_t probe_clusters = 0;
    int64_t pauses = 0;
};

class Simulation {
public:
    Simulation(LinkProfile link, uint64_t preset_bps, uint64_t start_bps)
        : link_(std::move(link)), preset_bps_(preset_bps) {
        GoogCcController::Config config;
        config.start_bps = start_bps;
        config.min_bps = 300'000;
        // The manager ceiling: 1.08 x the preset (RTP overhead).
        config.max_bps = preset_bps * 108 / 100;
        controller_.emplace(config, kEpochUs);
        take_probes(0);
    }

    // An IDR every `every_us` between `from_us` and `to_us` (the viewer's
    // PLIs while it gates on loss).
    void set_idr_requests(int64_t from_us, int64_t to_us, int64_t every_us) {
        idr_from_us_ = from_us;
        idr_to_us_ = to_us;
        idr_every_us_ = every_us;
    }
    // The most the encoder makes (the rung's bitrate). Default: the preset.
    void set_encoder_cap(uint64_t bps) { encoder_cap_bps_ = bps; }
    // Each hook runs once, at the first process interval at or after its
    // time. Add them in time order.
    void add_hook(int64_t at_us, std::function<void(int64_t, GoogCcController&)> hook,
                  bool with_idr = false) {
        hooks_.emplace_back(at_us, std::move(hook));
        hook_idrs_.push_back(with_idr);
    }

    Result run(int64_t duration_us) {
        Result result;
        int64_t next_frame = 0;
        int64_t frame_index = 0;
        int64_t next_report = kFeedbackIntervalUs;
        int64_t next_process = kProcessIntervalUs;
        int64_t next_sample = 0;
        constexpr int64_t kNever = std::numeric_limits<int64_t>::max();
        int64_t now = 0;
        while (now < duration_us) {
            const int64_t next_feedback = in_transit_.empty() ? kNever : in_transit_.front().first;
            const int64_t next_send = pacer_ready(now);
            now = std::min({next_frame, next_report, next_process, next_feedback, next_send, next_sample});
            if (now >= duration_us) {
                break;
            }
            if (next_feedback == now) {
                deliver(now);
                continue;
            }
            if (next_process == now) {
                while (!hooks_.empty() && now >= hooks_.front().first) {
                    hooks_.front().second(kEpochUs + now, *controller_);
                    hooks_.pop_front();
                    idr_requested_ = idr_requested_ || hook_idrs_.front();
                    hook_idrs_.pop_front();
                    take_probes(now);
                }
                controller_->on_process_interval(kEpochUs + now, queued_bytes_);
                take_probes(now);
                check_timeout(now, result);
                next_process += kProcessIntervalUs;
                continue;
            }
            if (next_sample == now) {
                Sample sample;
                sample.t_us = now;
                sample.target_bps = controller_->target_bps();
                sample.encoder_bps = encoder_bps();
                while (!sent_log_.empty() && sent_log_.front().first < now - 500'000) {
                    sent_log_.pop_front();
                }
                uint64_t bytes = 0;
                for (const auto& [at, size] : sent_log_) {
                    bytes += static_cast<uint64_t>(size);
                }
                sample.sent_bps = bytes * 8 * 2;
                result.samples.push_back(sample);
                next_sample += 100'000;
                continue;
            }
            if (next_frame == now) {
                if (idr_every_us_ > 0 && now >= idr_from_us_ && now < idr_to_us_
                    && now >= next_idr_us_) {
                    idr_requested_ = true;
                    next_idr_us_ = now + idr_every_us_;
                }
                encode_frame(frame_index == 0 || idr_requested_);
                idr_requested_ = false;
                ++frame_index;
                next_frame = frame_index * kFrameIntervalUs;
                continue;
            }
            if (next_report == now) {
                report(now);
                next_report += kFeedbackIntervalUs;
                continue;
            }
            if (next_send == now) {
                send_one(now, result);
                continue;
            }
        }
        result.pauses = pauses_;
        return result;
    }

private:
    uint64_t encoder_bps() const {
        return std::min<uint64_t>(controller_->encoder_target_bps() * 100 / 108, encoder_cap_bps_ != 0 ? encoder_cap_bps_ : preset_bps_);
    }

    void take_probes(int64_t now) {
        for (const auto& cluster : controller_->take_probe_clusters()) {
            probes_.push_back({cluster, 0, 0, now, now, false});
        }
    }

    void encode_frame(bool idr) {
        if (paused_) {
            queued_bytes_ = std::min<int64_t>(queued_bytes_, kPacketBytes);
            return;
        }
        // A delta is one frame of the encoder rate. An IDR is set by the
        // picture more than by the rate: 8 frames at the preset (a 1080p60
        // IDR at 8 Mbps is about 130 kB).
        const double frame_bytes =
            static_cast<double>(encoder_bps()) / 8.0 / 60.0;
        const double idr_bytes = static_cast<double>(preset_bps_) / 8.0 / 60.0 * 8.0;
        carry_ += idr ? idr_bytes : frame_bytes;
        while (carry_ >= kPacketBytes) {
            carry_ -= kPacketBytes;
            queued_bytes_ += kPacketBytes;
        }
    }

    void drain(int64_t now) {
        const uint64_t rate = std::max<uint64_t>(controller_->pacing_bps(), 50'000);
        debt_ = std::max(0.0, debt_ - static_cast<double>(now - debt_at_) * static_cast<double>(rate) / 8e6);
        debt_at_ = now;
    }

    int64_t pacer_ready(int64_t now) {
        constexpr int64_t kNever = std::numeric_limits<int64_t>::max();
        if (paused_) {
            return std::max(next_keepalive_, now);
        }
        // The sender's rule: a cluster starts when the pacer holds its bytes.
        while (!probes_.empty()) {
            Probe& front = probes_.front();
            if (front.active) {
                if (now - front.created <= 1'000'000) {
                    break;
                }
            } else if (static_cast<uint64_t>(queued_bytes_) >= front.cluster.min_bytes) {
                front.active = true;
                front.created = now;
                front.next = now;
                break;
            } else if (now - front.created <= 3'000'000) {
                break;
            }
            probes_.pop_front();
        }
        if (queued_bytes_ < kPacketBytes) {
            return kNever;
        }
        if (!probes_.empty() && probes_.front().active) {
            return std::max(probes_.front().next, now);
        }
        drain(now);
        const uint64_t rate = std::max<uint64_t>(controller_->pacing_bps(), 50'000);
        const double burst = static_cast<double>(rate) * kBurstWindowUs / 8e6;
        if (debt_ <= burst) {
            return now;
        }
        return now + static_cast<int64_t>((debt_ - burst) * 8e6 / static_cast<double>(rate)) + 1;
    }

    void send_one(int64_t now, Result& result) {
        SentPacketInfo info;
        info.transport_sequence = static_cast<uint16_t>(sequence_);
        info.send_time_us = kEpochUs + now;
        info.rtp_bytes = kPacketBytes;
        if (paused_) {
            next_keepalive_ = now + kKeepaliveUs;
            queued_bytes_ = 0;
        } else {
            queued_bytes_ -= kPacketBytes;
            drain(now);
            debt_ += kPacketBytes;
            if (!probes_.empty() && probes_.front().active) {
                Probe& probe = probes_.front();
                info.probe_cluster_id = probe.cluster.id;
                info.probe_min_probes = probe.cluster.min_probes;
                info.probe_min_bytes = static_cast<int>(probe.cluster.min_bytes);
                probe.started += 1;
                probe.bytes += kPacketBytes;
                probe.next = std::max(probe.next, now) +
                    static_cast<int64_t>(kPacketBytes * 8'000'000 / std::max<uint64_t>(probe.cluster.rate_bps, 50'000));
                if (probe.started >= probe.cluster.min_probes
                    && static_cast<uint64_t>(probe.bytes) >= probe.cluster.min_bytes) {
                    probes_.pop_front();
                    ++result.probe_clusters;
                }
            }
        }
        if (first_unfed_ < 0) {
            first_unfed_ = now;
        }
        controller_->on_packet_sent(info);
        take_probes(now);
        sent_log_.emplace_back(now, kPacketBytes);
        ++result.sent_packets;

        // The link.
        if (link_.down(now)) {
            ++result.lost_packets;
            ++sequence_;
            return;
        }
        const int64_t start = std::max(now, link_free_);
        if (start - now > link_.queue_limit_us) {
            ++result.lost_packets;
            ++sequence_;
            return;
        }
        const int64_t bps = link_.capacity_at(now);
        link_free_ = start + (kPacketBytes * 8 * 1'000'000 + bps - 1) / bps;
        arrivals_.push_back({static_cast<uint16_t>(sequence_), link_free_ + kOneWayUs});
        ++sequence_;
    }

    void report(int64_t now) {
        while (!arrivals_.empty() && arrivals_.front().second <= now) {
            if (!link_.down(arrivals_.front().second)) {
                // The receiver clock: another machine, another base.
                generator_.on_packet(arrivals_.front().first,
                                     arrivals_.front().second + 123'456'789);
            }
            arrivals_.pop_front();
        }
        if (link_.down(now)) {
            return;
        }
        for (;;) {
            const auto bytes = generator_.build_feedback(1, 2);
            if (bytes.empty()) {
                break;
            }
            TwccFeedback feedback;
            if (parse_twcc_feedback(bytes.data(), bytes.size(), feedback)) {
                in_transit_.emplace_back(now + kOneWayUs, feedback);
            }
        }
    }

    void deliver(int64_t now) {
        TwccFeedback feedback = std::move(in_transit_.front().second);
        in_transit_.pop_front();
        first_unfed_ = -1;
        if (paused_) {
            // The sender's resume: route change, the report is dropped.
            paused_ = false;
            idr_requested_ = true;
            controller_->on_route_change(kEpochUs + now);
            take_probes(now);
            return;
        }
        controller_->on_feedback(feedback, kEpochUs + now);
        take_probes(now);
    }

    void check_timeout(int64_t now, Result&) {
        if (!paused_ && first_unfed_ >= 0 && now - first_unfed_ > kFeedbackTimeoutUs) {
            paused_ = true;
            ++pauses_;
            probes_.clear();
            next_keepalive_ = now;
        }
    }

    struct Probe {
        SendProbeCluster cluster;
        int started = 0;
        int64_t bytes = 0;
        int64_t created = 0;
        int64_t next = 0;
        bool active = false;
    };

    LinkProfile link_;
    uint64_t preset_bps_;
    std::optional<GoogCcController> controller_;
    uint64_t encoder_cap_bps_ = 0;
    std::deque<std::pair<int64_t, std::function<void(int64_t, GoogCcController&)>>> hooks_;
    std::deque<bool> hook_idrs_;
    std::deque<Probe> probes_;
    int64_t queued_bytes_ = 0;
    double carry_ = 0.0;
    double debt_ = 0.0;
    int64_t debt_at_ = 0;
    int64_t sequence_ = 0;
    int64_t link_free_ = 0;
    std::deque<std::pair<uint16_t, int64_t>> arrivals_;
    TwccFeedbackGenerator generator_;
    std::deque<std::pair<int64_t, TwccFeedback>> in_transit_;
    std::deque<std::pair<int64_t, int64_t>> sent_log_;
    int64_t first_unfed_ = -1;
    bool paused_ = false;
    bool idr_requested_ = false;
    int64_t idr_from_us_ = 0;
    int64_t idr_to_us_ = 0;
    int64_t idr_every_us_ = 0;
    int64_t next_idr_us_ = 0;
    int64_t next_keepalive_ = 0;
    int64_t pauses_ = 0;
};

// First time at or after `from` with the target at or above `bps`, in
// seconds after `from`; -1 when never.
double time_to_reach(const Result& r, int64_t from, uint64_t bps) {
    for (const auto& s : r.samples) {
        if (s.t_us >= from && s.target_bps >= bps) {
            return static_cast<double>(s.t_us - from) / 1e6;
        }
    }
    return -1.0;
}

// First time at or after `from` with the target at or under `bps`.
double time_to_fall(const Result& r, int64_t from, uint64_t bps) {
    for (const auto& s : r.samples) {
        if (s.t_us >= from && s.target_bps <= bps) {
            return static_cast<double>(s.t_us - from) / 1e6;
        }
    }
    return -1.0;
}

#define REQUIRE_GOOG_CC()                                       \
    if (!GoogCcController::available()) {                       \
        GTEST_SKIP() << "libmello built without goog_cc";       \
    }

constexpr uint64_t kPreset = 8'000'000;

// The stage 4 gate: "on clean the start reaches the preset bitrate within
// 3 s". The sender starts at 60 % of the preset; the initial probes run on
// the first IDR (media only, no RTX padding).
TEST(SendController, TheStartReachesThePresetWithin3Seconds) {
    REQUIRE_GOOG_CC();
    Simulation sim(LinkProfile{}, kPreset, kPreset * 6 / 10);
    const Result r = sim.run(6'000'000);
    const double reach = time_to_reach(r, 0, kPreset);
    std::printf("[send-controller] start: preset reached after %.2f s, %llu probe clusters, %llu of %llu lost\n",
                reach, static_cast<unsigned long long>(r.probe_clusters),
                static_cast<unsigned long long>(r.lost_packets),
                static_cast<unsigned long long>(r.sent_packets));
    EXPECT_GE(reach, 0.0);
    EXPECT_LE(reach, 3.0);
    EXPECT_GE(r.probe_clusters, 1u);
    EXPECT_EQ(r.lost_packets, 0u);
}

// The stage 4 gate: "on congested the bitrate reaches the cap within 2 s".
// The link drops to 60 % of the preset at 5 s for 20 s.
TEST(SendController, ACapAt60PercentIsReachedWithin2Seconds) {
    REQUIRE_GOOG_CC();
    LinkProfile link;
    const int64_t cap = static_cast<int64_t>(kPreset) * 6 / 10;
    link.capacity = {{0, 20'000'000}, {5'000'000, cap}, {25'000'000, 20'000'000}};
    Simulation sim(link, kPreset, kPreset);
    const Result r = sim.run(30'000'000);
    const double fall = time_to_fall(r, 5'000'000, static_cast<uint64_t>(cap));
    uint64_t sum = 0;
    int count = 0;
    for (const auto& s : r.samples) {
        if (s.t_us >= 10'000'000 && s.t_us < 25'000'000) {
            sum += s.sent_bps;
            ++count;
        }
    }
    const double mean_in_cap = count ? static_cast<double>(sum) / count : 0.0;
    std::printf("[send-controller] cap: target under the cap after %.2f s, mean send rate in the cap %.0f kbps (cap %lld), loss %.2f %%\n",
                fall, mean_in_cap / 1000.0, static_cast<long long>(cap / 1000),
                100.0 * static_cast<double>(r.lost_packets) / static_cast<double>(r.sent_packets));
    EXPECT_GE(fall, 0.0);
    EXPECT_LE(fall, 2.0);
    // It uses the cap: at least 70 % of it on average, not a collapse.
    EXPECT_GE(mean_in_cap, 0.7 * static_cast<double>(cap));
    EXPECT_LT(static_cast<double>(r.lost_packets), 0.05 * static_cast<double>(r.sent_packets));
}

// The gate's congested profile: a cap at 60 % with a 150 ms tail-drop queue,
// and an IDR every second in the cap (the viewer's PLIs while stage 3 is not
// in). The send rate must stay near the cap, not collapse.
TEST(SendController, TheGateCapWithIdrsKeepsTheSendRateNearTheCap) {
    REQUIRE_GOOG_CC();
    LinkProfile link;
    const int64_t cap = static_cast<int64_t>(kPreset) * 108 / 100 * 6 / 10;
    link.capacity = {{0, 20'000'000}, {5'000'000, cap}, {25'000'000, 20'000'000}};
    link.queue_limit_us = 150'000;
    Simulation sim(link, kPreset, kPreset);
    sim.set_idr_requests(5'000'000, 25'000'000, 1'000'000);
    const Result r = sim.run(30'000'000);
    uint64_t sum = 0;
    uint64_t minimum = UINT64_MAX;
    int count = 0;
    for (const auto& s : r.samples) {
        if (s.t_us >= 7'000'000 && s.t_us < 25'000'000) {
            sum += s.sent_bps;
            minimum = std::min(minimum, s.target_bps);
            ++count;
        }
    }
    const double mean = count ? static_cast<double>(sum) / count : 0.0;
    std::printf("[send-controller] gate cap with IDRs: mean send in the cap %.0f kbps of %lld, lowest target %llu kbps, loss %.2f %%\n",
                mean / 1000.0, static_cast<long long>(cap / 1000),
                static_cast<unsigned long long>(minimum / 1000),
                100.0 * static_cast<double>(r.lost_packets) / static_cast<double>(r.sent_packets));
    EXPECT_GE(mean, 0.7 * static_cast<double>(cap));
}

// The spike's outage design: media stops after 1 s without feedback, and on
// resume the route change restarts from the last target and drops the
// report of the outage. The target must not collapse and must be back
// within 1 s.
TEST(SendController, AFeedbackOutageRecoversWithinOneSecondOfTheLinkReturning) {
    REQUIRE_GOOG_CC();
    LinkProfile link;
    link.outage_start_us = 8'000'000;
    link.outage_end_us = 11'000'000;
    Simulation sim(link, kPreset, kPreset);
    const Result r = sim.run(16'000'000);
    uint64_t before = 0;
    uint64_t minimum_after = UINT64_MAX;
    for (const auto& s : r.samples) {
        if (s.t_us >= 7'000'000 && s.t_us < 8'000'000) {
            before = std::max(before, s.target_bps);
        }
        if (s.t_us >= 11'000'000) {
            minimum_after = std::min(minimum_after, s.target_bps);
        }
    }
    const double back = time_to_reach(r, 11'000'000, before * 9 / 10);
    if (::testing::GTEST_FLAG(also_run_disabled_tests)) {
        for (const auto& s : r.samples) {
            if (s.t_us >= 10'500'000 && s.t_us < 14'000'000) {
                std::printf("  t=%.1f target=%llu encoder=%llu sent=%llu\n", s.t_us / 1e6,
                            static_cast<unsigned long long>(s.target_bps / 1000),
                            static_cast<unsigned long long>(s.encoder_bps / 1000),
                            static_cast<unsigned long long>(s.sent_bps / 1000));
            }
        }
    }
    std::printf("[send-controller] outage: %lld pause(s), target before %llu kbps, minimum after %llu kbps, back to 90 %% after %.2f s\n",
                static_cast<long long>(r.pauses), static_cast<unsigned long long>(before / 1000),
                static_cast<unsigned long long>(minimum_after / 1000), back);
    EXPECT_EQ(r.pauses, 1);
    EXPECT_GE(back, 0.0);
    EXPECT_LE(back, 1.0);
    EXPECT_GE(minimum_after, before / 3);
}

// The cap release (spike option B). A cap of 2.5 Mbps holds for 5 s and the
// QualityController sits on a lower rung (encoder at most 2.5 Mbps,
// allocation 2.7 Mbps). The encoder stays on its rung after the cap, so the
// sender is application limited at about its target. At 20 s (15 s stable)
// the rung steps up: encoder cap 8 Mbps, an IDR (a rung change sends one)
// and the allocation raised to the preset. Option B alone (the raise) makes
// goog_cc probe only in ALR, and the sender is not in ALR here. The sender
// therefore re-probes on a raise (RtpVideoSender::set_max_allocated_bps): the
// route change of the outage resume, which restarts the estimators at the
// current target and runs the initial probes on the IDR.
struct StepUp {
    double time_to_preset = -1.0;
    uint64_t probe_clusters = 0;
    uint64_t max_target_after = 0;
    double loss_after = 0.0;  // share of the packets lost in the 5 s after
};

StepUp step_up(bool reprobe, bool link_released) {
    LinkProfile link;
    link.capacity = {{0, 2'500'000}};
    if (link_released) {
        link.capacity.push_back({5'000'000, 20'000'000});
    }
    Simulation sim(link, kPreset, 2'500'000);
    Simulation* simulation = &sim;
    sim.set_encoder_cap(2'500'000);
    sim.add_hook(0, [](int64_t now, GoogCcController& controller) {
        controller.set_max_total_allocated(now, 2'700'000);
    });
    sim.add_hook(20'000'000, [reprobe, simulation](int64_t now, GoogCcController& controller) {
        simulation->set_encoder_cap(kPreset);
        controller.set_max_total_allocated(now, kPreset * 108 / 100);
        if (reprobe) {
            controller.on_route_change(now);
        }
    }, true);
    const Result r = sim.run(40'000'000);
    StepUp out;
    out.time_to_preset = time_to_reach(r, 20'000'000, kPreset * 9 / 10);
    out.probe_clusters = r.probe_clusters;
    for (const auto& sample : r.samples) {
        if (sample.t_us >= 21'000'000) {
            out.max_target_after = std::max(out.max_target_after, sample.target_bps);
        }
    }
    out.loss_after = r.sent_packets
        ? static_cast<double>(r.lost_packets) / static_cast<double>(r.sent_packets)
        : 0.0;
    return out;
}

TEST(SendController, ARungStepUpWithAReprobeFindsTheReleasedLink) {
    REQUIRE_GOOG_CC();
    const StepUp reprobe = step_up(true, true);
    const StepUp raise_only = step_up(false, true);
    std::printf("[send-controller] rung step up after a cap, time to 90 %% of the preset: "
                "raise + re-probe %.2f s (%llu probe clusters), raise only %.2f s (%llu)\n",
                reprobe.time_to_preset, static_cast<unsigned long long>(reprobe.probe_clusters),
                raise_only.time_to_preset, static_cast<unsigned long long>(raise_only.probe_clusters));
    EXPECT_GE(reprobe.time_to_preset, 0.0);
    EXPECT_LE(reprobe.time_to_preset, 1.0);
    EXPECT_GT(reprobe.probe_clusters, raise_only.probe_clusters);
}

// The same step up on a link that is still capped: the re-probe must not
// push the target over the cap for more than a moment.
TEST(SendController, ARungStepUpWithAReprobeOnACappedLinkStaysAtTheCap) {
    REQUIRE_GOOG_CC();
    const StepUp capped = step_up(true, false);
    std::printf("[send-controller] rung step up on a capped link (2.5 Mbps): highest target 1 s after %llu kbps, loss %.2f %%\n",
                static_cast<unsigned long long>(capped.max_target_after / 1000), 100.0 * capped.loss_after);
    EXPECT_LE(capped.max_target_after, 2'500'000u * 13 / 10);
    EXPECT_LT(capped.loss_after, 0.05);
}

// A cap release with a full encoder: goog_cc probes on an allocation change
// only in ALR, so the raise sends no probe and the target climbs by its own
// increase. The time is printed for the stage 4 report; the test checks
// only that the target gets back to the preset.
TEST(SendController, AFullEncoderClimbsBackAfterACapRelease) {
    REQUIRE_GOOG_CC();
    LinkProfile link;
    const int64_t cap = static_cast<int64_t>(kPreset) * 6 / 10;
    link.capacity = {{0, 20'000'000}, {5'000'000, cap}, {25'000'000, 20'000'000}};
    Simulation sim(link, kPreset, kPreset);
    sim.add_hook(25'000'000, [](int64_t now, GoogCcController& controller) {
        controller.set_max_total_allocated(now, kPreset * 108 / 100);
    }, true);
    const Result r = sim.run(60'000'000);
    const double back = time_to_reach(r, 25'000'000, kPreset * 9 / 10);
    std::printf("[send-controller] cap release, full encoder: 90 %% of the preset after %.2f s, %llu probe clusters\n",
                back, static_cast<unsigned long long>(r.probe_clusters));
    EXPECT_GE(back, 0.0);
}

// Arrival times come from the 24-bit reference time, which wraps every
// 12.4 days of receiver uptime. Reports across the wrap must stay usable:
// no false overuse, the target holds.
TEST(SendController, ArrivalTimesAcrossTheReferenceWrapKeepTheTarget) {
    REQUIRE_GOOG_CC();
    GoogCcController::Config config;
    config.start_bps = 4'000'000;
    config.max_bps = 4'000'000;
    GoogCcController controller(config, kEpochUs);
    TwccFeedbackGenerator generator;
    // Receiver clock 2 s before the wrap of the reference time.
    const int64_t wrap_us = (int64_t{1} << 24) * 64'000;
    const int64_t receiver_base = wrap_us - 2'000'000;
    uint16_t sequence = 0;
    for (int64_t t = 0; t < 5'000'000; t += 2'500) {
        SentPacketInfo info;
        info.transport_sequence = sequence;
        info.send_time_us = kEpochUs + t;
        info.rtp_bytes = 1'150;
        controller.on_packet_sent(info);
        generator.on_packet(sequence, receiver_base + t + 10'000);
        ++sequence;
        if (t % 50'000 == 0) {
            for (;;) {
                const auto bytes = generator.build_feedback(1, 2);
                if (bytes.empty()) {
                    break;
                }
                TwccFeedback feedback;
                ASSERT_TRUE(parse_twcc_feedback(bytes.data(), bytes.size(), feedback));
                controller.on_feedback(feedback, kEpochUs + t + 20'000);
            }
        }
        if (t % 25'000 == 0) {
            controller.on_process_interval(kEpochUs + t, 0);
        }
    }
    std::printf("[send-controller] wrap: target %llu kbps\n",
                static_cast<unsigned long long>(controller.target_bps() / 1000));
    EXPECT_GE(controller.target_bps(), 3'000'000u);
}

} // namespace
