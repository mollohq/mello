#pragma once

// A/V sync of one stream viewer (streaming-quality stage 7, spec 12 section
// 9.4, spec 14 section 7).
//
// Both streams carry the host's capture time on one clock: the video RTP
// timestamp is the capture time of the frame on the 90 kHz clock, and each
// game-audio packet carries, in a header extension, the video RTP timestamp
// of the instant its audio was captured. So the viewer knows, for the audio
// it plays now, which video timestamp belongs to it.
//
// The rule is libwebrtc's StreamSynchronization with audio as the master:
// the offset (when the audio is heard minus when the video frame of the
// same capture instant is shown) is filtered, ignored inside a dead band of
// 30 ms, and corrected by half of it per decision in small steps. A late
// audio raises the receive timeline's minimum delay; a late video first
// lowers that minimum again, and only then delays the audio (NetEQ's
// minimum delay), because the video cannot be earlier than its own target.
//
// AvSync is the decision logic alone, with the measurements passed in.
// AvSyncInput connects it: the stream view gives the audio clock and the
// audio delay control, the receiver session runs it on its worker and
// applies the video minimum delay to its timeline.
//
// This header has no WebRTC include (C++17).

#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>

namespace mello::transport {

class VideoReceiveTimeline;

/// What the game audio plays now.
struct AudioSyncSample {
    /// The video RTP timestamp (90 kHz) of the instant the host captured the
    /// audio that is heard at `heard_at_us`.
    uint32_t video_rtp_timestamp = 0;
    /// Steady clock (mello_clock_now_us), microseconds.
    int64_t heard_at_us = 0;
};

class AvSync {
public:
    struct Config {
        /// libwebrtc kMinDeltaMs: a filtered offset under this changes
        /// nothing.
        double dead_band_ms = 30.0;
        /// libwebrtc kFilterLength.
        int filter_length = 4;
        /// The largest raise of the video minimum in one decision. libwebrtc
        /// allows 80 ms in one step; the stage 3 spike showed that a step of
        /// 80 ms holds 3 frames once. VCMTiming applies a raised minimum to
        /// the render time at once, so a step of 10 ms shows one frame 10 ms
        /// longer.
        double max_video_step_ms = 10.0;
        /// The largest change of the audio minimum, and of a lowered video
        /// minimum, in one decision. Neither moves a picture at once: NetEQ
        /// stretches the audio to its new delay over the next frames, and a
        /// lowered video minimum lets the timeline's delay fall at 100 ms/s.
        double max_audio_step_ms = 40.0;
        /// The receive timeline's maximum delay.
        double max_video_delay_ms = 150.0;
        /// The most the audio is delayed for a late video.
        double max_audio_delay_ms = 150.0;
        /// A measured offset over this is not a sync error (a stream that
        /// restarts, a clock that jumps): it is ignored.
        double max_offset_ms = 1000.0;
        /// The video minimum binds when the timeline's delay is at most this
        /// much over it.
        double binding_margin_ms = 5.0;
    };

    struct Decision {
        bool changed = false;
        /// The receive timeline's minimum delay, ms (0: none).
        double video_min_delay_ms = 0.0;
        /// NetEQ's minimum delay for the game audio, ms.
        int audio_min_delay_ms = 0;
    };

    AvSync();
    explicit AvSync(Config config);

    /// One measurement. `offset_ms`: the time the audio of one capture
    /// instant is heard minus the time the video of that instant is shown
    /// (positive: the audio is late). `video_delay_ms`: the timeline's
    /// current delay.
    Decision update(double offset_ms, double video_delay_ms);

    /// Forget the filter and both delays (the audio stopped).
    void reset();

    /// The filtered offset, ms; empty before the first measurement.
    std::optional<double> filtered_offset_ms() const { return filtered_; }
    double video_min_delay_ms() const { return video_min_ms_; }
    int audio_min_delay_ms() const { return static_cast<int>(audio_min_ms_ + 0.5); }

private:
    Config config_;
    std::optional<double> filtered_;
    double video_min_ms_ = 0.0;
    double audio_min_ms_ = 0.0;
};

class AvSyncInput {
public:
    /// The audio's clock: what it plays now, or empty when it plays nothing
    /// that can be mapped (no audio yet, paused, no extension).
    using AudioClockFn = std::function<std::optional<AudioSyncSample>()>;
    /// Sets the game audio's minimum delay, ms.
    using AudioDelayFn = std::function<void(int delay_ms)>;

    struct Stats {
        /// A measurement was made in the last two intervals.
        bool active = false;
        /// The last measured offset and the filtered one, ms (+: audio late).
        double offset_ms = 0.0;
        double filtered_offset_ms = 0.0;
        double video_min_delay_ms = 0.0;
        int audio_min_delay_ms = 0;
        uint64_t measurements = 0;
        uint64_t changes = 0;
    };

    /// One decision per `interval_us`.
    static constexpr int64_t kDefaultIntervalUs = 250'000;
    /// Without a measurement for this long, the delays go back to none.
    static constexpr int64_t kIdleResetUs = 3'000'000;

    AvSyncInput(AudioClockFn audio, AudioDelayFn set_audio_delay,
                AvSync::Config config = AvSync::Config{},
                int64_t interval_us = kDefaultIntervalUs);

    /// The receiver session's worker. Measures and decides when the interval
    /// has passed since the last decision. A new timeline (a new video
    /// session) gets the current video minimum at once.
    void tick(VideoReceiveTimeline& timeline, int64_t now_us);

    Stats stats() const;

private:
    void apply_locked(VideoReceiveTimeline& timeline, const AvSync::Decision& d);

    const AudioClockFn audio_;
    const AudioDelayFn set_audio_delay_;
    const int64_t interval_us_;

    mutable std::mutex mutex_;
    AvSync sync_;
    int64_t next_us_ = 0;
    int64_t last_measurement_us_ = 0;
    const VideoReceiveTimeline* applied_to_ = nullptr;
    Stats stats_;
};

} // namespace mello::transport
