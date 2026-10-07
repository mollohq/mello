#pragma once

#include <cstdint>
#include <deque>
#include <limits>
#include <vector>

namespace mello::audio {

// Voice encoder adaptation to the send leg (spec 10 section 4.6).
//
// Input: one receiver report per second from the SFU about the microphone
// track (packets expected and lost since the previous report, round trip,
// interarrival jitter). Output: OPUS_SET_PACKET_LOSS_PERC and the Opus
// bitrate (a rung of a ladder whose top is the channel bitrate).
//
// Two rules, two signals:
// - Loss drives the expected loss only. A lossy link with a flat delay is
//   not congested: a lower bitrate costs quality and does not lower the
//   loss, while more in-band FEC repairs it.
// - The ladder steps down only on congestion evidence: loss together with a
//   rising round trip or a rising jitter, the signs of a queue that the
//   sender's own rate fills.
//
// Pure logic with the time passed in, so a test drives it with a trace.
// Not thread safe: the owner serializes the calls.

/// Rungs of the bitrate ladder in kbps, top first. The channel bitrate
/// replaces the top: the ladder is the channel bitrate, then every rung
/// below it.
inline constexpr int kSendLadderKbps[] = {96, 64, 48, 32, 24};
inline constexpr int kSendMinKbps = 24;
inline constexpr int kSendMaxKbps = 96;
inline constexpr int kSendDefaultKbps = 64;

/// OPUS_SET_PACKET_LOSS_PERC bounds.
inline constexpr int kLossPercFloor = 5;
inline constexpr int kLossPercCap = 30;

class SendAdaptation {
public:
    /// Congestion loss: the loss over this window exceeds kDownLoss. The
    /// window never reaches back before the last step down, and the next
    /// step down needs kDownMinReports reports after it: one lossy second
    /// does not count twice.
    static constexpr int64_t kDownWindowMs = 2000;
    static constexpr double kDownLoss = 0.10;
    static constexpr int kDownMinReports = 2;
    /// A loss window needs this many expected packets (0.5 s of speech) to
    /// count; a speech gate pause gives no evidence either way.
    static constexpr uint32_t kMinExpected = 25;
    /// Step up one rung after this long with no congestion, when the time
    /// holds kMinExpectedUp expected packets (5 s of speech).
    static constexpr int64_t kCleanMs = 10000;
    static constexpr uint32_t kMinExpectedUp = 250;
    /// OPUS_SET_PACKET_LOSS_PERC follows the loss over this window.
    static constexpr int64_t kLossPercWindowMs = 4000;
    /// Round-trip inflation: the median of the last 3 s (at least 2
    /// samples) exceeds 2 x the median of the last 30 s (at least 5
    /// samples). Medians, because one stalled sender report doubles a
    /// single sample. The 30 s median counts as at least kRttBaseMinMs: a
    /// 2 ms LAN round trip that becomes 5 ms is no queue.
    static constexpr int64_t kShortWindowMs = 3000;
    static constexpr int kShortMinSamples = 2;
    static constexpr int64_t kBaseWindowMs = 30000;
    static constexpr int kBaseMinSamples = 5;
    static constexpr float kRttBaseMinMs = 50.0f;
    /// Jitter growth: the same test on the RFC 3550 jitter of the reports
    /// that carry packets. The 30 s median counts as at least
    /// kJitterBaseMinMs.
    static constexpr float kJitterBaseMinMs = 10.0f;
    /// Ineffective step: the loss over the kEffectiveWindowMs of reports
    /// after a step down must fall to kDownLoss or to kEffectiveFactor x the
    /// loss that caused the step. If not, the step goes back up one rung and
    /// the ladder holds for kHoldMs.
    static constexpr int64_t kEffectiveWindowMs = 4000;
    static constexpr double kEffectiveFactor = 0.5;
    static constexpr int64_t kHoldMs = 30000;

    explicit SendAdaptation(int max_kbps = kSendDefaultKbps);

    /// The channel bitrate: the top rung. Clamped to 24..96. At the top
    /// rung the bitrate moves to the new top. Below it, the bitrate moves to
    /// the highest rung at or below the current bitrate.
    void set_max_kbps(int kbps);

    /// Back to the top rung and the loss floor; forget the reports (a new
    /// voice session).
    void reset();

    /// One receiver report at `now_ms`: packets expected and lost since the
    /// previous report, the round trip in ms (< 0: unknown) and the
    /// interarrival jitter in ms.
    void on_report(int64_t now_ms, uint32_t expected, uint32_t lost, float rtt_ms,
                   float jitter_ms);

    int bitrate_kbps() const { return rungs_[rung_]; }
    int max_kbps() const { return rungs_.front(); }
    int loss_perc() const { return loss_perc_; }
    /// Rung index, 0 = top.
    int rung() const { return rung_; }
    const std::vector<int>& rungs() const { return rungs_; }
    /// Rung changes since construction.
    uint32_t rung_changes() const { return rung_changes_; }
    /// Loss over the step-down window at the last report, 0..1; -1 when the
    /// window had too few packets.
    double window_loss() const { return window_loss_; }
    /// True while the ladder holds after an ineffective step.
    bool holding(int64_t now_ms) const { return now_ms < hold_until_ms_; }

private:
    struct Report {
        int64_t t_ms;
        uint32_t expected;
        uint32_t lost;
    };
    struct Sample {
        int64_t t_ms;
        float value;
    };

    void build_ladder(int max_kbps);
    void change_rung(int rung, int64_t now_ms, const char* why);
    void restart_clean_time(int64_t now_ms);
    /// Loss over the reports after `from_ms`: expected and lost.
    void loss_after(int64_t from_ms, uint64_t& expected, uint64_t& lost) const;
    /// The short median exceeds 2 x max(the base median, `base_min`).
    static bool inflated(const std::deque<Sample>& samples, int64_t now_ms, float base_min,
                         float* short_out, float* base_out);

    std::vector<int> rungs_;
    int rung_ = 0;
    int loss_perc_ = kLossPercFloor;
    std::deque<Report> reports_;
    std::deque<Sample> rtt_;
    std::deque<Sample> jitter_;
    int64_t last_down_ms_ = std::numeric_limits<int64_t>::min();
    // Reports since the last step down.
    int reports_since_down_ = 0;
    // The last step down while its effect is checked: time and the loss
    // that caused it. check_step_ms_ = min: nothing to check.
    int64_t check_step_ms_ = std::numeric_limits<int64_t>::min();
    double check_step_loss_ = 0.0;
    int64_t hold_until_ms_ = std::numeric_limits<int64_t>::min();
    // Start of the clean time and the packets in it.
    int64_t clean_since_ms_ = std::numeric_limits<int64_t>::min();
    uint64_t clean_expected_ = 0;
    double window_loss_ = -1.0;
    uint32_t rung_changes_ = 0;
};

} // namespace mello::audio
