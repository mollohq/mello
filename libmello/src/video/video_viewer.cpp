#include "video_viewer.hpp"

#include "decoder_factory.hpp"
#include "../util/log.hpp"

#include <algorithm>
#include <chrono>

#ifdef _WIN32
#include <windows.h>
#include <timeapi.h>
#endif
#ifdef __APPLE__
#include <CoreVideo/CoreVideo.h>
#include <Accelerate/Accelerate.h>
#endif

namespace mello::video {

using mello::transport::TimelineDecision;
using mello::transport::VideoReceiveTimeline;

static constexpr const char* TAG = "video/viewer";

// The decode thread asks for the peer's timeline again this often while it
// has none (the video track is not open yet), and wakes at least this often
// to see a stop.
static constexpr int64_t kProviderRetryUs = 100'000;
// A frame is shown when its render time is at most this far ahead: the
// present callback and the compositor take about that long.
static constexpr int64_t kPresentLeadUs = 2'000;
// Without a timeline (tests and tools feed a recorded stream in one burst)
// the decode jobs queue up to this many access units. Dropping a delta frame
// breaks every frame after it, so the bound is only a memory guard.
static constexpr size_t kMaxDecodeJobs = 1024;

int64_t ViewerPipeline::now_us() {
    return std::chrono::duration_cast<std::chrono::microseconds>(
               std::chrono::steady_clock::now().time_since_epoch())
        .count();
}

ViewerPipeline::ViewerPipeline() = default;

ViewerPipeline::~ViewerPipeline() {
    stop();
}

bool ViewerPipeline::start(const GraphicsDevice& device, const ViewerConfig& config, FrameCallback on_frame,
                           std::unique_ptr<Decoder> decoder) {
    if (running_.load()) {
        MELLO_LOG_WARN(TAG, "Viewer pipeline already running");
        return false;
    }
    device_ = device;
    config_ = config;
    {
        std::lock_guard<std::mutex> lock(callback_mutex_);
        frame_cb_ = std::move(on_frame);
    }

    DecoderConfig dec_config{};
    dec_config.width = config.width;
    dec_config.height = config.height;
    dec_config.codec = VideoCodec::H264;
    decoder_ = decoder ? std::move(decoder) : create_best_decoder(device_, dec_config);
    if (!decoder_) {
        MELLO_LOG_ERROR(TAG, "No decoder available");
        return false;
    }
    decoder_name_ = decoder_->name();
#ifdef _WIN32
    // The CPU readback is set up for every viewer: the gate and the tools
    // use the CPU callback, the client the native one. It costs one small
    // staging texture until the first readback.
    converter_ready_ = converter_.initialize(device_, true);
    if (!converter_ready_) {
        MELLO_LOG_ERROR(TAG, "No frame converter: this viewer shows no picture");
    }
#endif

    for (RingSlot& slot : ring_) slot = RingSlot{};
    {
        std::lock_guard<std::mutex> lock(jobs_mutex_);
        jobs_.clear();
    }
#ifdef _WIN32
    // The present thread wakes at each frame's render time and the decode
    // thread at each release time: both need 1 ms timer resolution (the
    // default 15.6 ms doubled the frame interval). Counted by Windows.
    timeBeginPeriod(1);
#endif
    running_.store(true, std::memory_order_release);
    decode_thread_ = std::thread([this] { decode_thread_main(); });
    present_thread_ = std::thread([this] { present_thread_main(); });
    MELLO_LOG_INFO(TAG, "Viewer pipeline started: decoder=%s %ux%u", decoder_->name(), config.width,
                   config.height);
    return true;
}

void ViewerPipeline::stop() {
    if (!running_.exchange(false)) {
        return;
    }
    jobs_cv_.notify_all();
    ring_cv_.notify_all();
    {
        std::lock_guard<std::mutex> lock(timeline_stats_mutex_);
        if (stats_timeline_) stats_timeline_->wake();
    }
    for (std::thread* t : {&decode_thread_, &present_thread_}) {
        if (t->joinable()) {
            if (t->get_id() == std::this_thread::get_id()) {
                t->detach();
            } else {
                t->join();
            }
        }
    }
#ifdef _WIN32
    timeEndPeriod(1);
#endif
    MELLO_LOG_INFO(TAG, "Viewer pipeline stopped: decoded=%llu presented=%llu dropped=%llu decode_errors=%llu",
                   static_cast<unsigned long long>(frames_decoded_.load()),
                   static_cast<unsigned long long>(frames_presented_.load()),
                   static_cast<unsigned long long>(frames_dropped_present_.load()),
                   static_cast<unsigned long long>(decode_errors_.load()));
    {
        std::lock_guard<std::mutex> lock(ring_mutex_);
        for (RingSlot& slot : ring_) release_slot_locked(slot);
    }
    if (decoder_) decoder_->shutdown();
    decoder_.reset();
#ifdef _WIN32
    converter_.shutdown();
    converter_ready_ = false;
#endif
    {
        std::lock_guard<std::mutex> lock(jobs_mutex_);
        jobs_.clear();
    }
    timeline_.reset();
    rgba_buf_.clear();
}

void ViewerPipeline::set_native_frame_callback(NativeFrameCallback cb) {
    std::lock_guard<std::mutex> lock(callback_mutex_);
    native_cb_ = std::move(cb);
}

void ViewerPipeline::attach_timeline(TimelineProvider provider) {
    {
        std::lock_guard<std::mutex> lock(provider_mutex_);
        provider_ = std::move(provider);
    }
    attached_.store(true, std::memory_order_release);
    jobs_cv_.notify_all();
}

bool ViewerPipeline::feed_packet(const uint8_t* data, size_t size, bool is_keyframe) {
    if (!running_.load() || !data || size == 0) return false;
    {
        std::lock_guard<std::mutex> lock(jobs_mutex_);
        if (jobs_.size() >= kMaxDecodeJobs) {
            // Without a timeline nothing asks for a key frame: drop the
            // oldest delta, never the newest key frame.
            auto drop = std::find_if(jobs_.begin(), jobs_.end(), [](const DecodeJob& j) { return !j.keyframe; });
            if (drop != jobs_.end()) {
                jobs_.erase(drop);
            } else {
                jobs_.pop_front();
            }
        }
        DecodeJob job;
        job.bytes.assign(data, data + size);
        job.keyframe = is_keyframe;
        jobs_.push_back(std::move(job));
    }
    jobs_cv_.notify_one();
    return true;
}

size_t ViewerPipeline::decode_queue_depth() const {
    std::lock_guard<std::mutex> lock(jobs_mutex_);
    return jobs_.size();
}

size_t ViewerPipeline::ring_depth() const {
    std::lock_guard<std::mutex> lock(ring_mutex_);
    return static_cast<size_t>(std::count_if(ring_.begin(), ring_.end(), [](const RingSlot& s) {
        return s.state == RingSlot::State::Ready;
    }));
}

bool ViewerPipeline::take_presented() {
    return presented_since_take_.exchange(false, std::memory_order_acq_rel);
}

void ViewerPipeline::stats(ViewerStats& out) const {
    out.decoder_name = decoder_name_;
    out.attached = attached_.load();
    out.frames_decoded = frames_decoded_.load();
    out.decode_errors = decode_errors_.load();
    out.frames_presented = frames_presented_.load();
    out.frames_dropped_present = frames_dropped_present_.load();
    out.decode_ms_last = decode_ms_last_.load();
    out.decode_ms_mean = decode_ms_mean_.load();
    out.present_lateness_ms = present_lateness_ms_.load();
    {
        std::lock_guard<std::mutex> lock(last_shown_mutex_);
        out.has_last_shown = has_last_shown_;
        out.last_shown_rtp = last_shown_rtp_;
        out.last_shown_us = last_shown_us_;
    }
    std::lock_guard<std::mutex> lock(timeline_stats_mutex_);
    if (stats_timeline_) out.timeline = stats_timeline_->stats();
}

// ---- decode thread ----------------------------------------------------------

std::shared_ptr<VideoReceiveTimeline> ViewerPipeline::current_timeline() {
    const int64_t now = now_us();
    if (timeline_ && now - provider_checked_us_ < kProviderRetryUs) return timeline_;
    provider_checked_us_ = now;
    TimelineProvider provider;
    {
        std::lock_guard<std::mutex> lock(provider_mutex_);
        provider = provider_;
    }
    if (!provider) return timeline_;
    auto fresh = provider();
    if (fresh && fresh != timeline_) {
        MELLO_LOG_INFO(TAG, "Viewer takes frames from a %s receive timeline", timeline_ ? "new" : "");
        timeline_ = fresh;
        std::lock_guard<std::mutex> lock(timeline_stats_mutex_);
        stats_timeline_ = fresh;
    }
    return timeline_;
}

void ViewerPipeline::decode_thread_main() {
    while (running_.load(std::memory_order_acquire)) {
        DecodeJob job;
        bool have_job = false;
        {
            std::unique_lock<std::mutex> lock(jobs_mutex_);
            if (!jobs_.empty()) {
                job = std::move(jobs_.front());
                jobs_.pop_front();
                have_job = true;
            } else if (!attached_.load(std::memory_order_acquire)) {
                jobs_cv_.wait_for(lock, std::chrono::microseconds(kProviderRetryUs), [this] {
                    return !running_.load() || !jobs_.empty() || attached_.load();
                });
                continue;
            }
        }
        if (have_job) {
            const int64_t t0 = now_us();
            if (!decode_into_ring(job.bytes, job.keyframe, t0)) {
                decode_errors_.fetch_add(1, std::memory_order_relaxed);
            }
            continue;
        }

        auto timeline = current_timeline();
        if (!timeline) {
            std::unique_lock<std::mutex> lock(jobs_mutex_);
            jobs_cv_.wait_for(lock, std::chrono::microseconds(kProviderRetryUs),
                              [this] { return !running_.load() || !jobs_.empty(); });
            continue;
        }

        TimelineDecision d = timeline->poll();
        switch (d.kind) {
        case TimelineDecision::Kind::Release: {
            const int64_t t0 = now_us();
            const bool ok = decode_into_ring(d.frame.annex_b, d.frame.keyframe,
                                             d.frame.render_time_us, d.frame.rtp_timestamp);
            const int64_t decode_us = now_us() - t0;
            if (ok) {
                timeline->on_decoded(decode_us);
            } else {
                decode_errors_.fetch_add(1, std::memory_order_relaxed);
                timeline->on_decode_error();
            }
            break;
        }
        case TimelineDecision::Kind::KeyframeNeeded:
            // The session's handler sent the request already.
            break;
        case TimelineDecision::Kind::Wait:
        case TimelineDecision::Kind::Idle: {
            const int64_t wake = d.kind == TimelineDecision::Kind::Wait
                                     ? std::min<int64_t>(d.wake_us, now_us() + kProviderRetryUs)
                                     : now_us() + kProviderRetryUs;
            if (!timeline->wait(wake)) {
                // A stopped timeline belongs to a closed session. Ask the
                // provider again a little later: the peer replaces the
                // session when the video track opens again.
                timeline_.reset();
                std::unique_lock<std::mutex> lock(jobs_mutex_);
                jobs_cv_.wait_for(lock, std::chrono::milliseconds(10),
                                  [this] { return !running_.load(); });
            }
            break;
        }
        }
    }
}

int ViewerPipeline::acquire_slot_for_decode() {
    std::lock_guard<std::mutex> lock(ring_mutex_);
    for (size_t i = 0; i < ring_.size(); ++i) {
        if (ring_[i].state == RingSlot::State::Free) {
            ring_[i].state = RingSlot::State::Decoding;
            return static_cast<int>(i);
        }
    }
    // Every slot is full: the oldest frame not on screen gives its slot up.
    int oldest = -1;
    for (size_t i = 0; i < ring_.size(); ++i) {
        if (ring_[i].state == RingSlot::State::Ready &&
            (oldest < 0 || ring_[i].seq < ring_[static_cast<size_t>(oldest)].seq)) {
            oldest = static_cast<int>(i);
        }
    }
    if (oldest < 0) return -1;
    release_slot_locked(ring_[static_cast<size_t>(oldest)]);
    frames_dropped_present_.fetch_add(1, std::memory_order_relaxed);
    ring_[static_cast<size_t>(oldest)].state = RingSlot::State::Decoding;
    return oldest;
}

void ViewerPipeline::release_slot_locked(RingSlot& slot) {
#ifdef _WIN32
    slot.picture = DecodedPicture{};
#elif defined(__APPLE__)
    if (slot.pixel_buffer) {
        CVPixelBufferRelease(static_cast<CVPixelBufferRef>(slot.pixel_buffer));
    }
    slot.pixel_buffer = nullptr;
#endif
    slot.state = RingSlot::State::Free;
}

bool ViewerPipeline::decode_into_ring(const std::vector<uint8_t>& bytes, bool keyframe, int64_t render_us,
                                      std::optional<uint32_t> rtp_timestamp) {
    if (!decoder_ || bytes.empty()) return false;
    const int slot_index = acquire_slot_for_decode();
    if (slot_index < 0) return false;

    const int64_t t0 = now_us();
    const DecodeFeedResult result = decoder_->decode(bytes.data(), bytes.size(), keyframe, slot_index);
    const double ms = static_cast<double>(now_us() - t0) / 1000.0;
    decode_ms_last_.store(ms, std::memory_order_relaxed);
    const double mean = decode_ms_mean_.load(std::memory_order_relaxed);
    decode_ms_mean_.store(mean == 0.0 ? ms : mean * 0.95 + ms * 0.05, std::memory_order_relaxed);

    std::unique_lock<std::mutex> lock(ring_mutex_);
    RingSlot& slot = ring_[static_cast<size_t>(slot_index)];
    if (result != DecodeFeedResult::FrameReady) {
        slot.state = RingSlot::State::Free;
        return result == DecodeFeedResult::Accepted;
    }
#ifdef _WIN32
    if (!decoder_->picture(slot_index, slot.picture)) {
        slot.state = RingSlot::State::Free;
        MELLO_LOG_ERROR(TAG, "Decoder %s reported a frame without a picture", decoder_->name());
        return false;
    }
#elif defined(__APPLE__)
    void* buffer = decoder_->get_frame_buffer();
    if (!buffer) {
        slot.state = RingSlot::State::Free;
        MELLO_LOG_ERROR(TAG, "Decoder %s reported a frame without a pixel buffer", decoder_->name());
        return false;
    }
    CVPixelBufferRetain(static_cast<CVPixelBufferRef>(buffer));
    slot.pixel_buffer = buffer;
#endif
    slot.render_us = render_us;
    slot.has_rtp = rtp_timestamp.has_value();
    slot.rtp_timestamp = rtp_timestamp.value_or(0);
    slot.seq = next_seq_++;
    slot.state = RingSlot::State::Ready;
    lock.unlock();
    ring_cv_.notify_all();

    const uint64_t decoded = frames_decoded_.fetch_add(1, std::memory_order_relaxed) + 1;
    if (decoded <= 3 || decoded % 600 == 0) {
        MELLO_LOG_INFO(TAG, "viewer: decoded=%llu errors=%llu presented=%llu dropped=%llu dec=%s decode_ms=%.2f",
                       static_cast<unsigned long long>(decoded),
                       static_cast<unsigned long long>(decode_errors_.load()),
                       static_cast<unsigned long long>(frames_presented_.load()),
                       static_cast<unsigned long long>(frames_dropped_present_.load()), decoder_->name(), ms);
    }
    return true;
}

// ---- present thread ---------------------------------------------------------

void ViewerPipeline::present_thread_main() {
    std::unique_lock<std::mutex> lock(ring_mutex_);
    while (running_.load(std::memory_order_acquire)) {
        const int64_t now = now_us();
        int due = -1;
        int64_t earliest = INT64_MAX;
        for (size_t i = 0; i < ring_.size(); ++i) {
            const RingSlot& s = ring_[i];
            if (s.state != RingSlot::State::Ready) continue;
            if (s.render_us <= now + kPresentLeadUs) {
                if (due < 0 || s.seq > ring_[static_cast<size_t>(due)].seq) due = static_cast<int>(i);
            } else {
                earliest = std::min(earliest, s.render_us);
            }
        }
        if (due < 0) {
            if (earliest == INT64_MAX) {
                ring_cv_.wait(lock);
            } else {
                ring_cv_.wait_for(lock, std::chrono::microseconds(earliest - kPresentLeadUs - now));
            }
            continue;
        }
        // Older frames that are due too were never shown: a newer one is due.
        uint64_t dropped = 0;
        for (RingSlot& s : ring_) {
            if (s.state == RingSlot::State::Ready && s.seq < ring_[static_cast<size_t>(due)].seq &&
                s.render_us <= now + kPresentLeadUs) {
                release_slot_locked(s);
                ++dropped;
            }
        }
        if (dropped) {
            frames_dropped_present_.fetch_add(dropped, std::memory_order_relaxed);
        }
        RingSlot& slot = ring_[static_cast<size_t>(due)];
        slot.state = RingSlot::State::Presenting;
        lock.unlock();

        present_slot(slot);

        lock.lock();
        release_slot_locked(slot);
    }
}

void ViewerPipeline::present_slot(RingSlot& slot) {
    FrameCallback frame_cb;
    NativeFrameCallback native_cb;
    {
        std::lock_guard<std::mutex> lock(callback_mutex_);
        frame_cb = frame_cb_;
        native_cb = native_cb_;
    }
    // The present time is the clock's decision, before the conversion and
    // the callback's own work (a CPU readback in the gate and the tools).
    const int64_t t = now_us();
    const uint64_t ts = static_cast<uint64_t>(t);
    present_lateness_ms_.store(static_cast<double>(t - slot.render_us) / 1000.0, std::memory_order_relaxed);
    bool shown = false;

#ifdef _WIN32
    if (!converter_ready_) return;
    const DecodedPicture& pic = slot.picture;
    if (native_cb) {
        if (converter_.convert(pic, false)) {
            native_cb(converter_.shared_rgba_handle(), converter_.width(), converter_.height(),
                      /*MELLO_NATIVE_FRAME_FORMAT_RGBA8*/ 1, converter_.height(), ts);
            shown = true;
        }
    } else if (frame_cb) {
        if (converter_.convert(pic, true)) {
            rgba_buf_.resize(static_cast<size_t>(converter_.width()) * converter_.height() * 4);
            if (converter_.read_rgba(rgba_buf_.data())) {
                frame_cb(rgba_buf_.data(), converter_.width(), converter_.height(), ts);
                shown = true;
            }
        }
    }
#elif defined(__APPLE__)
    auto pb = static_cast<CVPixelBufferRef>(slot.pixel_buffer);
    if (pb && frame_cb) {
        CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
        const uint32_t w = static_cast<uint32_t>(CVPixelBufferGetWidth(pb));
        const uint32_t h = static_cast<uint32_t>(CVPixelBufferGetHeight(pb));
        const size_t stride = CVPixelBufferGetBytesPerRow(pb);
        auto* base = static_cast<uint8_t*>(CVPixelBufferGetBaseAddress(pb));
        if (base && w > 0 && h > 0) {
            rgba_buf_.resize(static_cast<size_t>(w) * h * 4);
            vImage_Buffer src = {base, h, w, stride};
            vImage_Buffer dest = {rgba_buf_.data(), h, w, w * 4u};
            const uint8_t permute_map[4] = {2, 1, 0, 3};
            vImagePermuteChannels_ARGB8888(&src, &dest, permute_map, kvImageNoFlags);
            frame_cb(rgba_buf_.data(), w, h, ts);
            shown = true;
        }
        CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
    }
#else
    (void)ts;
#endif
    if (shown) {
        frames_presented_.fetch_add(1, std::memory_order_relaxed);
        presented_since_take_.store(true, std::memory_order_release);
        if (slot.has_rtp) {
            std::lock_guard<std::mutex> lock(last_shown_mutex_);
            has_last_shown_ = true;
            last_shown_rtp_ = slot.rtp_timestamp;
            last_shown_us_ = t;
        }
    }
}

} // namespace mello::video
