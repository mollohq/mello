#pragma once

#include <cstdint>
#include <deque>
#include <limits>
#include <vector>

namespace mello::audio {

// Voice encoder adaptation to the send leg (spec 10 section 4.6).
//
// Input: one receiver report per second from the SFU about the microphone
// track (packets expected and lost since the previous report, round trip).
// Output: the Opus bitrate (a rung of a ladder whose top is the channel
// bitrate) and OPUS_SET_PACKET_LOSS_PERC.
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
    /// Step down when the loss over this window exceeds kDownLoss.
    static constexpr int64_t kDownWindowMs = 2000;
    static constexpr double kDownLoss = 0.10;
    /// Step up one rung after this long with no step down and a loss of at
    /// most kCleanLoss over the time.
    static constexpr int64_t kCleanMs = 10000;
    static constexpr double kCleanLoss = 0.05;
    /// A loss window needs this many expected packets (0.5 s of speech) to
    /// count; a speech gate pause gives no evidence either way.
    static constexpr uint32_t kMinExpected = 25;
    /// A step up needs this many expected packets (5 s of speech) in the
    /// clean time.
    static constexpr uint32_t kMinExpectedUp = 250;
    /// OPUS_SET_PACKET_LOSS_PERC follows the loss over this window.
    static constexpr int64_t kLossPercWindowMs = 4000;
    /// The round trip "doubles" when a sample exceeds 2 x the floor, and the
    /// floor counts as at least kRttFloorMinMs (a 2 ms LAN round trip that
    /// becomes 5 ms is no congestion).
    static constexpr int64_t kRttFloorWindowMs = 30000;
    static constexpr float kRttFloorMinMs = 50.0f;
    static constexpr int kRttMinSamples = 3;

    explicit SendAdaptation(int max_kbps = kSendDefaultKbps);

    /// The channel bitrate: the top rung. Clamped to 24..96. At the top
    /// rung the bitrate moves to the new top. Below it, the bitrate moves to
    /// the highest rung at or below the current bitrate.
    void set_max_kbps(int kbps);

    /// Back to the top rung and the loss floor; forget the reports (a new
    /// voice session).
    void reset();

    /// One receiver report at `now_ms`: packets expected and lost since the
    /// previous report, and the round trip in ms (< 0: unknown).
    void on_report(int64_t now_ms, uint32_t expected, uint32_t lost, float rtt_ms);

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
    /// Lowest round trip of the last 30 s, ms; -1 with no sample.
    float rtt_floor_ms() const;

private:
    struct Report {
        int64_t t_ms;
        uint32_t expected;
        uint32_t lost;
    };
    struct RttSample {
        int64_t t_ms;
        float rtt_ms;
    };

    void build_ladder(int max_kbps);
    void change_rung(int rung, int64_t now_ms, const char* why, float rtt_ms);

    std::vector<int> rungs_;
    int rung_ = 0;
    int loss_perc_ = kLossPercFloor;
    std::deque<Report> reports_;
    std::deque<RttSample> rtt_;
    int64_t last_change_ms_ = std::numeric_limits<int64_t>::min();
    // Start of the clean time and the packets in it.
    int64_t clean_since_ms_ = std::numeric_limits<int64_t>::min();
    uint64_t clean_expected_ = 0;
    uint64_t clean_lost_ = 0;
    double window_loss_ = -1.0;
    uint32_t rung_changes_ = 0;
};

} // namespace mello::audio
