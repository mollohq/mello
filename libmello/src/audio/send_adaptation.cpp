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
    build_ladder(kbps);
    // At the top: the new top. Stepped down: the highest rung at or below
    // the current bitrate, and the clean steps climb from there.
    rung_ = 0;
    while (!at_top && rung_ + 1 < static_cast<int>(rungs_.size()) && rungs_[rung_] > current) {
        ++rung_;
    }
    MELLO_LOG_INFO("send_adapt", "channel bitrate %d kbps: ladder top %d, now %d kbps",
                   kbps, rungs_.front(), bitrate_kbps());
}

void SendAdaptation::reset() {
    rung_ = 0;
    loss_perc_ = kLossPercFloor;
    reports_.clear();
    rtt_.clear();
    last_change_ms_ = std::numeric_limits<int64_t>::min();
    clean_since_ms_ = std::numeric_limits<int64_t>::min();
    clean_expected_ = 0;
    clean_lost_ = 0;
    window_loss_ = -1.0;
}

float SendAdaptation::rtt_floor_ms() const {
    if (rtt_.empty()) {
        return -1.0f;
    }
    float floor = rtt_.front().rtt_ms;
    for (const auto& s : rtt_) {
        floor = std::min(floor, s.rtt_ms);
    }
    return floor;
}

void SendAdaptation::change_rung(int rung, int64_t now_ms, const char* why, float rtt_ms) {
    const int from = bitrate_kbps();
    rung_ = rung;
    last_change_ms_ = now_ms;
    clean_since_ms_ = now_ms;
    clean_expected_ = 0;
    clean_lost_ = 0;
    ++rung_changes_;
    MELLO_LOG_INFO("send_adapt",
                   "bitrate %d -> %d kbps (%s): loss over 2 s %.1f %%, rtt %.0f ms "
                   "(floor %.0f ms), loss perc %d",
                   from, bitrate_kbps(), why, window_loss_ < 0.0 ? -1.0 : window_loss_ * 100.0,
                   rtt_ms, rtt_floor_ms(), loss_perc_);
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
    while (!rtt_.empty() && rtt_.front().t_ms <= now_ms - kRttFloorWindowMs) {
        rtt_.pop_front();
    }

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

    // Step-down window: the last 2 s, but never before the last rung
    // change, so one lossy second does not step down twice.
    uint64_t down_expected = 0;
    uint64_t down_lost = 0;
    window(std::max(now_ms - kDownWindowMs, last_change_ms_), down_expected, down_lost);
    window_loss_ = down_expected >= kMinExpected
                       ? static_cast<double>(down_lost) / static_cast<double>(down_expected)
                       : -1.0;
    const bool loss_bad = window_loss_ > kDownLoss;
    const float floor = rtt_floor_ms();
    const bool rtt_bad = rtt_ms >= 0.0f && static_cast<int>(rtt_.size()) >= kRttMinSamples &&
                         rtt_ms > 2.0f * std::max(floor, kRttFloorMinMs);

    const int bottom = static_cast<int>(rungs_.size()) - 1;
    if (loss_bad || rtt_bad) {
        if (rung_ < bottom) {
            change_rung(rung_ + 1, now_ms, loss_bad ? "loss" : "rtt", rtt_ms);
        } else {
            // At the bottom: the clean time starts again.
            clean_since_ms_ = now_ms;
            clean_expected_ = 0;
            clean_lost_ = 0;
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
            // Loss between the thresholds: hold the rung, count again.
            clean_since_ms_ = now_ms;
            clean_expected_ = 0;
            clean_lost_ = 0;
        }
    }
}

} // namespace mello::audio
