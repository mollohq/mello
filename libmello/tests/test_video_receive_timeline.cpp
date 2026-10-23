// The viewer receive timeline (streaming-quality stage 3), alone and behind
// the H.264 receiver. Simulated clock, fixed seeds, no network, no decoder.
//
// The "behind the receiver" cases are the product's receive path without
// libdatachannel: RTP packets of a 60 fps stream, some lost, the NACK answered
// by a simulated SFU one RTT later, into RtpH264Receiver, whose access units
// go into the timeline, which a simulated decode thread polls. They prove the
// stage 3 gate items at unit level: a single loss costs no key frame, a
// burst that cannot be repaired costs exactly one, and the target delay
// rises in a jitter spike and returns within 5 s.

#include <gtest/gtest.h>

#include "transport/rtp_h264_receiver.hpp"
#include "transport/video_receive_timeline.hpp"

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <functional>
#include <map>
#include <memory>
#include <optional>
#include <random>
#include <vector>

using mello::transport::RtpH264Receiver;
using mello::transport::TimelineDecision;
using mello::transport::TimelineInput;
using mello::transport::VideoReceiveTimeline;

namespace {

constexpr int64_t kFrameUs = 16'667;
constexpr uint32_t kRtpPerFrame = 1500;

struct SimClock {
    int64_t now = 10'000'000;
    VideoReceiveTimeline::ClockFn fn() {
        return [this] { return now; };
    }
};

TimelineInput frame(int64_t id, int64_t reference, uint32_t rtp, int64_t arrival_us, bool key,
                    size_t bytes = 8000) {
    TimelineInput in;
    in.id = id;
    in.reference = key ? -1 : reference;
    in.rtp_timestamp = rtp;
    in.arrival_us = arrival_us;
    in.keyframe = key;
    in.annex_b.assign(bytes, 0x42);
    return in;
}

struct Released {
    int64_t id;
    int64_t render_us;
    int64_t released_us;
};

// Drive the timeline's decode side until `until_us`: poll, decode in
// `decode_us`, wait as told. Returns what it released and the key-frame
// requests.
struct DecodeLoop {
    VideoReceiveTimeline& tl;
    SimClock& clock;
    int64_t decode_us = 3'000;
    int64_t next_poll = 0;
    std::vector<Released> released;
    int keyframe_requests = 0;

    void run_until(int64_t until_us) {
        while (clock.now < until_us) {
            if (clock.now < next_poll) {
                clock.now = std::min(next_poll, until_us);
                continue;
            }
            TimelineDecision d = tl.poll();
            switch (d.kind) {
            case TimelineDecision::Kind::Release:
                released.push_back({d.frame.id, d.frame.render_time_us, clock.now});
                tl.on_decoded(decode_us);
                next_poll = clock.now;
                break;
            case TimelineDecision::Kind::KeyframeNeeded:
                ++keyframe_requests;
                next_poll = clock.now;
                break;
            case TimelineDecision::Kind::Wait:
                next_poll = std::max(clock.now + 1, std::min(d.wake_us, until_us));
                break;
            case TimelineDecision::Kind::Idle:
                next_poll = until_us;
                break;
            }
        }
    }
};

TEST(VideoReceiveTimeline, ACleanStreamIsReleasedOnTheRtpClock) {
    SimClock clock;
    VideoReceiveTimeline tl({}, clock.fn());
    DecodeLoop loop{tl, clock};
    int64_t last_id = -1;
    for (int i = 0; i < 600; ++i) {
        const int64_t send = clock.now;
        const int64_t id = i * 5 + 4;  // 5 packets per frame
        tl.insert(frame(id, last_id, 90000 + i * kRtpPerFrame, send + 20'000, i == 0));
        last_id = id;
        loop.run_until(send + kFrameUs);
    }
    loop.run_until(clock.now + 200'000);
    ASSERT_EQ(loop.released.size(), 600u);
    std::vector<double> intervals;
    for (size_t i = 1; i < loop.released.size(); ++i) {
        EXPECT_GT(loop.released[i].id, loop.released[i - 1].id);
        intervals.push_back((loop.released[i].render_us - loop.released[i - 1].render_us) / 1000.0);
    }
    std::sort(intervals.begin(), intervals.end());
    EXPECT_LT(intervals[intervals.size() * 95 / 100], 18.0) << "render times keep the 60 fps cadence";
    const auto s = tl.stats();
    EXPECT_GE(s.target_delay_ms, 1.0);
    EXPECT_LE(s.target_delay_ms, 150.0);
    EXPECT_EQ(s.keyframe_requests, 0u);
    EXPECT_EQ(loop.keyframe_requests, 0);
    EXPECT_FALSE(s.keyframe_required);
}

TEST(VideoReceiveTimeline, TheStartWaitsForAKeyFrameAndAsksOnlyOncePerRetry) {
    SimClock clock;
    VideoReceiveTimeline tl({}, clock.fn());
    DecodeLoop loop{tl, clock};
    // Delta frames only (a viewer that joined between key frames).
    int64_t last = 99;
    for (int i = 0; i < 150; ++i) {  // 2.5 s
        tl.insert(frame(100 + i, last, i * kRtpPerFrame, clock.now + 10'000, false));
        last = 100 + i;
        loop.run_until(clock.now + kFrameUs);
    }
    EXPECT_TRUE(loop.released.empty());
    // First request 50 ms after the first frame, then one per second.
    EXPECT_EQ(loop.keyframe_requests, 3);
    // The key frame starts the picture.
    tl.insert(frame(1000, -1, 150 * kRtpPerFrame, clock.now, true));
    loop.run_until(clock.now + 50'000);
    ASSERT_EQ(loop.released.size(), 1u);
    EXPECT_EQ(loop.released[0].id, 1000);
}

TEST(VideoReceiveTimeline, ALossAsksForOneKeyFrameAndTheDecodableFramesStillPlay) {
    SimClock clock;
    VideoReceiveTimeline tl({}, clock.fn());
    DecodeLoop loop{tl, clock};
    tl.insert(frame(0, -1, 0, clock.now, true));
    for (int i = 1; i < 10; ++i) tl.insert(frame(i, i - 1, i * kRtpPerFrame, clock.now, false));
    // Frame 10 never comes; 11.. reference it.
    for (int i = 11; i < 20; ++i) tl.insert(frame(i, i - 1, i * kRtpPerFrame, clock.now, false));
    tl.on_loss();
    loop.run_until(clock.now + 500'000);
    EXPECT_EQ(loop.keyframe_requests, 1) << "one loss, one request, nothing more in 0.5 s";
    EXPECT_EQ(loop.released.size(), 10u) << "frames 0..9 still play";
    tl.insert(frame(30, -1, 30 * kRtpPerFrame, clock.now, true));
    loop.run_until(clock.now + 100'000);
    ASSERT_EQ(loop.released.size(), 11u);
    EXPECT_EQ(loop.released.back().id, 30);
    EXPECT_FALSE(tl.keyframe_required());
}

TEST(VideoReceiveTimeline, ADecodeErrorDropsToTheNextKeyFrame) {
    SimClock clock;
    VideoReceiveTimeline tl({}, clock.fn());
    DecodeLoop loop{tl, clock};
    tl.insert(frame(0, -1, 0, clock.now, true));
    loop.run_until(clock.now + 50'000);
    ASSERT_EQ(loop.released.size(), 1u);
    tl.on_decode_error();
    for (int i = 1; i < 5; ++i) tl.insert(frame(i, i - 1, i * kRtpPerFrame, clock.now, false));
    loop.run_until(clock.now + 100'000);
    EXPECT_EQ(loop.keyframe_requests, 1);
    EXPECT_EQ(loop.released.size(), 1u) << "no delta frame on a broken reference";
    EXPECT_EQ(tl.stats().frames_dropped, 4u);
}

TEST(VideoReceiveTimeline, TheDelayStaysUnderTheCapInHeavyJitter) {
    SimClock clock;
    VideoReceiveTimeline tl({}, clock.fn());
    DecodeLoop loop{tl, clock};
    std::mt19937 rng(7);
    std::uniform_int_distribution<int> jitter(0, 400'000);
    int64_t last = -1;
    std::map<int64_t, TimelineInput> pending;
    const int64_t start = clock.now;
    for (int i = 0; i < 600; ++i) {
        const int64_t arrival = start + i * kFrameUs + jitter(rng);
        pending.emplace(arrival * 1000 + i, frame(i, last, i * kRtpPerFrame, arrival, i == 0));
        last = i;
    }
    for (auto& [key, in] : pending) {
        const int64_t arrival = in.arrival_us;
        if (arrival > clock.now) loop.run_until(arrival);
        tl.insert(std::move(in));
    }
    loop.run_until(clock.now + 1'000'000);
    EXPECT_LE(tl.stats().target_delay_ms, 150.5);
    EXPECT_GT(tl.stats().target_delay_ms, 50.0);
}

// The minimum delay (stage 7's A/V sync input) survives a timing reset.
TEST(VideoReceiveTimeline, TheMinimumDelayIsAppliedAgainAfterATimingReset) {
    SimClock clock;
    VideoReceiveTimeline tl({}, clock.fn());
    DecodeLoop loop{tl, clock};
    tl.set_min_delay_us(80'000);
    tl.insert(frame(0, -1, 0, clock.now, true));
    loop.run_until(clock.now + 200'000);
    // A repaired key frame whose RTP time is 30 s ahead (it does not move
    // the clock estimate) has a bad render time: the timing resets.
    // A key frame after a decode error is released at once, whatever its
    // render time says.
    tl.on_decode_error();
    TimelineInput late = frame(100, -1, 30u * 90000u, clock.now, true);
    late.retransmitted = true;
    tl.insert(std::move(late));
    loop.run_until(clock.now + 200'000);
    EXPECT_GE(tl.stats().timing_resets, 1u);
    EXPECT_NEAR(tl.stats().target_delay_ms, 80.0, 0.5);
}

// ---------------------------------------------------------------------------
// Behind the H.264 receiver.
// ---------------------------------------------------------------------------

std::vector<uint8_t> rtp(uint16_t seq, uint32_t ts, bool marker, std::vector<uint8_t> payload) {
    std::vector<uint8_t> p(12, 0);
    p[0] = 0x80;
    p[1] = static_cast<uint8_t>((marker ? 0x80 : 0) | 96);
    p[2] = static_cast<uint8_t>(seq >> 8);
    p[3] = static_cast<uint8_t>(seq);
    p[4] = static_cast<uint8_t>(ts >> 24);
    p[5] = static_cast<uint8_t>(ts >> 16);
    p[6] = static_cast<uint8_t>(ts >> 8);
    p[7] = static_cast<uint8_t>(ts);
    p.insert(p.end(), payload.begin(), payload.end());
    return p;
}

// One frame as packets: a key frame is SPS/PPS in a STAP-A then FU-A
// fragments of the IDR; a delta frame is FU-A fragments of a slice.
std::vector<std::vector<uint8_t>> frame_packets(uint16_t& seq, uint32_t ts, bool key, int fragments) {
    std::vector<std::vector<uint8_t>> out;
    if (key) {
        out.push_back(rtp(seq++, ts, false, {0x78, 0x00, 0x02, 0x67, 0x42, 0x00, 0x02, 0x68, 0xce}));
    }
    const uint8_t type = key ? 0x05 : 0x01;
    for (int f = 0; f < fragments; ++f) {
        uint8_t header = type;
        if (f == 0) header |= 0x80;
        if (f == fragments - 1) header |= 0x40;
        std::vector<uint8_t> payload{0x7c, header, static_cast<uint8_t>(f == 0 ? 0x88 : 0x11)};
        payload.resize(1100, 0x22);
        out.push_back(rtp(seq++, ts, f == fragments - 1, payload));
    }
    return out;
}

struct LinkSpec {
    double loss = 0.0;            // per packet, independent
    int burst_at_frame = -1;      // drop this many whole frames' packets from here
    int burst_frames = 0;
    bool repair_burst = true;     // the SFU still has the burst's packets
    int64_t one_way_us = 10'000;  // SFU to viewer
    std::function<int64_t(int frame)> jitter_us;
    uint32_t seed = 1;
};

struct PathResult {
    int keyframe_requests = 0;
    int losses = 0;
    size_t released = 0;
    size_t nacked = 0;
    uint64_t late = 0;
    std::vector<std::pair<int64_t, double>> target_ms;  // (sim us, ms)
};

// A 60 fps stream, 12-packet deltas and a key frame every 10 s, through a
// lossy link into the receiver and the timeline. A NACKed packet arrives
// again one RTT (2 x one way) after the NACK, unless the link loses it too.
PathResult run_path(const LinkSpec& link, int frames) {
    SimClock clock;
    VideoReceiveTimeline tl({}, clock.fn());
    auto tp = [](int64_t us) { return RtpH264Receiver::TimePoint{} + std::chrono::microseconds(us); };

    std::multimap<int64_t, std::vector<uint8_t>> in_flight;  // arrival -> packet
    std::map<uint16_t, std::vector<uint8_t>> sent;           // SFU retransmit buffer
    std::mt19937 rng(link.seed);
    std::uniform_real_distribution<double> uni(0.0, 1.0);
    PathResult result;

    RtpH264Receiver::Callbacks callbacks;
    callbacks.on_access_unit = [&](RtpH264Receiver::AccessUnit&& unit) {
        TimelineInput in;
        in.id = unit.last_sequence;
        in.reference = unit.reference;
        in.rtp_timestamp = unit.rtp_timestamp;
        in.arrival_us = std::chrono::duration_cast<std::chrono::microseconds>(
                            unit.last_arrival.time_since_epoch()).count();
        in.keyframe = unit.keyframe;
        in.retransmitted = unit.retransmitted;
        in.annex_b = std::move(unit.annex_b);
        tl.insert(std::move(in));
    };
    callbacks.on_nack = [&](const std::vector<uint16_t>& seqs) {
        for (uint16_t s : seqs) {
            ++result.nacked;
            auto it = sent.find(s);
            if (it == sent.end()) continue;
            if (uni(rng) < link.loss) continue;  // the repair is lost too
            in_flight.emplace(clock.now + 2 * link.one_way_us, it->second);
        }
    };
    callbacks.on_loss = [&](int64_t) {
        ++result.losses;
        tl.on_loss();
    };
    callbacks.render_time = [&](uint32_t ts) -> std::optional<RtpH264Receiver::TimePoint> {
        auto r = tl.render_time_us(ts);
        if (!r) return std::nullopt;
        return tp(*r);
    };
    RtpH264Receiver::Config config;
    config.rtt = std::chrono::microseconds(2 * link.one_way_us);
    RtpH264Receiver receiver(std::move(callbacks), config);
    tl.set_rtt_us(2 * link.one_way_us);
    int64_t timeline_rtt_us = 2 * link.one_way_us;
    DecodeLoop loop{tl, clock};

    uint16_t seq = 1000;
    const int64_t start = clock.now;
    int answered = 0;
    for (int f = 0; f < frames; ++f) {
        const int64_t send = start + f * kFrameUs;
        // The host answers a key-frame request with the next frame.
        const bool key = f % 600 == 0 || loop.keyframe_requests > answered;
        answered = loop.keyframe_requests;
        const int64_t extra = link.jitter_us ? link.jitter_us(f) : 0;
        const bool burst = link.burst_at_frame >= 0 && f >= link.burst_at_frame &&
                           f < link.burst_at_frame + link.burst_frames;
        for (auto& p : frame_packets(seq, static_cast<uint32_t>(f) * kRtpPerFrame, key, key ? 60 : 12)) {
            const uint16_t s = static_cast<uint16_t>((p[2] << 8) | p[3]);
            if (!burst || link.repair_burst) sent[s] = p;
            if (burst || uni(rng) < link.loss) continue;
            in_flight.emplace(send + link.one_way_us + extra, p);
        }
        // Deliver and tick in 1 ms steps until the next frame is sent.
        const int64_t next = send + kFrameUs;
        while (clock.now < next) {
            const int64_t step_end = std::min(next, clock.now + 1'000);
            while (!in_flight.empty() && in_flight.begin()->first <= step_end) {
                auto node = in_flight.extract(in_flight.begin());
                if (node.key() > clock.now) loop.run_until(node.key());
                receiver.on_rtp_packet(node.mapped().data(), node.mapped().size(), tp(clock.now));
                // An insert wakes the decode thread (VideoReceiveTimeline::wait).
                loop.next_poll = std::min(loop.next_poll, clock.now);
            }
            loop.run_until(step_end);
            receiver.tick(tp(clock.now));
            // As the receiver session does: the timeline's RTT is the
            // measured repair time when that is longer than the ping.
            const int64_t want = std::max<int64_t>(2 * link.one_way_us, receiver.repair_latency().count());
            if (std::abs(want - timeline_rtt_us) > 2'000) {
                timeline_rtt_us = want;
                tl.set_rtt_us(want);
            }
        }
        if (f % 6 == 0) result.target_ms.push_back({clock.now - start, tl.stats().target_delay_ms});
    }
    loop.run_until(clock.now + 500'000);
    result.keyframe_requests = loop.keyframe_requests;
    result.released = loop.released.size();
    result.late = tl.stats().frames_late;
    return result;
}

// Stage 3 gate item at unit level: home-wifi has 2 % single losses; every one
// is repaired inside the target delay, and no key frame is asked for. The
// receiver before stage 3 asked for one after 2 NACKs or a full 3-AU window.
TEST(ReceivePath, SingleLossesAreRepairedWithoutAKeyFrame) {
    LinkSpec link;
    link.loss = 0.02;
    link.seed = 11;
    const PathResult r = run_path(link, 1200);  // 20 s
    EXPECT_GT(r.nacked, 100u);
    EXPECT_EQ(r.losses, 0);
    // One request at the start: the first key frame is the first frame.
    EXPECT_EQ(r.keyframe_requests, 0);
    EXPECT_GE(r.released, 1190u);
}

// While the link needs repairs, the target delay keeps room for one. A
// repaired frame comes one RTT (80 ms here) after its original time; with a
// delay under the RTT it and the frames behind it are late. Upstream adds
// the RTT only after 3 NACKed frames and forgets it at each estimator
// reset, which the staggered pair does every 2 s.
TEST(ReceivePath, TheDelayKeepsRoomForARepairWhileTheLinkNeedsThem) {
    LinkSpec link;
    link.loss = 0.005;
    link.one_way_us = 40'000;
    link.seed = 3;
    const PathResult r = run_path(link, 1200);  // 20 s
    EXPECT_EQ(r.losses, 0);
    EXPECT_EQ(r.keyframe_requests, 0);
    double low = 1e9;
    for (const auto& [t, ms] : r.target_ms) {
        if (t > 5'000'000) low = std::min(low, ms);
    }
    EXPECT_GE(low, 80.0) << "the delay stays at the RTT or more after the first repair";
    EXPECT_LE(r.late, 12u);
}

// A burst the SFU can still resend is repaired: no key frame.
TEST(ReceivePath, ARepairableBurstCostsNoKeyFrame) {
    LinkSpec link;
    link.burst_at_frame = 300;
    link.burst_frames = 3;
    link.repair_burst = true;
    const PathResult r = run_path(link, 420);
    EXPECT_EQ(r.losses, 0);
    EXPECT_EQ(r.keyframe_requests, 0);
    EXPECT_GE(r.released, 415u);
}

// A burst that outlives the repair deadline (the SFU has nothing to resend)
// is one loss and one key-frame request, which the host answers: not one
// request per frame, and not 18 of them (the receiver before stage 3).
TEST(ReceivePath, AnUnrepairableBurstIsOneKeyFrameRequest) {
    LinkSpec link;
    link.burst_at_frame = 300;
    link.burst_frames = 3;
    link.repair_burst = false;
    const PathResult r = run_path(link, 420);
    EXPECT_EQ(r.losses, 1);
    EXPECT_EQ(r.keyframe_requests, 1);
    // The loss is declared after the 300 ms repair window (18 frames); the
    // key frame that answers it is the frame after that.
    EXPECT_GE(r.released, 420u - 3u - 18u - 2u) << "the picture comes back with the answering key frame";
}

// The target delay rises during a 3 s jitter spike and is back within 5 s
// after it (the staggered estimator pair; upstream alone does not rise).
TEST(ReceivePath, TheTargetDelayReturnsWithinFiveSecondsAfterAJitterSpike) {
    LinkSpec link;
    std::mt19937 rng(5);
    std::normal_distribution<double> g(0.0, 1.0);
    std::vector<int64_t> jitter(1800, 0);
    for (int f = 600; f < 780; ++f) jitter[f] = static_cast<int64_t>(std::abs(g(rng)) * 30'000);
    // Arrivals stay in order on one path.
    for (int f = 1; f < 1800; ++f) {
        const int64_t prev_arrival = (f - 1) * kFrameUs + jitter[f - 1];
        const int64_t arrival = f * kFrameUs + jitter[f];
        if (arrival < prev_arrival) jitter[f] = prev_arrival - f * kFrameUs;
    }
    link.jitter_us = [&jitter](int f) { return jitter[static_cast<size_t>(f)]; };
    const PathResult r = run_path(link, 1800);  // 30 s, spike from 10 s to 13 s

    auto value_at = [&](double t_s) {
        double v = 0.0;
        for (const auto& [t, ms] : r.target_ms) {
            if (t / 1e6 <= t_s) v = ms;
        }
        return v;
    };
    double clean = value_at(9.5);
    double peak = 0.0;
    for (const auto& [t, ms] : r.target_ms) {
        if (t >= 10'000'000 && t <= 13'000'000) peak = std::max(peak, ms);
    }
    double back_at = -1.0;
    for (const auto& [t, ms] : r.target_ms) {
        if (t > 13'000'000 && ms <= clean + 10.0) {
            back_at = t / 1e6 - 13.0;
            break;
        }
    }
    EXPECT_GT(peak, clean + 10.0) << "the delay rises in the spike";
    EXPECT_GE(back_at, 0.0) << "the delay never returned";
    EXPECT_LT(back_at, 5.0) << "the delay returns within 5 s";
    EXPECT_EQ(r.keyframe_requests, 0);
}

} // namespace
