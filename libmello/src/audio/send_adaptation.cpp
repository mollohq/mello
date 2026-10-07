#include "send_adaptation.hpp"

#include "../util/log.hpp"

#include <algorithm>
#include <cmath>

namespace mello::audio {
namespace {

constexpr int64_t kNever = std::numeric_limits<int64_t>::min();

} // namespace

SendAdaptation::SendAdaptation(int max_kbps) {
    build_ladder(max_kbps);
}

void SendAdaptation::build_ladder(int max_kbps) {
    const int top = std::clamp(max_kbps, kSendMinKbps, kSendMaxKbps);
    rungs_.clear();
    rungs_.push_back(top);
    for (const int kbps : kSendLadderKbps) {
        if (kbps < top) {
            rungs_.push_back(kbps);
        }
    }
}

void SendAdaptation::set_max_kbps(int kbps) {
    const bool at_top = rung_ == 0;
    const int current = bitrate_kbps();
    const int old_top = max_kbps();
    build_ladder(kbps);
    // At the top: the new top. Stepped down: the highest rung at or below
    // the current bitrate, and the clean steps climb from there.
    rung_ = 0;
    while (!at_top && rung_ + 1 < static_cast<int>(rungs_.size()) && rungs_[rung_] > current) {
        ++rung_;
    }
    if (max_kbps() != old_top) {
        MELLO_LOG_INFO("send_adapt", "channel bitrate %d kbps: ladder top %d, now %d kbps",
                       kbps, max_kbps(), bitrate_kbps());
    }
}

void SendAdaptation::reset() {
    rung_ = 0;
    loss_perc_ = kLossPercFloor;
    reports_.clear();
    rtt_.clear();
    jitter_.clear();
    last_down_ms_ = kNever;
    reports_since_down_ = 0;
    check_step_ms_ = kNever;
    check_step_loss_ = 0.0;
    hold_until_ms_ = kNever;
    clean_since_ms_ = kNever;
    clean_expected_ = 0;
    window_loss_ = -1.0;
}

bool SendAdaptation::inflated(const std::deque<Sample>& samples, int64_t now_ms,
                              float base_min, float* short_out, float* base_out) {
    std::vector<float> recent;
    std::vector<float> all;
    for (const auto& s : samples) {
        all.push_back(s.value);
        if (s.t_ms > now_ms - kShortWindowMs) {
            recent.push_back(s.value);
        }
    }
    auto median = [](std::vector<float>& v) {
        const size_t mid = v.size() / 2;
        std::nth_element(v.begin(), v.begin() + static_cast<std::ptrdiff_t>(mid), v.end());
        if (v.size() % 2 == 1) {
            return v[mid];
        }
        const float upper = v[mid];
        const float lower =
            *std::max_element(v.begin(), v.begin() + static_cast<std::ptrdiff_t>(mid));
        return (lower + upper) / 2.0f;
    };
    if (static_cast<int>(recent.size()) < kShortMinSamples ||
        static_cast<int>(all.size()) < kBaseMinSamples) {
        return false;
    }
    const float short_median = median(recent);
    const float base_median = median(all);
    if (short_out) *short_out = short_median;
    if (base_out) *base_out = base_median;
    return short_median > 2.0f * std::max(base_median, base_min);
}

void SendAdaptation::loss_after(int64_t from_ms, uint64_t& expected, uint64_t& lost) const {
    expected = 0;
    lost = 0;
    for (const auto& r : reports_) {
        if (r.t_ms > from_ms) {
            expected += r.expected;
            lost += r.lost;
        }
    }
}

void SendAdaptation::restart_clean_time(int64_t now_ms) {
    clean_since_ms_ = now_ms;
    clean_expected_ = 0;
}

void SendAdaptation::change_rung(int rung, int64_t now_ms, const char* why) {
    const int from = bitrate_kbps();
    const bool down = rung > rung_;
    rung_ = rung;
    if (down) {
        last_down_ms_ = now_ms;
        reports_since_down_ = 0;
    }
    restart_clean_time(now_ms);
    ++rung_changes_;
    float rtt_short = -1.0f;
    float rtt_base = -1.0f;
    float jit_short = -1.0f;
    float jit_base = -1.0f;
    inflated(rtt_, now_ms, kRttBaseMinMs, &rtt_short, &rtt_base);
    inflated(jitter_, now_ms, kJitterBaseMinMs, &jit_short, &jit_base);
    MELLO_LOG_INFO("send_adapt",
                   "bitrate %d -> %d kbps (%s): loss over 2 s %.1f %%, rtt %.0f ms "
                   "(30 s median %.0f ms), jitter %.1f ms (30 s median %.1f ms), loss perc %d",
                   from, bitrate_kbps(), why, window_loss_ < 0.0 ? -1.0 : window_loss_ * 100.0,
                   rtt_short, rtt_base, jit_short, jit_base, loss_perc_);
}

void SendAdaptation::on_report(int64_t now_ms, uint32_t expected, uint32_t lost, float rtt_ms,
                               float jitter_ms) {
    lost = std::min(lost, expected);
    reports_.push_back({now_ms, expected, lost});
    const int64_t keep_ms = std::max({kDownWindowMs, kLossPercWindowMs, kEffectiveWindowMs});
    while (!reports_.empty() && reports_.front().t_ms <= now_ms - keep_ms) {
        reports_.pop_front();
    }
    if (rtt_ms >= 0.0f) {
        rtt_.push_back({now_ms, rtt_ms});
    }
    // The jitter of a report without packets is the last one again.
    if (expected > 0 && jitter_ms >= 0.0f) {
        jitter_.push_back({now_ms, jitter_ms});
    }
    for (auto* samples : {&rtt_, &jitter_}) {
        while (!samples->empty() && samples->front().t_ms <= now_ms - kBaseWindowMs) {
            samples->pop_front();
        }
    }
    ++reports_since_down_;

    // OPUS_SET_PACKET_LOSS_PERC: the loss over 4 s, rounded up, floor 5,
    // cap 30. A window without enough packets keeps the last value. Loss
    // of any cause raises it: in-band FEC repairs random loss too.
    uint64_t perc_expected = 0;
    uint64_t perc_lost = 0;
    loss_after(now_ms - kLossPercWindowMs, perc_expected, perc_lost);
    if (perc_expected >= kMinExpected) {
        const double pct = 100.0 * static_cast<double>(perc_lost) /
                           static_cast<double>(perc_expected);
        const int want = static_cast<int>(std::ceil(pct - 1e-9));
        loss_perc_ = std::clamp(want, kLossPercFloor, kLossPercCap);
    }

    // Congestion loss: the last 2 s, but never before the last step down.
    uint64_t down_expected = 0;
    uint64_t down_lost = 0;
    loss_after(std::max(now_ms - kDownWindowMs, last_down_ms_), down_expected, down_lost);
    window_loss_ = down_expected >= kMinExpected
                       ? static_cast<double>(down_lost) / static_cast<double>(down_expected)
                       : -1.0;

    // Was the last step down effective? It must lower the loss within 4 s.
    if (check_step_ms_ != kNever && now_ms - check_step_ms_ >= kEffectiveWindowMs) {
        uint64_t after_expected = 0;
        uint64_t after_lost = 0;
        loss_after(check_step_ms_, after_expected, after_lost);
        if (after_expected >= kMinExpected) {
            const double after = static_cast<double>(after_lost) /
                                 static_cast<double>(after_expected);
            const bool effective =
                after <= kDownLoss || after <= kEffectiveFactor * check_step_loss_;
            check_step_ms_ = kNever;
            if (!effective) {
                hold_until_ms_ = now_ms + kHoldMs;
                MELLO_LOG_INFO("send_adapt",
                               "step down did not lower the loss (%.1f %% before, %.1f %% "
                               "after): back one rung, ladder held for %lld s",
                               check_step_loss_ * 100.0, after * 100.0,
                               static_cast<long long>(kHoldMs / 1000));
                if (rung_ > 0) {
                    change_rung(rung_ - 1, now_ms, "ineffective step");
                }
                restart_clean_time(now_ms);
                return;
            }
        }
    }

    const bool loss_bad = window_loss_ > kDownLoss;
    const bool rtt_bad = inflated(rtt_, now_ms, kRttBaseMinMs, nullptr, nullptr);
    const bool jitter_bad = inflated(jitter_, now_ms, kJitterBaseMinMs, nullptr, nullptr);
    const bool congested = loss_bad && (rtt_bad || jitter_bad);
    const bool held = holding(now_ms);

    if (congested) {
        const int bottom = static_cast<int>(rungs_.size()) - 1;
        const bool fresh = last_down_ms_ == kNever || reports_since_down_ >= kDownMinReports;
        if (rung_ < bottom && fresh && !held) {
            check_step_ms_ = now_ms;
            check_step_loss_ = window_loss_;
            change_rung(rung_ + 1, now_ms, rtt_bad ? "loss and rtt" : "loss and jitter");
        }
        // Not clean: the clean time starts again.
        restart_clean_time(now_ms);
        return;
    }

    if (clean_since_ms_ == kNever) {
        clean_since_ms_ = now_ms;
    }
    clean_expected_ += expected;
    if (rung_ > 0 && !held && now_ms - clean_since_ms_ >= kCleanMs &&
        clean_expected_ >= kMinExpectedUp) {
        change_rung(rung_ - 1, now_ms, "clean");
    }
}

} // namespace mello::audio
