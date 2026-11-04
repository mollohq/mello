// The game clip player (CLIPS.md 15.9), the same on every platform: m3llo's
// own MP4 demux over the read callback, libopus for the sound, and the
// stream viewer's decoder and present threads for the picture. The sound is
// the clock. Only the graphics device and the decoder (the viewer's factory)
// differ per platform.

#include "clip_player.hpp"
#include "../replay/mp4_demux.hpp"
#include "../video/default_device.hpp"
#include "../util/log.hpp"

#include <opus.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <deque>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <vector>

namespace mello::clip {

using replay::Mp4Movie;
using replay::Mp4Sample;
using replay::Mp4Track;
using transport::TimelineDecision;
using video::ViewerPipeline;

namespace {

constexpr const char* TAG = "clip/player";

/// A frame leaves the source this long before its render time: one decode,
/// and no more, or the 3-slot ring evicts frames that wait.
constexpr int64_t kDecodeLeadUs = 25'000;
/// How often a source with nothing to release looks again.
constexpr int64_t kIdlePollUs = 10'000;
/// Sound that must wait before play starts (0.3 s) and goes on after a stall
/// (1 s, CLIPS.md 15.9); under 50 ms of it is a stall.
constexpr size_t kStartFrames = audio::ClipAudioStream::kSampleRate * 3 / 10;
constexpr size_t kResumeFrames = audio::ClipAudioStream::kSampleRate;
constexpr size_t kStarveFrames = audio::ClipAudioStream::kSampleRate / 20;
/// Demuxed frames that may wait: 4 s at 60 fps, 10 s while the sound starves.
constexpr size_t kFrameCap = 240;
constexpr size_t kFrameCapStarving = 600;
/// A read that waits for bytes asks again this often.
constexpr auto kReadRetry = std::chrono::milliseconds(15);
/// One read call asks for at most this much (the range reader's range).
constexpr size_t kReadBlock = 1 << 20;
/// Opus frames before a seek target that the decoder needs (80 ms of 20 ms
/// frames, the Opus encapsulation spec), when the file has no roll group.
constexpr int32_t kOpusPreRoll = 4;
constexpr int kOpusMaxFrame = 5760;  // 120 ms at 48 kHz

int64_t now_us() { return ViewerPipeline::now_us(); }

struct OpusDecoderDeleter {
    void operator()(OpusDecoder* d) const { opus_decoder_destroy(d); }
};

// ---- Frame source -----------------------------------------------------------

struct DemuxedFrame {
    std::vector<uint8_t> annex_b;
    int64_t pts_us = 0;
    bool keyframe = false;
};

/// The demuxed frames, released to the viewer's decode thread at their
/// render times. The player says the render time of a frame's time, or
/// nothing while the picture holds.
class ClipFrameSource : public transport::FrameSource {
public:
    /// `first`: the first frame since the start or a seek. It shows at once,
    /// so the picture never waits for the sound.
    using RenderTimeFn = std::function<std::optional<int64_t>(int64_t pts_us, int64_t now, bool first)>;

    explicit ClipFrameSource(RenderTimeFn render_time) : render_time_(std::move(render_time)) {}

    TimelineDecision poll() override {
        TimelineDecision d;
        const int64_t now = now_us();
        std::unique_lock<std::mutex> lock(mutex_);
        if (stopped_ || frames_.empty()) {
            d.kind = TimelineDecision::Kind::Wait;
            d.wake_us = now + kIdlePollUs;
            return d;
        }
        const int64_t pts = frames_.front().pts_us;
        const uint64_t clears = clears_;
        const bool first = released_ == 0;
        lock.unlock();
        const std::optional<int64_t> render = render_time_(pts, now, first);
        lock.lock();
        // A seek cleared the queue meanwhile: this time is not for its front.
        if (frames_.empty() || !render || clears_ != clears) {
            d.kind = TimelineDecision::Kind::Wait;
            d.wake_us = now + kIdlePollUs;
            return d;
        }
        if (*render - now > kDecodeLeadUs) {
            d.kind = TimelineDecision::Kind::Wait;
            d.wake_us = std::min(*render - kDecodeLeadUs, now + 100'000);
            return d;
        }
        DemuxedFrame f = std::move(frames_.front());
        frames_.pop_front();
        ++released_;
        last_released_pts_ = f.pts_us;
        lock.unlock();
        space_cv_.notify_all();
        d.kind = TimelineDecision::Kind::Release;
        d.frame.annex_b = std::move(f.annex_b);
        d.frame.keyframe = f.keyframe;
        d.frame.render_time_us = *render;
        d.frame.released_us = now;
        return d;
    }
    void on_decoded(int64_t) override {}
    void on_decode_error() override { decode_errors_.fetch_add(1); }
    bool wait(int64_t wake_us) override {
        std::unique_lock<std::mutex> lock(mutex_);
        const int64_t now = now_us();
        const int64_t until = wake_us > 0 ? wake_us : now + 100'000;
        if (until > now && !stopped_) {
            frame_cv_.wait_for(lock, std::chrono::microseconds(until - now));
        }
        return !stopped_;
    }
    void wake() override { frame_cv_.notify_all(); }
    transport::TimelineStats stats() const override {
        transport::TimelineStats s;
        std::lock_guard<std::mutex> lock(mutex_);
        s.frames_released = released_total_ + released_;
        s.frames_buffered = frames_.size();
        return s;
    }

    // ---- Demux thread ----

    /// Waits while `cap` frames wait, then adds the frame. False when
    /// `cancel` turned true while it waited.
    bool push(DemuxedFrame frame, const std::function<size_t()>& cap, const std::function<bool()>& cancel) {
        std::unique_lock<std::mutex> lock(mutex_);
        while (frames_.size() >= cap()) {
            if (cancel()) return false;
            space_cv_.wait_for(lock, std::chrono::milliseconds(10));
        }
        frames_.push_back(std::move(frame));
        lock.unlock();
        frame_cv_.notify_all();
        return true;
    }
    /// A seek: the frames that wait are gone, and the next one is a first.
    void clear() {
        std::lock_guard<std::mutex> lock(mutex_);
        frames_.clear();
        released_total_ += released_;
        released_ = 0;
        ++clears_;
        space_cv_.notify_all();
    }
    void stop() {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            stopped_ = true;
        }
        frame_cv_.notify_all();
        space_cv_.notify_all();
    }
    void wake_pusher() { space_cv_.notify_all(); }

    size_t waiting() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return frames_.size();
    }
    int64_t last_released_pts() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return last_released_pts_;
    }
    uint64_t decode_errors() const { return decode_errors_.load(); }

private:
    RenderTimeFn render_time_;
    mutable std::mutex mutex_;
    std::condition_variable frame_cv_;
    std::condition_variable space_cv_;
    std::deque<DemuxedFrame> frames_;
    uint64_t released_ = 0;        // since the last clear
    uint64_t released_total_ = 0;  // before it
    uint64_t clears_ = 0;
    int64_t last_released_pts_ = 0;
    bool stopped_ = false;
    std::atomic<uint64_t> decode_errors_{0};
};

// ---- Player -----------------------------------------------------------------

class Player final : public ClipPlayer {
public:
    explicit Player(ClipPlayerOptions options) : options_(std::move(options)) {
        audio_ = std::make_shared<audio::ClipAudioStream>();
        source_ = std::make_shared<ClipFrameSource>(
            [this](int64_t pts, int64_t now, bool first) { return render_time(pts, now, first); });
        thread_ = std::thread([this] { run(); });
    }
    ~Player() override { stop(); }

    void set_native_frame_callback(ViewerPipeline::NativeFrameCallback cb) override {
        std::lock_guard<std::mutex> lock(viewer_mutex_);
        native_cb_ = cb;
        if (viewer_) viewer_->set_native_frame_callback(std::move(cb));
    }

    void set_frame_callback(ViewerPipeline::FrameCallback cb) override {
        std::lock_guard<std::mutex> lock(frame_cb_mutex_);
        frame_cb_ = std::move(cb);
    }

    void set_paused(bool paused) override {
        std::lock_guard<std::mutex> lock(clock_mutex_);
        if (paused == paused_) return;
        const int64_t now = now_us();
        if (!has_audio_ || audio_done_) {
            wall_pos_ = position_locked(now);
            wall_at_ = now;
        }
        paused_ = paused;
        apply_audio_pause_locked();
        MELLO_LOG_INFO(TAG, "%s at %.2f s", paused ? "paused" : "playing", position_locked(now) / 1e6);
    }

    void seek(int64_t position_us) override {
        seek_to_.store(std::max<int64_t>(0, position_us));
        seek_pending_.store(true, std::memory_order_release);
        source_->wake_pusher();
    }

    ClipPlayerState state() const override {
        ClipPlayerState s;
        const int64_t now = now_us();
        std::lock_guard<std::mutex> lock(clock_mutex_);
        s.duration_us = duration_us_.load();
        s.position_us = std::clamp<int64_t>(position_locked(now), 0, std::max<int64_t>(s.duration_us, 0));
        s.width = width_.load();
        s.height = height_.load();
        s.paused = paused_;
        s.buffering = reports_buffering(buffering_, has_audio_, waiting_.load(), ended_locked(now));
        s.ended = ended_locked(now);
        s.failed = failed_.load();
        return s;
    }

    void stop() override {
        if (stopped_.exchange(true)) return;
        stop_flag_.store(true, std::memory_order_release);
        source_->stop();
        if (thread_.joinable()) thread_.join();
    }

private:
    // ---- Clock (clock_mutex_) ----

    int64_t position_locked(int64_t now) const {
        if (!has_audio_ || audio_done_) {
            return wall_pos_ + (paused_ || buffering_ ? 0 : now - wall_at_);
        }
        return audio_position(now);
    }

    int64_t audio_position(int64_t now) const {
        const int64_t played =
            static_cast<int64_t>(audio_->played_frames() * 1'000'000 / audio::ClipAudioStream::kSampleRate);
        int64_t since = 0;
        const int64_t mixed_at = audio_->last_mix_us();
        if (!audio_->paused() && audio_->buffered_frames() > 0 && mixed_at > 0) {
            since = std::clamp<int64_t>(now - mixed_at, 0, 20'000);
        }
        return played + since - latency_us_;
    }

    bool ended_locked(int64_t now) const {
        return demux_ended_.load() && source_->waiting() == 0 && (!has_audio_ || audio_->buffered_frames() == 0) &&
               position_locked(now) >= source_->last_released_pts();
    }

    void apply_audio_pause_locked() { audio_->set_paused(paused_ || buffering_); }

    /// The decode thread asks this before each release: it also runs the
    /// stall rule and the change to the wall clock when the sound ends.
    std::optional<int64_t> render_time(int64_t pts, int64_t now, bool first) {
        std::lock_guard<std::mutex> lock(clock_mutex_);
        if (has_audio_ && !audio_done_ && audio_->ended() && audio_->buffered_frames() == 0) {
            // The sound is over: the last frames follow the wall clock.
            wall_pos_ = audio_position(now);
            wall_at_ = now;
            audio_done_ = true;
        }
        if (has_audio_ && !audio_done_) {
            const size_t buffered = audio_->buffered_frames();
            const bool eos = demux_ended_.load() || audio_->ended();
            if (!buffering_ && started_ && buffered < kStarveFrames && !eos && !paused_) {
                buffering_ = true;
                MELLO_LOG_INFO(TAG, "buffering at %.2f s", audio_position(now) / 1e6);
                apply_audio_pause_locked();
            } else if (buffering_ && (buffered >= (started_ ? kResumeFrames : kStartFrames) || eos)) {
                buffering_ = false;
                if (started_) MELLO_LOG_INFO(TAG, "playing again at %.2f s", audio_position(now) / 1e6);
                started_ = true;
                apply_audio_pause_locked();
            }
        } else if (buffering_) {
            buffering_ = false;
            started_ = true;
            wall_at_ = now;
        }
        if (first) return now;
        if (paused_ || buffering_) return std::nullopt;
        return now + (pts - position_locked(now));
    }

    // ---- Reads (demux thread) ----

    bool cancelled(bool by_seek) const {
        return stop_flag_.load(std::memory_order_acquire) ||
               (by_seek && seek_pending_.load(std::memory_order_acquire));
    }

    /// Reads exactly `len` bytes. A read that waits asks again every 15 ms;
    /// `waiting_` says so for the buffering rule. False on an error, the end
    /// of the file, the stop, and (when `by_seek`) a seek that waits.
    bool read_at(uint64_t offset, uint8_t* buf, size_t len, bool by_seek) {
        size_t done = 0;
        while (done < len) {
            if (cancelled(by_seek)) {
                waiting_.store(false);
                return false;
            }
            const auto ask = static_cast<uint32_t>(std::min(len - done, kReadBlock));
            const int64_t n = options_.read(offset + done, buf + done, ask);
            if (n == kReadWait) {
                waiting_.store(true);
                std::this_thread::sleep_for(kReadRetry);
                continue;
            }
            waiting_.store(false);
            if (n <= 0 || static_cast<uint64_t>(n) > ask) {
                if (!cancelled(by_seek)) {
                    MELLO_LOG_ERROR(TAG, "a read at %llu failed: %lld", static_cast<unsigned long long>(offset + done),
                                    static_cast<long long>(n));
                    read_failed_ = true;
                }
                return false;
            }
            done += static_cast<size_t>(n);
        }
        return true;
    }

    // ---- Demux thread ----

    bool open() {
        replay::Mp4ReadAt read = [this](uint64_t offset, uint8_t* buf, size_t len) {
            return read_at(offset, buf, len, false);
        };
        std::string error;
        if (!replay::open_mp4(read, options_.length, movie_, &error)) {
            if (!stop_flag_.load()) MELLO_LOG_ERROR(TAG, "the clip does not open: %s", error.c_str());
            return false;
        }
        video_ = movie_.video();
        audio_track_ = movie_.audio();
        if (!video_ || video_->samples.empty()) {
            MELLO_LOG_ERROR(TAG, "the clip has no H.264 video");
            return false;
        }
        for (const auto& t : movie_.tracks) {
            if (t.kind == Mp4Track::Kind::Audio && !t.supported) {
                MELLO_LOG_WARN(TAG, "the clip's sound is not Opus (%c%c%c%c): no sound",
                               static_cast<char>(t.codec >> 24), static_cast<char>(t.codec >> 16),
                               static_cast<char>(t.codec >> 8), static_cast<char>(t.codec));
            }
        }
        if (audio_track_ && audio_track_->samples.empty()) audio_track_ = nullptr;
        if (audio_track_) {
            int err = 0;
            opus_.reset(opus_decoder_create(48000, 2, &err));
            if (!opus_ || err != OPUS_OK) {
                MELLO_LOG_WARN(TAG, "no Opus decoder (%d): no sound", err);
                opus_.reset();
                audio_track_ = nullptr;
            }
        }
        width_.store(video_->width);
        height_.store(video_->height);
        // The parameter sets go before the first key frame of each run, in
        // case the file keeps them only in the track header.
        static const uint8_t kStart[] = {0, 0, 0, 1};
        for (const auto& p : video_->sps) {
            header_.insert(header_.end(), kStart, kStart + 4);
            header_.insert(header_.end(), p.begin(), p.end());
        }
        for (const auto& p : video_->pps) {
            header_.insert(header_.end(), kStart, kStart + 4);
            header_.insert(header_.end(), p.begin(), p.end());
        }
        const Mp4Sample& last = video_->samples.back();
        const int64_t video_end = video_->time_us(last.time + last.duration);
        duration_us_.store(std::max<int64_t>(static_cast<int64_t>(movie_.duration_us), video_end));
        return true;
    }

    bool start_viewer(const video::GraphicsDevice& device) {
        auto viewer = std::make_unique<ViewerPipeline>();
        video::ViewerConfig config;
        config.width = width_.load();
        config.height = height_.load();
        config.fps = 60;
        std::unique_ptr<video::Decoder> decoder = options_.make_decoder ? options_.make_decoder(device) : nullptr;
#ifdef _WIN32
        // The native presenter shows the clip: a CPU frame callback only for
        // the tests, so the viewer makes no readback otherwise.
        ViewerPipeline::FrameCallback on_frame = options_.on_cpu_frame;
#else
        // No native presenter: the frames go out as RGBA, to the callback
        // that set_frame_callback set (the client), else the test's.
        ViewerPipeline::FrameCallback on_frame = [this](const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t ts) {
            ViewerPipeline::FrameCallback cb;
            {
                std::lock_guard<std::mutex> lock(frame_cb_mutex_);
                cb = frame_cb_;
            }
            if (cb) {
                cb(rgba, w, h, ts);
            } else if (options_.on_cpu_frame) {
                options_.on_cpu_frame(rgba, w, h, ts);
            }
        };
#endif
        if (!viewer->start(device, config, on_frame, std::move(decoder))) {
            MELLO_LOG_ERROR(TAG, "no decoder for the clip (%ux%u)", config.width, config.height);
            return false;
        }
        std::lock_guard<std::mutex> lock(viewer_mutex_);
        if (native_cb_) viewer->set_native_frame_callback(native_cb_);
        auto source = source_;
        viewer->attach_timeline([source] { return source; });
        viewer_ = std::move(viewer);
        return true;
    }

    void run() {
        if (!open()) {
            failed_.store(!stop_flag_.load());
            return;
        }
        {
            std::lock_guard<std::mutex> lock(clock_mutex_);
            has_audio_ = audio_track_ != nullptr;
            latency_us_ = options_.output_latency_us ? options_.output_latency_us() : 0;
            buffering_ = has_audio_;
            wall_at_ = now_us();
        }
        video::GraphicsDevice device = video::create_viewer_device();
        if (!device.handle || !start_viewer(device)) {
            failed_.store(true);
        } else {
            if (has_audio_ && options_.attach_audio) options_.attach_audio(audio_);
            MELLO_LOG_INFO(TAG, "playing: %ux%u, %.2f s, sound %s, %llu bytes", width_.load(), height_.load(),
                           duration_us_.load() / 1e6, has_audio_ ? "Opus" : "off",
                           static_cast<unsigned long long>(options_.length));
            start_at(0);
            demux_loop();
            if (has_audio_ && options_.attach_audio) options_.attach_audio(nullptr);
        }
        source_->stop();
        {
            std::unique_ptr<ViewerPipeline> viewer;
            {
                std::lock_guard<std::mutex> lock(viewer_mutex_);
                viewer = std::move(viewer_);
            }
            if (viewer) viewer->stop();
        }
        video::release_viewer_device(device);
    }

    /// The read positions for play from `target_us`: the video from the key
    /// frame at or before it, the sound from the frame that holds it less the
    /// pre-roll, with the samples before the target decoded and dropped.
    void start_at(int64_t target_us) {
        const auto& vs = video_->samples;
        next_video_ = 0;
        for (size_t i = 0; i < vs.size(); ++i) {
            if (vs[i].sync && video_->time_us(vs[i].time) <= target_us) next_video_ = i;
        }
        want_header_ = true;
        next_audio_ = 0;
        drop_frames_ = 0;
        if (!audio_track_) return;
        const auto& as = audio_track_->samples;
        const int64_t scale = audio_track_->timescale;
        const int64_t want = audio_track_->edit_media_time + target_us * scale / 1'000'000;
        size_t at = 0;
        while (at + 1 < as.size() && as[at].time + as[at].duration <= want) ++at;
        const int32_t pre = audio_track_->roll_distance < 0 ? -audio_track_->roll_distance : kOpusPreRoll;
        next_audio_ = target_us > 0 && at > static_cast<size_t>(pre) ? at - static_cast<size_t>(pre) : 0;
        // The decoder starts fresh; what it decodes before the target (the
        // pre-skip at the start, the pre-roll after a seek) is not played.
        const int64_t skip = std::max<int64_t>(0, want - as[next_audio_].time);
        drop_frames_ = skip * audio::ClipAudioStream::kSampleRate / scale;
        opus_decoder_ctl(opus_.get(), OPUS_RESET_STATE);
    }

    void demux_loop() {
        std::vector<uint8_t> raw;
        std::vector<int16_t> pcm(static_cast<size_t>(kOpusMaxFrame) * 2);
        bool audio_ended = false;
        while (!stop_flag_.load(std::memory_order_acquire)) {
            if (seek_pending_.exchange(false, std::memory_order_acq_rel)) {
                do_seek(seek_to_.load());
                audio_ended = false;
            }
            const bool video_left = next_video_ < video_->samples.size();
            const bool audio_left = audio_track_ && next_audio_ < audio_track_->samples.size();
            if (audio_track_ && !audio_left && !audio_ended) {
                audio_ended = true;
                audio_->set_ended(true);
            }
            if (!video_left && !audio_left) {
                if (!demux_ended_.exchange(true)) MELLO_LOG_INFO(TAG, "the whole clip is read");
                std::this_thread::sleep_for(std::chrono::milliseconds(20));
                continue;
            }
            // The next sample in file order: the reads go forward.
            const bool take_video =
                video_left && (!audio_left || video_->samples[next_video_].offset <=
                                                  audio_track_->samples[next_audio_].offset);
            const Mp4Sample& s = take_video ? video_->samples[next_video_] : audio_track_->samples[next_audio_];
            raw.resize(s.size);
            if (s.size > 0 && !read_at(s.offset, raw.data(), raw.size(), true)) {
                if (read_failed_) {
                    failed_.store(true);
                    break;
                }
                continue;  // a stop or a seek
            }
            if (take_video) {
                if (!push_video(raw, s)) continue;  // a seek or a stop while it waited
                ++next_video_;
            } else {
                const int frames = opus_decode(opus_.get(), raw.data(), static_cast<opus_int32>(raw.size()),
                                               pcm.data(), kOpusMaxFrame, 0);
                ++next_audio_;
                if (frames < 0) {
                    MELLO_LOG_WARN(TAG, "opus_decode failed: %d", frames);
                    continue;
                }
                size_t from = 0;
                if (drop_frames_ > 0) {
                    from = static_cast<size_t>(std::min<int64_t>(drop_frames_, frames));
                    drop_frames_ -= static_cast<int64_t>(from);
                }
                push_audio(pcm.data() + from * audio::ClipAudioStream::kChannels, static_cast<size_t>(frames) - from);
            }
        }
    }

    bool push_video(const std::vector<uint8_t>& raw, const Mp4Sample& s) {
        DemuxedFrame f;
        if (!replay::avcc_to_annex_b(raw.data(), raw.size(), video_->nal_length_size, f.annex_b)) {
            MELLO_LOG_WARN(TAG, "a video sample is not length-prefixed H.264: skipped");
            return true;
        }
        f.keyframe = s.sync;
        f.pts_us = video_->time_us(s.time);
        if (want_header_ && f.keyframe && !header_.empty()) {
            f.annex_b.insert(f.annex_b.begin(), header_.begin(), header_.end());
            want_header_ = false;
        }
        const auto cap = [this] {
            const bool starving = audio_track_ && audio_->buffered_frames() < kResumeFrames && !audio_->ended();
            return starving ? kFrameCapStarving : kFrameCap;
        };
        return source_->push(std::move(f), cap, [this] { return cancelled(true); });
    }

    void push_audio(const int16_t* pcm, size_t frames) {
        size_t done = 0;
        while (done < frames && !cancelled(true)) {
            done += audio_->write(pcm + done * audio::ClipAudioStream::kChannels, frames - done);
            if (done < frames) std::this_thread::sleep_for(std::chrono::milliseconds(5));
        }
    }

    void do_seek(int64_t target_us) {
        target_us = std::clamp<int64_t>(target_us, 0, std::max<int64_t>(duration_us_.load(), 0));
        start_at(target_us);
        // The old frames go first, then the ring: no frame from before the
        // seek can land in the ring after the flush.
        source_->clear();
        {
            std::lock_guard<std::mutex> lock(viewer_mutex_);
            if (viewer_) viewer_->flush();
        }
        const uint64_t frames = static_cast<uint64_t>(target_us) * audio::ClipAudioStream::kSampleRate / 1'000'000;
        audio_->reset(frames + static_cast<uint64_t>(latency_us_) * audio::ClipAudioStream::kSampleRate / 1'000'000);
        audio_->set_ended(false);
        demux_ended_.store(false);
        std::lock_guard<std::mutex> lock(clock_mutex_);
        wall_pos_ = target_us;
        wall_at_ = now_us();
        audio_done_ = false;
        started_ = false;
        buffering_ = has_audio_;
        apply_audio_pause_locked();
        MELLO_LOG_INFO(TAG, "seek to %.2f s", target_us / 1e6);
    }

    ClipPlayerOptions options_;
    std::shared_ptr<audio::ClipAudioStream> audio_;
    std::shared_ptr<ClipFrameSource> source_;
    std::atomic<bool> stop_flag_{false};
    std::atomic<bool> waiting_{false};
    std::atomic<bool> stopped_{false};
    std::atomic<bool> failed_{false};
    std::atomic<bool> demux_ended_{false};
    std::atomic<bool> seek_pending_{false};
    std::atomic<int64_t> seek_to_{0};
    std::atomic<int64_t> duration_us_{0};
    std::atomic<uint32_t> width_{0};
    std::atomic<uint32_t> height_{0};

    // Demux thread.
    Mp4Movie movie_;
    const Mp4Track* video_ = nullptr;
    const Mp4Track* audio_track_ = nullptr;
    std::unique_ptr<OpusDecoder, OpusDecoderDeleter> opus_;
    std::vector<uint8_t> header_;
    size_t next_video_ = 0;
    size_t next_audio_ = 0;
    int64_t drop_frames_ = 0;
    bool want_header_ = true;
    bool read_failed_ = false;

    std::mutex viewer_mutex_;
    std::unique_ptr<ViewerPipeline> viewer_;
    ViewerPipeline::NativeFrameCallback native_cb_;
    std::mutex frame_cb_mutex_;
    ViewerPipeline::FrameCallback frame_cb_;  // frame_cb_mutex_

    mutable std::mutex clock_mutex_;
    bool has_audio_ = false;
    bool audio_done_ = false;  // the sound ended: the wall clock runs on
    bool paused_ = false;
    bool buffering_ = false;
    bool started_ = false;
    int64_t latency_us_ = 0;
    int64_t wall_pos_ = 0;
    int64_t wall_at_ = 0;

    std::thread thread_;
};

}  // namespace

std::unique_ptr<ClipPlayer> ClipPlayer::start(ClipPlayerOptions options) {
    if (!options.read || options.length == 0) return nullptr;
    return std::make_unique<Player>(std::move(options));
}

}  // namespace mello::clip
