// Stage 3 spike: WebRTC video receive timing (M131) as the viewer jitter
// buffer. This suite drives the vendored FrameBuffer, VCMTiming,
// JitterEstimator and FrameDecodeTiming with synthetic 60 fps H.264 access
// units: RTP timestamp at 90 kHz, arrival time from a simulated link, size and
// key frame flag. Seeds are fixed. Nothing here touches the receiver session,
// the decoders or the present path.
//
// SpikeTimeline below is the prototype of the stage 3 class. It does
// synchronously what upstream's VideoStreamBufferController does on its task
// queue: insert a complete access unit, and answer "which frame to decode
// now, or when to ask again". The rule for a frame that does not arrive in
// time ("past the render time plus one RTT") is mello's: upstream has only a
// 3 s no-frame timeout.
//
// Each scenario prints its numbers with the prefix "[video-timing-spike]"
// and fails when the timing misbehaves. Run one scenario alone with
//   mello_video_timing_spike_tests --gtest_filter=VideoTimingSpike.<Name>
//
// Thread model: single thread. The simulation inserts frames and polls the
// timeline from the test thread in simulated-time order.

#include <gtest/gtest.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <functional>
#include <map>
#include <memory>
#include <new>
#include <optional>
#include <random>
#include <string>
#include <utility>
#include <vector>

#include "absl/container/inlined_vector.h"
#include "api/field_trials_view.h"
#include "api/make_ref_counted.h"
#include "api/units/data_size.h"
#include "api/units/time_delta.h"
#include "api/units/timestamp.h"
#include "api/video/encoded_frame.h"
#include "api/video/encoded_image.h"
#include "api/video/frame_buffer.h"
#include "api/video/video_frame_type.h"
#include "modules/video_coding/frame_helpers.h"
#include "modules/video_coding/timing/inter_frame_delay_variation_calculator.h"
#include "modules/video_coding/timing/jitter_estimator.h"
#include "modules/video_coding/timing/timing.h"
#include "system_wrappers/include/clock.h"
#include "video/frame_decode_timing.h"

// ---------------------------------------------------------------------------
// Heap counter. This executable replaces the global operator new and delete
// so the RAM measurement can count the C++ heap that one timeline holds.
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

void operator delete(void* p, size_t) noexcept { operator delete(p); }

namespace {

using webrtc::DataSize;
using webrtc::TimeDelta;
using webrtc::Timestamp;

constexpr int kFps = 60;
constexpr uint32_t kRtpPerFrame = 90000 / kFps;  // 1500
constexpr uint32_t kRtp0 = 3'000'000;
constexpr int64_t kStartUs = 10'000'000;  // simulated clock origin
constexpr double kFrameIntervalMs = 1000.0 / kFps;

// Upstream VideoReceiveStream2 values with NACK on (rtp history 1000 ms).
constexpr TimeDelta kMaxWaitForFrame = TimeDelta::Seconds(3);
constexpr int kMaxFramesBuffered = 800;
constexpr int kMaxFramesHistory = 1 << 13;

// ---------------------------------------------------------------------------
// Deterministic random numbers. std::normal_distribution differs between
// standard libraries, so the Gaussian is a Box-Muller on mt19937_64.
// ---------------------------------------------------------------------------
class Rng {
public:
    explicit Rng(uint64_t seed) : gen_(seed) {}
    double uniform() { return static_cast<double>(gen_() >> 11) * (1.0 / 9007199254740992.0); }
    double gaussian() {
        if (has_spare_) {
            has_spare_ = false;
            return spare_;
        }
        constexpr double kTwoPi = 6.283185307179586;
        const double u1 = std::max(uniform(), 1e-300);
        const double u2 = uniform();
        const double r = std::sqrt(-2.0 * std::log(u1));
        spare_ = r * std::sin(kTwoPi * u2);
        has_spare_ = true;
        return r * std::cos(kTwoPi * u2);
    }

private:
    std::mt19937_64 gen_;
    bool has_spare_ = false;
    double spare_ = 0.0;
};

// ---------------------------------------------------------------------------
// Field trials for one timeline.
// ---------------------------------------------------------------------------
class SpikeFieldTrials : public webrtc::FieldTrialsView {
public:
    SpikeFieldTrials() = default;
    explicit SpikeFieldTrials(std::map<std::string, std::string> trials) : trials_(std::move(trials)) {}
    std::string Lookup(absl::string_view key) const override {
        auto it = trials_.find(std::string(key));
        return it == trials_.end() ? std::string() : it->second;
    }

private:
    std::map<std::string, std::string> trials_;
};

// ---------------------------------------------------------------------------
// Access unit and the EncodedFrame that carries it into FrameBuffer.
// ---------------------------------------------------------------------------

// What rtp_video_receiver_session hands to the timeline: one complete AU.
// `id` is the frame id. For single-layer H.264 it is the unwrapped RTP
// sequence number of the AU's last packet, and a delta AU references the id
// of the AU before it (first sequence number - 1), as upstream's
// RtpSeqNumOnlyRefFinder does. The spike uses 0, 1, 2, ...
struct AccessUnit {
    int64_t id = 0;
    uint32_t rtp_timestamp = 0;
    Timestamp arrival = Timestamp::Zero();  // last packet of the AU
    size_t size = 0;
    bool keyframe = false;
    bool retransmitted = false;  // completed by a NACK repair
};

// The AU bytes stay with the caller in the spike; the frame carries only the
// size. The rebuild can move the AU bytes into the frame instead.
class SizeOnlyBuffer : public webrtc::EncodedImageBufferInterface {
public:
    explicit SizeOnlyBuffer(size_t size) : size_(size) {}
    const uint8_t* data() const override { return nullptr; }
    uint8_t* data() override { return nullptr; }
    size_t size() const override { return size_; }

private:
    size_t size_;
};

class SpikeFrame : public webrtc::EncodedFrame {
public:
    explicit SpikeFrame(const AccessUnit& au) : receive_ms_(au.arrival.ms()), retransmitted_(au.retransmitted) {
        SetId(au.id);
        SetRtpTimestamp(au.rtp_timestamp);
        SetEncodedData(webrtc::make_ref_counted<SizeOnlyBuffer>(au.size));
        SetFrameType(au.keyframe ? webrtc::VideoFrameType::kVideoFrameKey
                                 : webrtc::VideoFrameType::kVideoFrameDelta);
        if (au.keyframe) {
            num_references = 0;
        } else {
            num_references = 1;
            references[0] = au.id - 1;
        }
        is_last_spatial_layer = true;
    }
    int64_t ReceivedTime() const override { return receive_ms_; }
    bool delayed_by_retransmission() const override { return retransmitted_; }

private:
    int64_t receive_ms_;
    bool retransmitted_;
};

// ---------------------------------------------------------------------------
// SpikeTimeline: the prototype of the stage 3 viewer timeline.
// ---------------------------------------------------------------------------

struct Released {
    int64_t id = 0;
    uint32_t rtp_timestamp = 0;
    Timestamp arrival = Timestamp::Zero();
    Timestamp render_time = Timestamp::Zero();
    Timestamp released_at = Timestamp::Zero();
    TimeDelta max_wait = TimeDelta::Zero();  // < 0: released after its decode deadline
    bool keyframe = false;
};

struct Decision {
    enum class Kind {
        kIdle,             // nothing stored; ask again when a frame arrives
        kWait,             // ask again at `wake` (or when a frame arrives)
        kRelease,          // decode `frame` now, show it at frame.render_time
        kKeyframeNeeded,   // a frame did not arrive in time: send a PLI
    };
    Kind kind = Kind::kIdle;
    Timestamp wake = Timestamp::PlusInfinity();
    Released frame;
};

// Mello knob for the jitter estimate (see VideoTimingSpike.JitterSpikeRecovery).
// Upstream's JitterEstimator forgets with a fixed weight (kAlphaCountMax 400,
// scaled to 60 fps: a time constant of about 13 s), and no field trial
// changes it. `staggered_pair` runs two upstream estimators on the same input.
// Each one is reset every `window`, the second half a window after the
// first. The jitter delay is the larger of the two estimates. A young
// estimator learns fast, so the delay rises within about a second of jitter,
// and it falls at most one window after the link calms.
struct JitterKnob {
    bool staggered_pair = false;
    TimeDelta window = TimeDelta::Seconds(4);
};

class SpikeTimeline {
public:
    SpikeTimeline(webrtc::Clock* clock, const webrtc::FieldTrialsView& trials, JitterKnob knob = {})
        : clock_(clock),
          knob_(knob),
          timing_(clock, trials),
          jitter_(clock, trials),
          jitter_b_(clock, trials),
          buffer_(std::make_unique<webrtc::FrameBuffer>(kMaxFramesBuffered, kMaxFramesHistory, trials)),
          decode_timing_(clock, &timing_),
          reset_a_(clock->CurrentTime()),
          reset_b_(clock->CurrentTime() - knob.window / 2) {}

    // Network side: one complete AU.
    void insert(const AccessUnit& au) {
        auto frame = std::make_unique<SpikeFrame>(au);
        if (!buffer_->InsertFrame(std::move(frame))) {
            ++rejected_inserts_;
            return;
        }
        pending_.emplace(au.id, au);
        if (!au.retransmitted) {
            timing_.IncomingTimestamp(au.rtp_timestamp, au.arrival);
        }
    }

    // Present side: the next decision. Mirrors
    // VideoStreamBufferController::MaybeScheduleFrameForRelease,
    // FrameReadyForDecode and OnFrameReady.
    Decision poll() {
        const Timestamp now = clock_->CurrentTime();
        auto info = buffer_->DecodableTemporalUnitsInfo();
        if (!info) {
            scheduled_.reset();
            if (pending_.empty() || keyframe_required_) return {};
            // A gap: frames after a missing frame are stored, none is
            // decodable. Give up when the first stored frame is past its
            // render time plus one RTT; the missing frame was due before it.
            const Timestamp deadline = timing_.RenderTime(pending_.begin()->second.rtp_timestamp, now) + rtt_;
            if (now >= deadline) {
                keyframe_required_ = true;
                ++keyframe_requests_;
                Decision d;
                d.kind = Decision::Kind::kKeyframeNeeded;
                return d;
            }
            Decision d;
            d.kind = Decision::Kind::kWait;
            d.wake = deadline;
            return d;
        }
        if (keyframe_required_) {
            return release_next_keyframe(now);
        }
        while (info) {
            if (scheduled_ && scheduled_->rtp_timestamp == info->next_rtp_timestamp) {
                // Keep the schedule upstream committed to for this frame.
                if (scheduled_->latest_decode_time > now) {
                    Decision d;
                    d.kind = Decision::Kind::kWait;
                    d.wake = scheduled_->latest_decode_time;
                    return d;
                }
                return release(now, scheduled_->render_time);
            }
            auto schedule = decode_timing_.OnFrameBufferUpdated(info->next_rtp_timestamp, info->last_rtp_timestamp,
                                                                kMaxWaitForFrame, false);
            if (schedule) {
                scheduled_ = Scheduled{info->next_rtp_timestamp, schedule->latest_decode_time, schedule->render_time};
                if (schedule->latest_decode_time > now) {
                    Decision d;
                    d.kind = Decision::Kind::kWait;
                    d.wake = schedule->latest_decode_time;
                    return d;
                }
                return release(now, schedule->render_time);
            }
            buffer_->DropNextDecodableTemporalUnit();
            ++fast_forward_drops_;
            info = buffer_->DecodableTemporalUnitsInfo();
        }
        return {};
    }

    // Decoder side: the time the decoder took for the frame just released.
    void on_decoded(TimeDelta decode_time) { timing_.StopDecodeTimer(decode_time, clock_->CurrentTime()); }

    // RTT from RTCP. Feeds the NACK term of the jitter estimate and the
    // late-frame deadline.
    void set_rtt(TimeDelta rtt) {
        rtt_ = rtt;
        jitter_.UpdateRtt(rtt);
        jitter_b_.UpdateRtt(rtt);
    }

    // A/V sync (stage 7): the video playout delay follows audio.
    void set_min_delay(TimeDelta d) {
        min_delay_ = d;
        timing_.set_min_playout_delay(d);
    }
    void set_max_delay(TimeDelta d) {
        max_delay_ = d;
        timing_.set_max_playout_delay(d);
    }

    const webrtc::VCMTiming& timing() const { return timing_; }
    webrtc::VCMTiming& timing() { return timing_; }
    const webrtc::FrameBuffer& buffer() const { return *buffer_; }
    int fast_forward_drops() const { return fast_forward_drops_; }
    int discarded_for_keyframe() const { return discarded_for_keyframe_; }
    int keyframe_requests() const { return keyframe_requests_; }
    int rejected_inserts() const { return rejected_inserts_; }
    int timing_resets() const { return timing_resets_; }
    size_t pending() const { return pending_.size(); }

private:
    struct Scheduled {
        uint32_t rtp_timestamp;
        Timestamp latest_decode_time;
        Timestamp render_time;
    };

    // ForceKeyFrameReleaseImmediately: discard decodable delta frames until a
    // key frame.
    Decision release_next_keyframe(Timestamp now) {
        while (buffer_->DecodableTemporalUnitsInfo()) {
            const uint32_t rtp = buffer_->DecodableTemporalUnitsInfo()->next_rtp_timestamp;
            auto frames = buffer_->ExtractNextDecodableTemporalUnit();
            if (frames.empty()) continue;
            if (frames.front()->is_keyframe()) {
                return finish_release(now, timing_.RenderTime(rtp, now), std::move(frames));
            }
            discarded_for_keyframe_ += static_cast<int>(frames.size());
            erase_pending_through(frames.back()->Id());
        }
        return {};
    }

    Decision release(Timestamp now, Timestamp render_time) {
        auto frames = buffer_->ExtractNextDecodableTemporalUnit();
        if (frames.empty()) return {};
        return finish_release(now, render_time, std::move(frames));
    }

    Decision finish_release(Timestamp now, Timestamp render_time,
                            absl::InlinedVector<std::unique_ptr<webrtc::EncodedFrame>, 4> frames) {
        scheduled_.reset();
        const webrtc::EncodedFrame& first = *frames.front();
        if (first.is_keyframe()) keyframe_required_ = false;

        if (webrtc::FrameHasBadRenderTiming(render_time, now) ||
            webrtc::TargetVideoDelayIsTooLarge(timing_.TargetVideoDelay())) {
            jitter_.Reset();
            timing_.Reset();
            // VCMTiming::Reset clears the minimum delay; apply it again.
            timing_.set_min_playout_delay(min_delay_);
            if (max_delay_) timing_.set_max_playout_delay(*max_delay_);
            render_time = timing_.RenderTime(first.RtpTimestamp(), now);
            ++timing_resets_;
        }

        bool retransmitted = false;
        DataSize size = DataSize::Zero();
        Timestamp max_receive = Timestamp::MinusInfinity();
        for (auto& f : frames) {
            f->SetRenderTime(render_time.ms());
            retransmitted |= f->delayed_by_retransmission();
            size += DataSize::Bytes(f->size());
            max_receive = std::max(max_receive, *f->ReceivedTimestamp());
        }
        if (!retransmitted) {
            auto ifdv = ifdv_.Calculate(first.RtpTimestamp(), max_receive);
            if (ifdv) {
                jitter_.UpdateEstimate(*ifdv, size);
                if (knob_.staggered_pair) jitter_b_.UpdateEstimate(*ifdv, size);
            }
            // Upstream constants: kRttMult 0.9, kRttMultAddCap 200 ms.
            TimeDelta jitter = jitter_.GetJitterEstimate(0.9, TimeDelta::Millis(200));
            if (knob_.staggered_pair) {
                jitter = std::max(jitter, jitter_b_.GetJitterEstimate(0.9, TimeDelta::Millis(200)));
                if (now - reset_a_ >= knob_.window) {
                    jitter_.Reset();
                    reset_a_ = now;
                }
                if (now - reset_b_ >= knob_.window) {
                    jitter_b_.Reset();
                    reset_b_ = now;
                }
            }
            timing_.SetJitterDelay(jitter);
            timing_.UpdateCurrentDelay(render_time, now);
        } else {
            jitter_.FrameNacked();
            jitter_b_.FrameNacked();
        }
        timing_.SetLastDecodeScheduledTimestamp(now);

        Decision d;
        d.kind = Decision::Kind::kRelease;
        auto it = pending_.find(first.Id());
        d.frame.id = first.Id();
        d.frame.rtp_timestamp = first.RtpTimestamp();
        d.frame.arrival = it != pending_.end() ? it->second.arrival : max_receive;
        d.frame.render_time = render_time;
        d.frame.released_at = now;
        d.frame.max_wait = timing_.MaxWaitingTime(render_time, now, false);
        d.frame.keyframe = first.is_keyframe();
        erase_pending_through(frames.back()->Id());
        return d;
    }

    void erase_pending_through(int64_t id) { pending_.erase(pending_.begin(), pending_.upper_bound(id)); }

    webrtc::Clock* clock_;
    const JitterKnob knob_;
    webrtc::VCMTiming timing_;
    webrtc::JitterEstimator jitter_;
    webrtc::JitterEstimator jitter_b_;  // used with knob_.staggered_pair only
    webrtc::InterFrameDelayVariationCalculator ifdv_;
    std::unique_ptr<webrtc::FrameBuffer> buffer_;
    webrtc::FrameDecodeTiming decode_timing_;
    Timestamp reset_a_;
    Timestamp reset_b_;
    std::optional<Scheduled> scheduled_;
    std::map<int64_t, AccessUnit> pending_;  // inserted, not released
    TimeDelta rtt_ = TimeDelta::Millis(30);
    TimeDelta min_delay_ = TimeDelta::Zero();
    std::optional<TimeDelta> max_delay_;
    bool keyframe_required_ = true;  // the stream starts on a key frame
    int fast_forward_drops_ = 0;
    int discarded_for_keyframe_ = 0;
    int keyframe_requests_ = 0;
    int rejected_inserts_ = 0;
    int timing_resets_ = 0;
};

// ---------------------------------------------------------------------------
// Stream generator: 60 fps, one path (frames keep their order), a
// serialization delay from the link rate, a base delay and a Gaussian delay
// whose standard deviation is a function of the send time.
// ---------------------------------------------------------------------------
struct StreamSpec {
    int frames = kFps * 20;
    uint64_t seed = 1;
    double base_delay_ms = 40.0;
    double rate_mbps = 30.0;
    int gop_frames = 120;  // 2 s GOP, as the host sends today; 0: only frame 0
    size_t delta_bytes = 12'000;
    size_t key_bytes = 60'000;
    std::function<double(double t_s)> jitter_ms;  // standard deviation at send time t_s
    std::vector<int> extra_keyframes;
};

std::vector<AccessUnit> make_stream(const StreamSpec& spec) {
    Rng rng(spec.seed);
    std::vector<AccessUnit> out;
    out.reserve(static_cast<size_t>(spec.frames));
    double link_free_us = 0.0;
    double prev_arrival_us = 0.0;
    for (int i = 0; i < spec.frames; ++i) {
        const int64_t send_us = kStartUs + static_cast<int64_t>(i) * 1'000'000 / kFps;
        const bool key = i == 0 || (spec.gop_frames > 0 && i % spec.gop_frames == 0) ||
                         std::find(spec.extra_keyframes.begin(), spec.extra_keyframes.end(), i) !=
                             spec.extra_keyframes.end();
        const double r = rng.uniform();
        const double g = rng.gaussian();  // drawn for every frame so seeds line up
        const size_t size = key ? spec.key_bytes : static_cast<size_t>(spec.delta_bytes * (0.7 + 0.6 * r));
        link_free_us = std::max(link_free_us, static_cast<double>(send_us)) + size * 8.0 / spec.rate_mbps;
        const double sigma = spec.jitter_ms ? spec.jitter_ms((send_us - kStartUs) / 1e6) : 0.0;
        const double extra_ms = std::max(sigma * g, -(spec.base_delay_ms - 5.0));
        double arrival_us = link_free_us + (spec.base_delay_ms + extra_ms) * 1000.0;
        arrival_us = std::max(arrival_us, prev_arrival_us);
        prev_arrival_us = arrival_us;
        AccessUnit au;
        au.id = i;
        au.rtp_timestamp = kRtp0 + static_cast<uint32_t>(i) * kRtpPerFrame;
        au.arrival = Timestamp::Micros(static_cast<int64_t>(arrival_us));
        au.size = size;
        au.keyframe = key;
        out.push_back(au);
    }
    return out;
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------
struct Sample {
    double t_s = 0.0;  // since the first frame was sent
    double target_ms = 0.0;
    double current_ms = 0.0;
    double jitter_ms = 0.0;  // VCMTiming jitter delay (minimum_delay)
    double decode_ms = 0.0;
};

struct RunOptions {
    TimeDelta rtt = TimeDelta::Millis(30);
    std::optional<TimeDelta> min_delay;
    std::optional<TimeDelta> max_delay;
    std::function<int(int64_t id)> decode_ms = [](int64_t) { return 3; };
    TimeDelta sample_period = TimeDelta::Millis(100);
    std::map<std::string, std::string> trials;
    JitterKnob knob;
    // Called at each sample time, before the poll.
    std::function<void(double t_s, SpikeTimeline&)> on_sample;
};

struct RunResult {
    std::vector<Released> released;
    std::vector<Sample> samples;
    std::vector<double> keyframe_requests_s;
    int fast_forward_drops = 0;
    int buffer_dropped = 0;
    int discarded_for_keyframe = 0;
    int rejected_inserts = 0;
    int timing_resets = 0;
    int64_t insert_ns = 0;
    int64_t insert_calls = 0;
    int64_t poll_ns = 0;
    int64_t poll_calls = 0;
    int64_t release_poll_ns = 0;
    int64_t release_polls = 0;
};

double since_start_s(Timestamp t) { return (t.us() - kStartUs) / 1e6; }

RunResult run(std::vector<AccessUnit> stream, const RunOptions& opt) {
    std::stable_sort(stream.begin(), stream.end(),
                     [](const AccessUnit& a, const AccessUnit& b) { return a.arrival < b.arrival; });
    webrtc::SimulatedClock clock(Timestamp::Micros(kStartUs));
    SpikeFieldTrials trials(opt.trials);
    SpikeTimeline tl(&clock, trials, opt.knob);
    tl.set_rtt(opt.rtt);
    if (opt.min_delay) tl.set_min_delay(*opt.min_delay);
    if (opt.max_delay) tl.set_max_delay(*opt.max_delay);

    RunResult res;
    res.released.reserve(stream.size());
    size_t next = 0;
    Timestamp wake = Timestamp::PlusInfinity();
    Timestamp next_sample = Timestamp::Micros(kStartUs);
    const Timestamp end = stream.back().arrival + TimeDelta::Seconds(1);
    using Clock = std::chrono::steady_clock;

    while (true) {
        const Timestamp next_arrival = next < stream.size() ? stream[next].arrival : Timestamp::PlusInfinity();
        Timestamp t = std::min(next_arrival, wake);
        if (next_sample <= end) t = std::min(t, next_sample);
        if (t.IsPlusInfinity()) break;
        const Timestamp now = clock.CurrentTime();
        if (t > now) clock.AdvanceTime(t - now);

        while (next < stream.size() && stream[next].arrival <= clock.CurrentTime()) {
            const auto c0 = Clock::now();
            tl.insert(stream[next++]);
            res.insert_ns += std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - c0).count();
            ++res.insert_calls;
        }
        if (next_sample <= clock.CurrentTime()) {
            const double t_s = since_start_s(next_sample);
            if (opt.on_sample) opt.on_sample(t_s, tl);
            const auto timings = tl.timing().GetTimings();
            Sample s;
            s.t_s = t_s;
            s.target_ms = tl.timing().TargetVideoDelay().ms<double>();
            s.current_ms = timings.current_delay.ms<double>();
            s.jitter_ms = timings.minimum_delay.ms<double>();
            s.decode_ms = timings.estimated_max_decode_time.ms<double>();
            res.samples.push_back(s);
            next_sample += opt.sample_period;
        }
        wake = Timestamp::PlusInfinity();
        while (true) {
            const auto c0 = Clock::now();
            Decision d = tl.poll();
            const int64_t ns = std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - c0).count();
            res.poll_ns += ns;
            ++res.poll_calls;
            if (d.kind == Decision::Kind::kRelease) {
                res.release_poll_ns += ns;
                ++res.release_polls;
                res.released.push_back(d.frame);
                tl.on_decoded(TimeDelta::Millis(opt.decode_ms(d.frame.id)));
                continue;
            }
            if (d.kind == Decision::Kind::kKeyframeNeeded) {
                res.keyframe_requests_s.push_back(since_start_s(clock.CurrentTime()));
                continue;
            }
            if (d.kind == Decision::Kind::kWait) wake = d.wake;
            break;
        }
    }
    res.fast_forward_drops = tl.fast_forward_drops();
    res.buffer_dropped = tl.buffer().GetTotalNumberOfDroppedFrames();
    res.discarded_for_keyframe = tl.discarded_for_keyframe();
    res.rejected_inserts = tl.rejected_inserts();
    res.timing_resets = tl.timing_resets();
    return res;
}

// ---------------------------------------------------------------------------
// Statistics helpers
// ---------------------------------------------------------------------------
double percentile(std::vector<double> v, double p) {
    if (v.empty()) return 0.0;
    std::sort(v.begin(), v.end());
    const size_t idx = std::min(v.size() - 1, static_cast<size_t>(p * (v.size() - 1) + 0.5));
    return v[idx];
}

double mean(const std::vector<double>& v) {
    if (v.empty()) return 0.0;
    double s = 0.0;
    for (double x : v) s += x;
    return s / v.size();
}

double mean_target(const RunResult& r, double from_s, double to_s) {
    std::vector<double> v;
    for (const auto& s : r.samples)
        if (s.t_s >= from_s && s.t_s < to_s) v.push_back(s.target_ms);
    return mean(v);
}

double mean_current(const RunResult& r, double from_s, double to_s) {
    std::vector<double> v;
    for (const auto& s : r.samples)
        if (s.t_s >= from_s && s.t_s < to_s) v.push_back(s.current_ms);
    return mean(v);
}

double max_target(const RunResult& r, double from_s, double to_s) {
    double m = 0.0;
    for (const auto& s : r.samples)
        if (s.t_s >= from_s && s.t_s < to_s) m = std::max(m, s.target_ms);
    return m;
}

struct ReleaseStats {
    int count = 0;
    int late = 0;  // released after the decode deadline of its render time
    double interval_mean_ms = 0.0;
    double interval_dev_p99_ms = 0.0;  // |interval - 16.67 ms|
    double hold_mean_ms = 0.0;         // release time - arrival
    double hold_p95_ms = 0.0;
    double render_minus_arrival_mean_ms = 0.0;
    double late_p95_ms = 0.0;  // how late the late frames were
    bool in_order = true;
};

ReleaseStats release_stats(const RunResult& r, double from_s, double to_s) {
    ReleaseStats st;
    std::vector<double> intervals, devs, holds, render_lead, lateness;
    const Released* prev = nullptr;
    for (const auto& f : r.released) {
        if (prev && (f.id <= prev->id || f.rtp_timestamp <= prev->rtp_timestamp)) st.in_order = false;
        const double t = since_start_s(f.released_at);
        if (t >= from_s && t < to_s) {
            ++st.count;
            if (f.max_wait < TimeDelta::Millis(-1)) {
                ++st.late;
                lateness.push_back(-f.max_wait.ms<double>());
            }
            if (prev && prev->id == f.id - 1) {
                const double iv = (f.released_at - prev->released_at).ms<double>();
                intervals.push_back(iv);
                devs.push_back(std::fabs(iv - kFrameIntervalMs));
            }
            holds.push_back((f.released_at - f.arrival).ms<double>());
            render_lead.push_back((f.render_time - f.arrival).ms<double>());
        }
        prev = &f;
    }
    st.interval_mean_ms = mean(intervals);
    st.interval_dev_p99_ms = percentile(devs, 0.99);
    st.hold_mean_ms = mean(holds);
    st.hold_p95_ms = percentile(holds, 0.95);
    st.render_minus_arrival_mean_ms = mean(render_lead);
    st.late_p95_ms = percentile(lateness, 0.95);
    return st;
}

// Seconds after `from_s` until the target stays within `band_ms` of `ref_ms`
// for the rest of the run. Negative when it never settles.
double recovery_s(const RunResult& r, double from_s, double ref_ms, double band_ms, bool current = false) {
    double last_out = from_s;
    bool any = false;
    for (const auto& s : r.samples) {
        if (s.t_s < from_s) continue;
        any = true;
        if ((current ? s.current_ms : s.target_ms) > ref_ms + band_ms) last_out = s.t_s + 0.1;
    }
    if (!any) return -1.0;
    const double end = r.samples.back().t_s;
    if (last_out > end - 1.0) return -1.0;
    return last_out - from_s;
}

void print_trace(const char* name, const RunResult& r, double from_s, double to_s, double step_s) {
    std::printf("[video-timing-spike] %s trace (t_s target current jitter decode):", name);
    double next = from_s;
    for (const auto& s : r.samples) {
        if (s.t_s + 1e-9 < next || s.t_s >= to_s) continue;
        std::printf(" %.1f:%.0f/%.0f/%.0f/%.0f", s.t_s, s.target_ms, s.current_ms, s.jitter_ms, s.decode_ms);
        next += step_s;
    }
    std::printf("\n");
}

// ---------------------------------------------------------------------------
// 1. Clean link
// ---------------------------------------------------------------------------
TEST(VideoTimingSpike, Clean) {
    StreamSpec spec;
    spec.frames = kFps * 30;
    const RunResult r = run(make_stream(spec), {});
    const auto st = release_stats(r, 5.0, 29.0);
    const double target = mean_target(r, 10.0, 29.0);
    std::printf("[video-timing-spike] clean: released=%zu/%d target=%.1f ms (jitter %.1f + decode %.1f + render 10) "
                "interval mean=%.3f ms dev_p99=%.3f ms hold mean=%.1f p95=%.1f ms render-arrival=%.1f ms late=%d "
                "ff_drops=%d buffer_dropped=%d resets=%d\n",
                r.released.size(), spec.frames, target, r.samples.back().jitter_ms, r.samples.back().decode_ms,
                st.interval_mean_ms, st.interval_dev_p99_ms, st.hold_mean_ms, st.hold_p95_ms,
                st.render_minus_arrival_mean_ms, st.late, r.fast_forward_drops, r.buffer_dropped, r.timing_resets);
    print_trace("clean", r, 0.0, 30.0, 2.0);

    EXPECT_EQ(r.released.size(), static_cast<size_t>(spec.frames));
    EXPECT_TRUE(st.in_order);
    EXPECT_EQ(st.late, 0);
    EXPECT_EQ(r.fast_forward_drops, 0);
    EXPECT_EQ(r.timing_resets, 0);
    EXPECT_NEAR(st.interval_mean_ms, kFrameIntervalMs, 0.05);
    EXPECT_LT(st.interval_dev_p99_ms, 1.0) << "release cadence is not steady";
    // Minimum target: 1 ms jitter floor + 10 ms OS jitter + decode p95 (3 ms)
    // + 10 ms render delay = 24 ms. Allow the frame-size term.
    EXPECT_LT(target, 40.0);
    EXPECT_GE(target, 23.0);
}

// ---------------------------------------------------------------------------
// 2. 20 ms Gaussian jitter
// ---------------------------------------------------------------------------
TEST(VideoTimingSpike, Jitter20msGaussian) {
    StreamSpec spec;
    spec.frames = kFps * 60;
    spec.seed = 2;
    spec.jitter_ms = [](double t) { return t >= 10.0 ? 20.0 : 0.0; };
    const auto stream = make_stream(spec);
    RunOptions pair4;
    pair4.knob.staggered_pair = true;
    for (int k = 0; k < 2; ++k) {
        const char* name = k == 0 ? "defaults" : "pair4s";
        const RunResult r = run(stream, k == 0 ? RunOptions{} : pair4);
        const double clean = mean_target(r, 5.0, 10.0);
        const double during = mean_target(r, 40.0, 60.0);
        const auto st_all = release_stats(r, 0.0, 61.0);
        const auto st_jit = release_stats(r, 10.0, 61.0);
        const auto st_late = release_stats(r, 40.0, 61.0);
        std::printf("[video-timing-spike] jitter20 %s: target clean=%.1f ms, 30-50 s into jitter=%.1f ms (max %.1f); "
                    "playout delay clean=%.1f, 30-50 s=%.1f ms; released=%zu/%d in_order=%d late=%d of %d (p95 %.1f "
                    "ms late; last 20 s: %d of %d) ff_drops=%d buffer_dropped=%d hold mean=%.1f p95=%.1f ms "
                    "resets=%d\n",
                    name, clean, during, max_target(r, 10.0, 61.0), mean_current(r, 5.0, 10.0),
                    mean_current(r, 40.0, 60.0), r.released.size(), spec.frames, st_all.in_order ? 1 : 0, st_jit.late,
                    st_jit.count, st_jit.late_p95_ms, st_late.late, st_late.count, r.fast_forward_drops,
                    r.buffer_dropped, st_jit.hold_mean_ms, st_jit.hold_p95_ms, r.timing_resets);
        print_trace(name, r, 0.0, 61.0, 5.0);

        EXPECT_TRUE(st_all.in_order) << name;
        EXPECT_EQ(r.released.size(), static_cast<size_t>(spec.frames)) << name << ": a frame was dropped";
        EXPECT_GT(during, clean + 10.0) << name << ": target delay did not rise under jitter";
        EXPECT_LT(during, 150.0) << name;
        // Most frames make their render time once the estimate has converged.
        EXPECT_LT(st_late.late, st_late.count / 10) << name;
    }
}

// ---------------------------------------------------------------------------
// 3. 3 s jitter spike, then clean
// ---------------------------------------------------------------------------
struct SpikeOutcome {
    double clean = 0.0;     // mean target before the jitter
    double peak = 0.0;      // max target from the jitter start to 1 s after its end
    double recovery = 0.0;  // s after the jitter end until the target stays within clean + 10 ms
    int late = 0;           // frames late from the jitter start to 1 s after its end
    int frames = 0;
    double late_p95 = 0.0;        // ms
    double current_clean = 0.0;   // playout delay (VCMTiming current delay) before the jitter
    double current_jitter = 0.0;  // mean playout delay during the jitter
    double current_recovery = 0.0;
};

// Clean until 20 s, 20 ms Gaussian jitter for `jitter_s`, then clean.
SpikeOutcome run_spike(const RunOptions& opt, double jitter_s, const char* name) {
    constexpr double kOn = 20.0;
    const double off = kOn + jitter_s;
    StreamSpec spec;
    spec.frames = static_cast<int>(kFps * (off + 60.0));
    spec.seed = 3;
    spec.jitter_ms = [off](double t) { return (t >= kOn && t < off) ? 20.0 : 0.0; };
    const RunResult r = run(make_stream(spec), opt);
    SpikeOutcome o;
    o.clean = mean_target(r, kOn - 5.0, kOn);
    o.peak = max_target(r, kOn, off + 1.0);
    o.recovery = recovery_s(r, off, o.clean, 10.0);
    const auto st = release_stats(r, kOn, off + 1.0);
    o.late = st.late;
    o.frames = st.count;
    o.late_p95 = st.late_p95_ms;
    o.current_clean = mean_current(r, kOn - 5.0, kOn);
    o.current_jitter = mean_current(r, kOn, off);
    o.current_recovery = recovery_s(r, off, o.current_clean, 10.0, true);
    std::printf("[video-timing-spike] jitter %.0f s, %s: target clean=%.1f peak=%.1f (+%.1f) ms, recovery to "
                "clean+10 ms=%.1f s; playout delay clean=%.1f during=%.1f ms, recovery=%.1f s; late=%d of %d "
                "(p95 %.1f ms late) (-1: not within the run)\n",
                jitter_s, name, o.clean, o.peak, o.peak - o.clean, o.recovery, o.current_clean, o.current_jitter,
                o.current_recovery, o.late, o.frames, o.late_p95);
    print_trace(name, r, kOn - 2.0, off + 40.0, 1.0);
    return o;
}

TEST(VideoTimingSpike, JitterSpikeRecovery) {
    RunOptions defaults;
    RunOptions clamp;
    clamp.trials["WebRTC-JitterEstimatorConfig"] = "num_stddev_delay_clamp:10";
    RunOptions pair4;
    pair4.knob.staggered_pair = true;
    pair4.knob.window = TimeDelta::Seconds(4);
    RunOptions pair2;
    pair2.knob.staggered_pair = true;
    pair2.knob.window = TimeDelta::Seconds(2);

    // The 3 s spike of the brief.
    const SpikeOutcome def3 = run_spike(defaults, 3.0, "defaults");
    run_spike(clamp, 3.0, "clamp10");
    const SpikeOutcome p4_3 = run_spike(pair4, 3.0, "pair4s");
    run_spike(pair2, 3.0, "pair2s");
    // 30 s of jitter, so the upstream estimate has time to rise.
    const SpikeOutcome def30 = run_spike(defaults, 30.0, "defaults");
    run_spike(clamp, 30.0, "clamp10");
    const SpikeOutcome p4_30 = run_spike(pair4, 30.0, "pair4s");
    run_spike(pair2, 30.0, "pair2s");

    // Upstream defaults: the estimate barely moves in a 3 s spike (the
    // frames are shown late instead), rises over about 25 s of sustained
    // jitter, and comes back within 5 s.
    EXPECT_LT(def3.peak, def3.clean + 10.0);
    EXPECT_GT(def30.peak, def30.clean + 10.0);
    EXPECT_GE(def30.recovery, 0.0);
    EXPECT_LT(def30.recovery, 5.0);
    // The staggered pair rises during the 3 s spike, comes back within 5 s,
    // and is late on fewer frames than the defaults.
    EXPECT_GT(p4_3.peak, p4_3.clean + 10.0) << "target delay did not rise during the spike";
    EXPECT_GE(p4_3.recovery, 0.0);
    EXPECT_LT(p4_3.recovery, 5.0);
    EXPECT_LT(p4_3.late, def3.late);
    EXPECT_GE(p4_30.recovery, 0.0);
    EXPECT_LT(p4_30.recovery, 5.0);
    EXPECT_LT(std::fabs(p4_3.clean - def3.clean), 10.0) << "the knob moves the clean target";
}

// ---------------------------------------------------------------------------
// 4. Late repair inside the deadline, and a frame past the deadline
// ---------------------------------------------------------------------------
TEST(VideoTimingSpike, LateRepairAndKeyframe) {
    constexpr int kRepaired = 600;   // 10 s
    constexpr int kTooLate = 1200;   // 20 s
    const TimeDelta rtt = TimeDelta::Millis(30);
    StreamSpec spec;
    spec.frames = kFps * 30;
    spec.seed = 4;
    spec.gop_frames = 0;  // infinite GOP: a key frame only on request

    auto build = [&](std::optional<int> keyframe_at) {
        StreamSpec s = spec;
        if (keyframe_at) s.extra_keyframes.push_back(*keyframe_at);
        auto stream = make_stream(s);
        stream[kRepaired].arrival += TimeDelta::Millis(40);
        stream[kRepaired].retransmitted = true;
        stream[kTooLate].arrival += TimeDelta::Millis(200);
        stream[kTooLate].retransmitted = true;
        return stream;
    };
    RunOptions opt;
    opt.rtt = rtt;

    // Pass 1 finds the key frame request time; pass 2 sends the key frame
    // the host would encode after the PLI (first frame sent after the
    // request + RTT / 2).
    const RunResult p1 = run(build(std::nullopt), opt);
    ASSERT_FALSE(p1.keyframe_requests_s.empty());
    const double req_s = p1.keyframe_requests_s.front();
    const int key_at = static_cast<int>(std::ceil((req_s + rtt.ms() / 2000.0) * kFps));
    const RunResult r = run(build(key_at), opt);

    const Released* repaired = nullptr;
    bool too_late_released = false;
    int released_after_gap = 0;
    double first_after_key_s = -1.0;
    for (const auto& f : r.released) {
        if (f.id == kRepaired) repaired = &f;
        if (f.id == kTooLate) too_late_released = true;
        if (f.id > kTooLate && f.id < key_at) ++released_after_gap;
        if (f.id == key_at) first_after_key_s = since_start_s(f.released_at);
    }
    ASSERT_NE(repaired, nullptr);
    const double nominal_arrival_s = since_start_s(repaired->arrival) - 0.040;
    const auto st_rep = release_stats(r, 9.0, 12.0);
    const double too_late_render_s =
        since_start_s(Timestamp::Micros(kStartUs + static_cast<int64_t>(kTooLate) * 1'000'000 / kFps));
    std::printf("[video-timing-spike] late-repair: frame %d arrived +40 ms (retransmitted) at %.3f s, released at "
                "%.3f s, render %.3f s (late by %.1f ms against its decode deadline), frames late in 9-12 s=%d, "
                "keyframe requests before 20 s=%d\n",
                kRepaired, since_start_s(repaired->arrival), since_start_s(repaired->released_at),
                since_start_s(repaired->render_time), -repaired->max_wait.ms<double>(), st_rep.late,
                static_cast<int>(std::count_if(r.keyframe_requests_s.begin(), r.keyframe_requests_s.end(),
                                               [](double t) { return t < 19.9; })));
    std::printf("[video-timing-spike] too-late: frame %d +200 ms; keyframe needed at %.3f s (send time %.3f s, "
                "nominal arrival +%.0f ms); key frame %d released at %.3f s; frames after the gap discarded=%d, "
                "released=%d; buffer dropped=%d; frozen for %.0f ms\n",
                kTooLate, req_s, too_late_render_s, spec.base_delay_ms, key_at, first_after_key_s,
                r.discarded_for_keyframe, released_after_gap, r.buffer_dropped,
                (first_after_key_s - (too_late_render_s + spec.base_delay_ms / 1000.0)) * 1000.0);
    (void)nominal_arrival_s;

    // The repaired frame is decoded, no key frame is requested for it, and
    // nothing is dropped.
    EXPECT_EQ(std::count_if(r.keyframe_requests_s.begin(), r.keyframe_requests_s.end(),
                            [](double t) { return t < 19.9; }),
              0);
    EXPECT_EQ(r.fast_forward_drops, 0);
    // The frame past the deadline: one key frame request, the frames after
    // the gap are not shown, the stream resumes on the key frame.
    EXPECT_EQ(r.keyframe_requests_s.size(), 1u);
    EXPECT_FALSE(too_late_released);
    EXPECT_EQ(released_after_gap, 0);
    EXPECT_GT(first_after_key_s, 0.0);
    // Every frame before the gap and from the key frame on is released.
    EXPECT_EQ(r.released.size(), static_cast<size_t>(kTooLate + (spec.frames - key_at)));

    // A repair that arrives inside the playout delay: +5 ms. The buffer holds
    // the frames after it and releases the repair on its render time.
    {
        auto stream = make_stream(spec);
        stream[kRepaired].arrival += TimeDelta::Millis(5);
        stream[kRepaired].retransmitted = true;
        const RunResult r0 = run(stream, opt);
        const Released* rep0 = nullptr;
        for (const auto& f : r0.released)
            if (f.id == kRepaired) rep0 = &f;
        ASSERT_NE(rep0, nullptr);
        std::printf("[video-timing-spike] small repair: frame %d +5 ms released %.1f ms after arrival, "
                    "max_wait=%.1f ms (late if < 0), frames late in 9-12 s=%d, keyframe requests=%zu\n",
                    kRepaired, (rep0->released_at - rep0->arrival).ms<double>(), rep0->max_wait.ms<double>(),
                    release_stats(r0, 9.0, 12.0).late, r0.keyframe_requests_s.size());
        EXPECT_GE(rep0->max_wait, TimeDelta::Millis(-1));
        EXPECT_EQ(release_stats(r0, 9.0, 12.0).late, 0);
        EXPECT_TRUE(r0.keyframe_requests_s.empty());
    }

    // RTT input: after three NACK repairs within 60 s, JitterEstimator adds
    // 0.9 x RTT (cap 200 ms) to the jitter delay. The same +40 ms repair then
    // lands inside the target and is not late.
    auto build_rtt = [&]() {
        auto stream = make_stream(spec);
        for (int id : {300, 360, 420}) {
            stream[id].arrival += TimeDelta::Millis(40);
            stream[id].retransmitted = true;
        }
        stream[kRepaired].arrival += TimeDelta::Millis(40);
        stream[kRepaired].retransmitted = true;
        return stream;
    };
    const RunResult r2 = run(build_rtt(), opt);
    const Released* rep2 = nullptr;
    for (const auto& f : r2.released)
        if (f.id == kRepaired) rep2 = &f;
    ASSERT_NE(rep2, nullptr);
    const double target_before = mean_target(r, 8.0, 9.9);
    const double target_rtt = mean_target(r2, 8.0, 9.9);
    const double current_before = mean_current(r, 8.0, 9.9);
    const double current_rtt = mean_current(r2, 8.0, 9.9);
    std::printf("[video-timing-spike] late-repair after 3 repairs (RTT %lld ms): target %.1f ms, playout delay %.1f "
                "ms (single repair run: %.1f and %.1f ms); frame %d +40 ms max_wait=%.1f ms (late if < 0), frames "
                "late in 9-12 s=%d\n",
                static_cast<long long>(rtt.ms()), target_rtt, current_rtt, target_before, current_before, kRepaired,
                rep2->max_wait.ms<double>(), release_stats(r2, 9.0, 12.0).late);
    EXPECT_NEAR(target_rtt - target_before, 0.9 * rtt.ms(), 3.0);
    // The playout delay grows only by observed lateness (VCMTiming current
    // delay), so the repair is still late, but by less.
    EXPECT_GT(rep2->max_wait, repaired->max_wait);
    EXPECT_TRUE(r2.keyframe_requests_s.empty());
}

// ---------------------------------------------------------------------------
// 5. Decode time input
// ---------------------------------------------------------------------------
TEST(VideoTimingSpike, DecodeTimeP95) {
    StreamSpec spec;
    spec.frames = kFps * 40;
    spec.seed = 5;
    RunOptions opt;
    opt.decode_ms = [](int64_t id) { return id < kFps * 20 ? 3 : 12; };
    const RunResult r = run(make_stream(spec), opt);
    auto at = [&](double t) {
        for (const auto& s : r.samples)
            if (s.t_s >= t) return s;
        return r.samples.back();
    };
    const Sample a = at(19.5), b = at(39.5);
    double adopt_s = -1.0;
    for (const auto& s : r.samples)
        if (s.t_s >= 20.0 && s.decode_ms >= 12.0) {
            adopt_s = s.t_s - 20.0;
            break;
        }
    std::printf("[video-timing-spike] decode: 3 ms -> target %.1f ms (jitter %.1f + decode p95 %.1f + 10); "
                "12 ms -> target %.1f ms (jitter %.1f + decode p95 %.1f + 10); p95 adopted 12 ms after %.2f s\n",
                a.target_ms, a.jitter_ms, a.decode_ms, b.target_ms, b.jitter_ms, b.decode_ms, adopt_s);
    EXPECT_EQ(a.decode_ms, 3.0);
    EXPECT_EQ(b.decode_ms, 12.0);
    EXPECT_NEAR(a.target_ms, a.jitter_ms + a.decode_ms + 10.0, 0.5);
    EXPECT_NEAR(b.target_ms, b.jitter_ms + b.decode_ms + 10.0, 0.5);
    EXPECT_GT(b.target_ms, a.target_ms + 8.0);
}

// ---------------------------------------------------------------------------
// 6. Minimum playout delay (A/V sync input)
// ---------------------------------------------------------------------------
TEST(VideoTimingSpike, MinPlayoutDelay) {
    StreamSpec spec;
    spec.frames = kFps * 30;
    spec.seed = 6;
    RunOptions opt;
    opt.on_sample = [](double t_s, SpikeTimeline& tl) {
        if (t_s >= 10.0 && t_s < 20.0) {
            tl.set_min_delay(TimeDelta::Millis(80));
        } else {
            tl.set_min_delay(TimeDelta::Zero());
        }
    };
    const RunResult r = run(make_stream(spec), opt);
    const double before = mean_target(r, 5.0, 10.0);
    const double with80 = mean_target(r, 11.0, 20.0);
    const double after = mean_target(r, 25.0, 30.0);
    const auto s_before = release_stats(r, 5.0, 10.0);
    const auto s_80 = release_stats(r, 11.0, 20.0);
    const auto s_after = release_stats(r, 25.0, 30.0);
    const auto s_step = release_stats(r, 9.9, 10.5);
    std::printf("[video-timing-spike] min-delay: target before=%.1f ms, with min 80 ms=%.1f ms, after=%.1f ms; "
                "render-arrival before=%.1f ms, with 80=%.1f ms, after=%.1f ms; frames released at the step "
                "(9.9-10.5 s)=%d of 36; late with 80=%d\n",
                before, with80, after, s_before.render_minus_arrival_mean_ms, s_80.render_minus_arrival_mean_ms,
                s_after.render_minus_arrival_mean_ms, s_step.count, s_80.late);
    EXPECT_NEAR(with80, 80.0, 0.5);
    EXPECT_LT(before, 40.0);
    EXPECT_LT(after, 40.0);
    // The playout delay (render time - arrival) follows the minimum.
    EXPECT_GT(s_80.render_minus_arrival_mean_ms, 70.0);
    EXPECT_LT(s_80.render_minus_arrival_mean_ms, 90.0);
}

// ---------------------------------------------------------------------------
// VCMTiming switches to "low-latency rendering" when the minimum delay is 0
// and the maximum is 500 ms or less: RenderTime returns 0 and frames are
// decoded at once with 8 ms pacing. A 150 ms cap set through
// set_max_playout_delay with a 0 minimum removes the jitter buffer.
// ---------------------------------------------------------------------------
TEST(VideoTimingSpike, MaxDelayCapTurnsOffJitterBuffer) {
    webrtc::SimulatedClock clock(Timestamp::Micros(kStartUs));
    SpikeFieldTrials trials;
    webrtc::VCMTiming timing(&clock, trials);
    timing.IncomingTimestamp(kRtp0, clock.CurrentTime());
    timing.set_max_playout_delay(TimeDelta::Millis(150));
    const Timestamp low_latency = timing.RenderTime(kRtp0, clock.CurrentTime());
    timing.set_min_playout_delay(TimeDelta::Millis(1));
    const Timestamp with_min = timing.RenderTime(kRtp0, clock.CurrentTime());
    std::printf("[video-timing-spike] cap: max 150 ms, min 0 -> render_time=%lld us (0 = decode at once); "
                "min 1 ms -> render_time=now+%lld us\n",
                static_cast<long long>(low_latency.us()),
                static_cast<long long>((with_min - clock.CurrentTime()).us()));
    EXPECT_TRUE(low_latency.IsZero());
    EXPECT_FALSE(with_min.IsZero());

    // Under jitter, the capped timeline with min 1 ms keeps the buffer.
    StreamSpec spec;
    spec.frames = kFps * 40;
    spec.seed = 2;
    spec.jitter_ms = [](double t) { return t >= 10.0 ? 20.0 : 0.0; };
    RunOptions opt;
    opt.max_delay = TimeDelta::Millis(150);
    opt.min_delay = TimeDelta::Millis(1);
    const RunResult r = run(make_stream(spec), opt);
    const auto st = release_stats(r, 25.0, 41.0);
    std::printf("[video-timing-spike] cap: max 150 ms + min 1 ms under 20 ms jitter: target=%.1f ms late=%d of %d\n",
                mean_target(r, 25.0, 41.0), st.late, st.count);
    EXPECT_TRUE(st.in_order);
}

// ---------------------------------------------------------------------------
// 7. Cost: CPU per call, heap per instance
// ---------------------------------------------------------------------------
TEST(VideoTimingSpike, CostCpuAndRam) {
    StreamSpec spec;
    spec.frames = kFps * 60;
    spec.seed = 7;
    spec.jitter_ms = [](double) { return 10.0; };
    const auto stream = make_stream(spec);
    int64_t ins_ns = 0, ins_n = 0, poll_ns = 0, poll_n = 0, rel_ns = 0, rel_n = 0;
    for (int rep = 0; rep < 5; ++rep) {
        const RunResult r = run(stream, {});
        ins_ns += r.insert_ns;
        ins_n += r.insert_calls;
        poll_ns += r.poll_ns;
        poll_n += r.poll_calls;
        rel_ns += r.release_poll_ns;
        rel_n += r.release_polls;
    }
    const double ins_us = ins_ns / 1000.0 / std::max<int64_t>(ins_n, 1);
    const double poll_us = poll_ns / 1000.0 / std::max<int64_t>(poll_n, 1);
    const double rel_us = rel_ns / 1000.0 / std::max<int64_t>(rel_n, 1);
    std::printf("[video-timing-spike] cpu: insert=%.2f us/call (%lld) poll=%.2f us/call (%lld) "
                "release poll=%.2f us/call (%lld) polls per frame=%.2f\n",
                ins_us, static_cast<long long>(ins_n), poll_us, static_cast<long long>(poll_n), rel_us,
                static_cast<long long>(rel_n), static_cast<double>(poll_n) / std::max<int64_t>(ins_n, 1));
#ifdef NDEBUG
    // Generous bounds: one 60 fps frame is 16.7 ms.
    EXPECT_LT(ins_us, 50.0);
    EXPECT_LT(rel_us, 100.0);
#endif

    // Heap per instance: an empty timeline, the steady state after 10 s, and
    // the cost of each frame held while a missing frame blocks the chain.
    webrtc::SimulatedClock clock(Timestamp::Micros(kStartUs));
    SpikeFieldTrials trials;
    const int64_t heap0 = g_live_heap_bytes.load();
    auto tl = std::make_unique<SpikeTimeline>(&clock, trials);
    const int64_t heap_empty = g_live_heap_bytes.load() - heap0;
    auto feed = [&](size_t i) {
        const AccessUnit& au = stream[i];
        if (au.arrival > clock.CurrentTime()) clock.AdvanceTime(au.arrival - clock.CurrentTime());
        tl->insert(au);
        while (tl->poll().kind == Decision::Kind::kRelease) {
        }
    };
    for (size_t i = 0; i < 600; ++i) feed(i);
    clock.AdvanceTime(TimeDelta::Millis(200));  // release the last frames
    while (tl->poll().kind == Decision::Kind::kRelease) {
    }
    const int64_t heap_steady = g_live_heap_bytes.load() - heap0;
    const size_t held_steady = tl->buffer().CurrentSize();
    constexpr size_t kHeld = 50;  // frame 600 is missing; 601..650 wait
    for (size_t i = 601; i <= 600 + kHeld; ++i) feed(i);
    const int64_t heap_gap = g_live_heap_bytes.load() - heap0;
    const size_t held_gap = tl->buffer().CurrentSize();
    tl.reset();
    const int64_t heap_after = g_live_heap_bytes.load() - heap0;
    const double per_frame = static_cast<double>(heap_gap - heap_steady) / static_cast<double>(held_gap - held_steady);
    std::printf("[video-timing-spike] ram: sizeof(SpikeTimeline)=%zu bytes, sizeof(SpikeFrame)=%zu bytes; heap "
                "empty=%.1f KiB, steady after 10 s=%.1f KiB (%zu frames held), with %zu frames held=%.1f KiB "
                "(%.0f bytes per held frame); leak after destroy=%lld bytes\n",
                sizeof(SpikeTimeline), sizeof(SpikeFrame), heap_empty / 1024.0, heap_steady / 1024.0, held_steady,
                held_gap, heap_gap / 1024.0, per_frame, static_cast<long long>(heap_after));
    EXPECT_EQ(heap_after, 0);
    EXPECT_EQ(held_steady, 0u);
    EXPECT_EQ(held_gap, kHeld);
    EXPECT_LT(heap_gap, 1024 * 1024);
}

}  // namespace
