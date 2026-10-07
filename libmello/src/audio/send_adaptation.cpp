#include "send_adaptation.hpp"

#include "../util/log.hpp"

#include <algorithm>
#include <cmath>

namespace mello::audio {

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
    last_down_ms_ = std::numeric_limits<int64_t>::min();
    reports_since_down_ = 0;
    clean_since_ms_ = std::numeric_limits<int64_t>::min();
    clean_expected_ = 0;
    clean_lost_ = 0;
    window_loss_ = -1.0;
}

float SendAdaptation::rtt_median_after(int64_t from_ms, int min_samples) const {
    std::vector<float> v;
    for (const auto& s : rtt_) {
        if (s.t_ms > from_ms) {
            v.push_back(s.rtt_ms);
        }
    }
    if (static_cast<int>(v.size()) < min_samples || v.empty()) {
        return -1.0f;
    }
    const size_t mid = v.size() / 2;
    std::nth_element(v.begin(), v.begin() + static_cast<std::ptrdiff_t>(mid), v.end());
    if (v.size() % 2 == 1) {
        return v[mid];
    }
    const float upper = v[mid];
    const float lower = *std::max_element(v.begin(), v.begin() + static_cast<std::ptrdiff_t>(mid));
    return (lower + upper) / 2.0f;
}

float SendAdaptation::rtt_base_ms() const {
    if (rtt_.empty()) {
        return -1.0f;
    }
    return rtt_median_after(rtt_.back().t_ms - kRttBaseWindowMs, 1);
}

void SendAdaptation::restart_clean_time(int64_t now_ms) {
    clean_since_ms_ = now_ms;
    clean_expected_ = 0;
    clean_lost_ = 0;
}

void SendAdaptation::change_rung(int rung, int64_t now_ms, const char* why, float rtt_ms) {
    const int from = bitrate_kbps();
    const bool down = rung > rung_;
    rung_ = rung;
    if (down) {
        last_down_ms_ = now_ms;
        reports_since_down_ = 0;
    }
    restart_clean_time(now_ms);
    ++rung_changes_;
    MELLO_LOG_INFO("send_adapt",
                   "bitrate %d -> %d kbps (%s): loss over 2 s %.1f %%, rtt %.0f ms "
                   "(30 s median %.0f ms), loss perc %d",
                   from, bitrate_kbps(), why, window_loss_ < 0.0 ? -1.0 : window_loss_ * 100.0,
                   rtt_ms, rtt_base_ms(), loss_perc_);
}

void SendAdaptation::on_report(int64_t now_ms, uint32_t expected, uint32_t lost, float rtt_ms) {
    lost = std::min(lost, expected);
    reports_.push_back({now_ms, expected, lost});
    const int64_t keep_ms = std::max(kDownWindowMs, kLossPercWindowMs);
    while (!reports_.empty() && reports_.front().t_ms <= now_ms - keep_ms) {
        reports_.pop_front();
    }
    if (rtt_ms >= 0.0f) {
        rtt_.push_back({now_ms, rtt_ms});
    }
    while (!rtt_.empty() && rtt_.front().t_ms <= now_ms - kRttBaseWindowMs) {
        rtt_.pop_front();
    }
    ++reports_since_down_;

    // Loss over a window: the reports after `from_ms`.
    auto window = [&](int64_t from_ms, uint64_t& sum_expected, uint64_t& sum_lost) {
        sum_expected = 0;
        sum_lost = 0;
        for (const auto& r : reports_) {
            if (r.t_ms > from_ms) {
                sum_expected += r.expected;
                sum_lost += r.lost;
            }
        }
    };

    // OPUS_SET_PACKET_LOSS_PERC: the loss over 4 s, rounded up, floor 5,
    // cap 30. A window without enough packets keeps the last value.
    uint64_t perc_expected = 0;
    uint64_t perc_lost = 0;
    window(now_ms - kLossPercWindowMs, perc_expected, perc_lost);
    if (perc_expected >= kMinExpected) {
        const double pct = 100.0 * static_cast<double>(perc_lost) /
                           static_cast<double>(perc_expected);
        const int want = static_cast<int>(std::ceil(pct - 1e-9));
        loss_perc_ = std::clamp(want, kLossPercFloor, kLossPercCap);
    }

    // Step-down window: the last 2 s, but never before the last step
    // down, so one lossy second does not step down twice.
    uint64_t down_expected = 0;
    uint64_t down_lost = 0;
    window(std::max(now_ms - kDownWindowMs, last_down_ms_), down_expected, down_lost);
    window_loss_ = down_expected >= kMinExpected
                       ? static_cast<double>(down_lost) / static_cast<double>(down_expected)
                       : -1.0;
    const bool loss_bad = window_loss_ > kDownLoss;
    const float rtt_short = rtt_median_after(now_ms - kRttShortWindowMs, kRttShortMinSamples);
    const float rtt_base = rtt_median_after(now_ms - kRttBaseWindowMs, kRttBaseMinSamples);
    const bool rtt_bad = rtt_short >= 0.0f && rtt_base >= 0.0f &&
                         rtt_short > 2.0f * std::max(rtt_base, kRttBaseMinMs);

    if (loss_bad || rtt_bad) {
        const int bottom = static_cast<int>(rungs_.size()) - 1;
        const bool fresh = last_down_ms_ == std::numeric_limits<int64_t>::min() ||
                           reports_since_down_ >= kDownMinReports;
        if (rung_ < bottom && fresh) {
            change_rung(rung_ + 1, now_ms, loss_bad ? "loss" : "rtt", rtt_ms);
        } else {
            // Not clean: at the bottom, or the evidence since the last step
            // is too short. The clean time starts again.
            restart_clean_time(now_ms);
        }
        return;
    }

    if (clean_since_ms_ == std::numeric_limits<int64_t>::min()) {
        clean_since_ms_ = now_ms;
    }
    clean_expected_ += expected;
    clean_lost_ += lost;
    if (rung_ > 0 && now_ms - clean_since_ms_ >= kCleanMs && clean_expected_ >= kMinExpectedUp) {
        if (static_cast<double>(clean_lost_) <=
            kCleanLoss * static_cast<double>(clean_expected_)) {
            change_rung(rung_ - 1, now_ms, "clean", rtt_ms);
        } else {
            // Sparse lossy reports that never made a 2 s window: count again.
            restart_clean_time(now_ms);
        }
    }
}

} // namespace mello::audio
