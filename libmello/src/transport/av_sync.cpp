#include "av_sync.hpp"

#include "video_receive_timeline.hpp"
#include "../util/log.hpp"

#include <algorithm>
#include <cmath>
#include <vector>

namespace mello::transport {

AvSync::AvSync() : AvSync(Config{}) {}

AvSync::AvSync(Config config) : config_(config) {}

AvSync::Decision AvSync::update(double offset_ms, double video_delay_ms) {
    Decision d;
    d.video_min_delay_ms = video_min_ms_;
    d.audio_min_delay_ms = audio_min_delay_ms();
    if (!std::isfinite(offset_ms) || std::abs(offset_ms) > config_.max_offset_ms) return d;

    const double n = static_cast<double>(std::max(1, config_.filter_length));
    filtered_ = filtered_ ? (*filtered_ * (n - 1.0) + offset_ms) / n : offset_ms;
    if (std::abs(*filtered_) < config_.dead_band_ms) return d;

    // The filtered offset, one kind of change per decision, each with its
    // step limit. libwebrtc moves half of it; the measurement here already
    // counts the delay NetEQ and the timeline still have to reach (the
    // pending delays), so a whole step does not overshoot.
    const double want = *filtered_;
    const double video_before = video_min_ms_;
    const double audio_before = audio_min_ms_;
    if (want > 0.0) {
        // The audio is late. First take back delay the audio got for a late
        // video, then hold the video longer.
        if (audio_min_ms_ > 0.0) {
            audio_min_ms_ -= std::min({audio_min_ms_, want, config_.max_audio_step_ms});
        } else {
            // From the delay the frames get now: a minimum under it does not
            // bind and would change nothing.
            const double base = std::max(video_min_ms_, video_delay_ms);
            video_min_ms_ = std::min(base + std::min(want, config_.max_video_step_ms),
                                     config_.max_video_delay_ms);
        }
    } else {
        // The video is late. Lower the video minimum while it binds; the
        // video cannot be earlier than its own target, so then delay the
        // audio.
        const double s = std::min(-want, config_.max_audio_step_ms);
        const bool binding =
            video_min_ms_ > 0.0 && video_delay_ms <= video_min_ms_ + config_.binding_margin_ms;
        if (binding) {
            video_min_ms_ = std::max(0.0, video_min_ms_ - s);
        } else {
            audio_min_ms_ = std::min(audio_min_ms_ + s, config_.max_audio_delay_ms);
        }
    }
    // The applied change is already on its way: take it out of the filter,
    // or the old measurements in it ask for the same change again.
    const double applied = (video_min_ms_ - video_before) - (audio_min_ms_ - audio_before);
    *filtered_ -= applied;

    d.video_min_delay_ms = video_min_ms_;
    d.audio_min_delay_ms = audio_min_delay_ms();
    d.changed = std::abs(video_min_ms_ - video_before) >= 0.5 ||
                std::abs(audio_min_ms_ - audio_before) >= 0.5;
    return d;
}

void AvSync::reset() {
    filtered_.reset();
    video_min_ms_ = 0.0;
    audio_min_ms_ = 0.0;
}

void AvSync::start_audio(double audio_min_delay_ms) {
    filtered_.reset();
    audio_min_ms_ = std::clamp(audio_min_delay_ms, 0.0, config_.max_audio_delay_ms);
}

AvSyncInput::AvSyncInput(AudioClockFn audio, AudioDelayFn set_audio_delay, AvSync::Config config,
                         int64_t interval_us)
    : audio_(std::move(audio)),
      set_audio_delay_(std::move(set_audio_delay)),
      interval_us_(std::max<int64_t>(interval_us, 1)),
      sync_(config),
      max_audio_delay_ms_(config.max_audio_delay_ms) {}

void AvSyncInput::apply_locked(VideoReceiveTimeline& timeline, const AvSync::Decision& d) {
    // The timeline's own floor is 1 ms (spec 14 section 7.2).
    timeline.set_min_delay_us(
        std::max<int64_t>(1'000, static_cast<int64_t>(d.video_min_delay_ms * 1000.0 + 0.5)));
    if (set_audio_delay_) set_audio_delay_(d.audio_min_delay_ms);
    stats_.video_min_delay_ms = d.video_min_delay_ms;
    stats_.audio_min_delay_ms = d.audio_min_delay_ms;
}

std::optional<int> AvSyncInput::start_buffer_ms(uint32_t video_rtp_timestamp,
                                                int64_t heard_if_now_us) {
    std::lock_guard<std::mutex> lock(mutex_);
    const auto timeline = timeline_.lock();
    if (!timeline) return std::nullopt;
    const auto render_us = timeline->render_time_us(video_rtp_timestamp);
    if (!render_us) return std::nullopt;
    // Where the video goes: its target, not the delay frames get now (the
    // timeline's delay climbs at 100 ms/s while its jitter estimate settles,
    // and the audio could follow only by stretching).
    const TimelineStats ts = timeline->stats();
    const double video_applied_ms =
        std::clamp(ts.current_delay_ms, std::max(1.0, sync_.video_min_delay_ms()), 150.0);
    const double want_ms = static_cast<double>(*render_us - heard_if_now_us) / 1000.0 +
                           lateness_ms_ + (ts.target_delay_ms - video_applied_ms);
    const int buffer_ms = static_cast<int>(std::clamp(want_ms, 0.0, max_audio_delay_ms_) + 0.5);
    sync_.start_audio(buffer_ms);
    stats_.audio_min_delay_ms = sync_.audio_min_delay_ms();
    MELLO_LOG_INFO("av_sync", "audio starts with a %d ms buffer (the video of its first packet "
                   "is shown %.1f ms after it could be heard)", buffer_ms, want_ms);
    return buffer_ms;
}

void AvSyncInput::tick(const std::shared_ptr<VideoReceiveTimeline>& timeline_ptr, int64_t now_us) {
    if (!timeline_ptr) return;
    VideoReceiveTimeline& timeline = *timeline_ptr;
    std::lock_guard<std::mutex> lock(mutex_);
    timeline_ = timeline_ptr;
    if (applied_to_ != &timeline) {
        // A new video session: its timeline starts without our minimum.
        applied_to_ = &timeline;
        if (sync_.video_min_delay_ms() > 0.0) {
            AvSync::Decision d;
            d.video_min_delay_ms = sync_.video_min_delay_ms();
            d.audio_min_delay_ms = sync_.audio_min_delay_ms();
            apply_locked(timeline, d);
        }
    }
    if (now_us < next_us_) return;
    next_us_ = now_us + interval_us_;

    if (video_lateness_) {
        const double l = video_lateness_();
        if (std::isfinite(l)) {
            lateness_samples_.push_back(l);
            if (lateness_samples_.size() > 16) lateness_samples_.pop_front();
            std::vector<double> sorted(lateness_samples_.begin(), lateness_samples_.end());
            std::nth_element(sorted.begin(), sorted.begin() + sorted.size() / 2, sorted.end());
            lateness_ms_ = std::clamp(sorted[sorted.size() / 2], -20.0, 60.0);
            stats_.video_lateness_ms = lateness_ms_;
        }
    }
    const std::optional<AudioSyncSample> sample = audio_ ? audio_() : std::nullopt;
    std::optional<int64_t> shown_us;
    if (sample) shown_us = timeline.render_time_us(sample->video_rtp_timestamp);
    if (!sample || !shown_us) {
        stats_.active = last_measurement_us_ != 0 && now_us - last_measurement_us_ < 2 * interval_us_;
        if (last_measurement_us_ != 0 && now_us - last_measurement_us_ >= kIdleResetUs &&
            (sync_.video_min_delay_ms() > 0.0 || sync_.audio_min_delay_ms() > 0)) {
            // The audio stopped (a pause, a stream without audio): no reason
            // to hold the video.
            sync_.reset();
            apply_locked(timeline, AvSync::Decision{});
            MELLO_LOG_INFO("av_sync", "no audio for %lld ms: delays reset",
                           static_cast<long long>((now_us - last_measurement_us_) / 1000));
        }
        return;
    }

    // The timeline's target: its own delay or the minimum, whichever is
    // larger. The current delay lags a changed minimum (100 ms/s), and a
    // decision on it would count a minimum that was just lowered as one that
    // does not bind.
    const TimelineStats ts = timeline.stats();
    const double video_delay_ms = ts.target_delay_ms;
    // The offset when both delays have reached their targets: the audio
    // gains its pending delay; the video moves from the delay frames get now
    // (VCMTiming's current delay within the bounds) to its target.
    const double video_applied_ms =
        std::clamp(ts.current_delay_ms, std::max(1.0, sync_.video_min_delay_ms()), 150.0);
    const double offset_ms = static_cast<double>(sample->heard_at_us - *shown_us) / 1000.0 -
                             lateness_ms_ + sample->pending_ms -
                             (video_delay_ms - video_applied_ms);
    const AvSync::Decision d = sync_.update(offset_ms, video_delay_ms);
    last_measurement_us_ = now_us;
    ++stats_.measurements;
    stats_.active = true;
    stats_.offset_ms = offset_ms;
    stats_.filtered_offset_ms = sync_.filtered_offset_ms().value_or(offset_ms);
    if (stats_.measurements <= 3 || (stats_.measurements % 120) == 0) {
        MELLO_LOG_INFO("av_sync", "offset %.1f ms (filtered %.1f), video delay %.1f ms, "
                       "video min %.1f ms, audio min %d ms",
                       offset_ms, stats_.filtered_offset_ms, video_delay_ms,
                       sync_.video_min_delay_ms(), sync_.audio_min_delay_ms());
    }
    if (d.changed) {
        ++stats_.changes;
        apply_locked(timeline, d);
    }
}

void AvSyncInput::set_video_lateness(VideoLatenessFn fn) {
    std::lock_guard<std::mutex> lock(mutex_);
    video_lateness_ = std::move(fn);
}

AvSyncInput::Stats AvSyncInput::stats() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return stats_;
}

} // namespace mello::transport
