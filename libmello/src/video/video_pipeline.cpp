#include "video_pipeline.hpp"

#include <future>
#include "encoder_factory.hpp"
#include "frame_cadence.hpp"
#include "../util/log.hpp"
#include <chrono>
#include <cstdlib>
#include <cstring>
#include <cstdio>
#include <algorithm>
#include <cmath>

#ifdef __APPLE__
#include <CoreVideo/CoreVideo.h>
#include <Accelerate/Accelerate.h>
#endif

namespace mello::video {

// How long `stop_host` waits for the encode thread. It is the same bound the
// capture backends use: long enough that a healthy thread always makes it, and
// short enough that a stuck one never holds a user interface.
static constexpr std::chrono::seconds kEncodeJoinDeadline{5};

// iOS has no hosting/capture in v1 (and the macOS impl lives in the excluded
// capture_screencapturekit.mm), so provide the nullptr stub here too.
#if (!defined(_WIN32) && !defined(__APPLE__)) || defined(MELLO_IOS_NO_HOSTING)
std::unique_ptr<CaptureSource> create_capture_source(const CaptureSourceDesc&) { return nullptr; }
#endif

static constexpr const char* TAG = "video/pipeline";

// Smallest frame the hardware encoders will accept. NVENC H.264's documented
// minimum is 145x49; AMF and QSV are comparable. Below this the encoder rejects
// initialization outright, so catching it here turns a misleading
// "no hardware encoder" into a statement of what is actually wrong.
// The encoder minimums live with the capture sources, because choosing what to
// capture needs them too.
using mello::video::kMinEncodeWidth;
using mello::video::kMinEncodeHeight;

// GOP: 4 s at the preset frame rate (streaming-quality stage 4; 2 s before).
// The plan's 10 s holds at 4 s: on the gate's mobile profile a 10 s GOP gave
// a glass-to-glass p95 of 2455 and 2532 ms against 437 and 1061 ms at 4 s,
// with the same number of IDRs (PLIs drive them there) and no better VMAF,
// fps or freeze time. Measure 10 s again when the stage 3 viewer is in.
// IDRs on demand (join, PLI, a rung change) are unchanged.
// MELLO_STREAM_GOP_SECONDS (1 to 60) overrides it for the GOP measurement of
// the streaming gate.
static uint32_t keyframe_interval_for(uint32_t fps) {
    uint32_t seconds = 4;
#ifdef _WIN32
    char buf[8] = {};
    const DWORD n = GetEnvironmentVariableA("MELLO_STREAM_GOP_SECONDS", buf, sizeof(buf));
    if (n > 0 && n < sizeof(buf)) {
        const long v = std::strtol(buf, nullptr, 10);
        if (v >= 1 && v <= 60) seconds = static_cast<uint32_t>(v);
    }
#endif
    return std::max<uint32_t>(fps, 1) * seconds;
}

static uint64_t now_us() {
    return static_cast<uint64_t>(
        std::chrono::duration_cast<std::chrono::microseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count());
}

VideoPipeline::VideoPipeline() = default;

VideoPipeline::~VideoPipeline() {
    stop_host();
    stop_viewer();

    if (device_.handle) {
#ifdef _WIN32
        device_.d3d11()->Release();
#elif defined(__APPLE__)
        CFRelease(device_.handle);
#endif
        device_.handle = nullptr;
    }
}

bool VideoPipeline::init_device() {
    if (device_.handle) return true;
#ifdef _WIN32
    device_ = create_d3d11_device();
#elif defined(__APPLE__)
    device_ = create_metal_device();
#elif defined(__linux__)
    device_ = create_cpu_device();
#endif
    return device_.handle != nullptr;
}

// ─────────────────────────────────────────────────────────────────────────────
// HOST SIDE
// ─────────────────────────────────────────────────────────────────────────────

bool VideoPipeline::start_host(const CaptureSourceDesc& source,
                               const PipelineConfig& config,
                               PacketCallback on_packet) {
    if (host_running_.load()) {
        MELLO_LOG_WARN(TAG, "Host pipeline already running");
        return false;
    }

    if (!init_device()) return false;

    config_    = config;
    packet_cb_ = std::move(on_packet);

    // 1. Capture. The user picks what to stream; this picks how. A window that
    // cannot carry a stream on its own becomes its process here.
#ifdef _WIN32
    const CaptureSourceDesc resolved = resolve_capture_target(source);
#else
    const CaptureSourceDesc resolved = source;
#endif
    capture_ = create_capture_source(resolved);
    if (capture_) capture_->set_present_delay_histogram(&present_delay_hist_);
    if (!capture_ || !capture_->initialize(device_, resolved)) {
        MELLO_LOG_ERROR(TAG, "Failed to initialize capture source");
        return false;
    }

#ifdef _WIN32
    // NV12 requires even dimensions (chroma plane is half-res)
    uint32_t cap_w = capture_->width()  & ~1u;
    uint32_t cap_h = capture_->height() & ~1u;

    // The color converter downscales in the same GPU pass as BGRA→NV12. The
    // encode size keeps the source aspect, so the converter never stretches.
    const EncodeSize target = encode_size_for(cap_w, cap_h, config.height, config.fps);
    if (target.level_capped) {
        MELLO_LOG_WARN(TAG,
            "Encode size for %ux%u at %u fps is over H.264 level 5.1; reduced to %ux%u",
            cap_w, cap_h, config.fps, target.width, target.height);
    }
    encode_w_ = target.width;
    encode_h_ = target.height;
    uint32_t enc_w = target.width;
    uint32_t enc_h = target.height;

    // Refuse a source too small to encode, naming the actual size.
    //
    // Hardware encoders have a minimum frame size (NVENC H.264 is 145x49) and
    // reject anything under it with a generic unsupported-parameter error. That
    // surfaces as "no hardware encoder available", which reads as a broken GPU
    // and sends debugging in entirely the wrong direction — it cost a live test
    // session. The usual cause is a capture target that is not what the user
    // thinks it is: a minimized window (WGC hands back the ~160x28 iconic size)
    // or an auxiliary window that happens to share the game's title.
    if (enc_w < kMinEncodeWidth || enc_h < kMinEncodeHeight) {
        MELLO_LOG_ERROR(TAG,
            "Capture source is %ux%u, below the encoder minimum of %ux%u — refusing to "
            "start. The target is probably minimized or is not the window you meant.",
            enc_w, enc_h, kMinEncodeWidth, kMinEncodeHeight);
        return false;
    }

    // 2. Video preprocessor: BGRA→NV12 color conversion + GPU downscale.
    // It gets the capture size as it is, odd or not: the source is BGRA and
    // the whole of it scales into the even NV12 encode size.
    preprocessor_ = std::make_unique<VideoPreprocessor>();
    source_w_.store(capture_->width(), std::memory_order_relaxed);
    source_h_.store(capture_->height(), std::memory_order_relaxed);
    source_size_changes_.store(0, std::memory_order_relaxed);
    source_size_known_ = false;
    if (!preprocessor_->initialize(device_, capture_->width(), capture_->height(), enc_w, enc_h)) {
        MELLO_LOG_ERROR(TAG, "Failed to initialize video preprocessor");
        return false;
    }
    preprocessor_w_ = enc_w;
    preprocessor_h_ = enc_h;
    preprocessor_generation_ = 0;
    encoder_generation_ = 0;
    encode_height_request_.store(0, std::memory_order_relaxed);
    geometry_changes_.store(0, std::memory_order_relaxed);
    last_bitrate_kbps_.store(config.bitrate_kbps, std::memory_order_relaxed);

    // 3. Encoder
    EncoderConfig enc_config{};
    enc_config.width         = enc_w;
    enc_config.height        = enc_h;
    enc_config.fps           = config.fps;
    enc_config.bitrate_kbps  = config.bitrate_kbps;
    enc_config.keyframe_interval = keyframe_interval_for(config.fps);
    enc_config.codec         = VideoCodec::H264;

    encoder_ = create_best_encoder(device_, enc_config);
    if (!encoder_) {
        MELLO_LOG_ERROR(TAG, "No encoder available");
        return false;
    }
    encoder_control_.attach(encoder_.get());

    // 4. Start encode thread + capture — frames flow: capture -> preprocess -> queue -> encode thread
    host_running_    = true;
    host_start_time_ = now_us();
    frames_encoded_  = 0;
    eq_head_ = eq_tail_ = eq_count_ = 0;
    eq_drops_ = 0;
    last_convert_ms_ = last_encode_ms_ = 0;
    frames_captured_.store(0, std::memory_order_relaxed);
    last_capture_us_.store(host_start_time_, std::memory_order_relaxed);
    encode_ms_mean_.store(0.0, std::memory_order_relaxed);
    output_fps_.store(0, std::memory_order_relaxed);
    next_emit_deadline_us_ = 0;

    {
        std::promise<void> exited;
        encode_exited_ = exited.get_future();
        encode_thread_ = std::thread([this, exited = std::move(exited)]() mutable {
            encode_thread_func();
            exited.set_value();
        });
    }

    auto self = this;
    if (!capture_->start(config.fps, [self](ID3D11Texture2D* tex, uint64_t ts) {
        self->on_captured_frame(tex, ts);
    })) {
        MELLO_LOG_ERROR(TAG, "Failed to start capture");
        host_running_ = false;
        eq_cv_.notify_all();
        if (encode_thread_.joinable()) encode_thread_.join();
        return false;
    }
#elif defined(__APPLE__)
    // macOS: No preprocessor needed — VT accepts BGRA CVPixelBuffer directly
    uint32_t cap_w = capture_->width()  & ~1u;
    uint32_t cap_h = capture_->height() & ~1u;
    const EncodeSize target = encode_size_for(cap_w, cap_h, config.height, config.fps);
    encode_w_ = target.width;
    encode_h_ = target.height;

    EncoderConfig enc_config{};
    enc_config.width         = encode_w_;
    enc_config.height        = encode_h_;
    enc_config.fps           = config.fps;
    enc_config.bitrate_kbps  = config.bitrate_kbps;
    enc_config.keyframe_interval = keyframe_interval_for(config.fps);
    enc_config.codec         = VideoCodec::H264;

    encoder_ = create_best_encoder(device_, enc_config);
    if (!encoder_) {
        MELLO_LOG_ERROR(TAG, "No encoder available");
        return false;
    }
    encoder_control_.attach(encoder_.get());

    host_running_    = true;
    host_start_time_ = now_us();
    frames_encoded_  = 0;
    frames_captured_.store(0, std::memory_order_relaxed);
    last_capture_us_.store(host_start_time_, std::memory_order_relaxed);
    encode_ms_mean_.store(0.0, std::memory_order_relaxed);
    output_fps_.store(0, std::memory_order_relaxed);
    next_emit_deadline_us_ = 0;

    auto self = this;
    if (!capture_->start(config.fps, [self](void* pixel_buffer, uint64_t ts) {
        self->on_captured_frame(pixel_buffer, ts);
    })) {
        MELLO_LOG_ERROR(TAG, "Failed to start capture");
        host_running_ = false;
        return false;
    }
#endif

    MELLO_LOG_INFO(TAG, "Host pipeline starting: encoder=%s capture=%s res=%ux%u fps=%u bitrate=%ukbps low_latency=%s",
        encoder_ ? encoder_->name() : "none",
        capture_->backend_name(),
        capture_->width(), capture_->height(),
        config.fps, config.bitrate_kbps,
        config.low_latency ? "true" : "false");

    return true;
}

void VideoPipeline::stop_host() {
    if (!host_running_.load()) return;
    host_running_ = false;

    // Each step is logged before it starts. A stop that hangs in a driver or a
    // capture thread then names its own step in the log: the 2026-09-15 beta
    // hang left no way to tell capture, encode and encoder shutdown apart.
    MELLO_LOG_INFO(TAG, "stop_host: capture stop (%s)", capture_ ? capture_->backend_name() : "none");
    if (capture_) {
        capture_->stop();
        if (capture_->stop_timed_out()) {
            // A capture thread is still running inside the driver. It calls
            // back into this pipeline, so nothing here may be freed or shut
            // down: leak the whole pipeline and return. The alternative is a
            // use-after-free, or the indefinite block this replaces.
            abandoned_ = true;
            (void)capture_.release();
            MELLO_LOG_ERROR(TAG,
                "stop_host: capture could not be stopped; abandoning this pipeline "
                "(encoder, preprocessor and device stay allocated)");
            return;
        }
    }

    // Wake and join the encode thread before shutting down encoder/preprocessor.
    //
    // The join is bounded. This thread calls into the display driver and the
    // hardware encoder, and either can block for as long as it likes: on
    // 2026-09-16 a stop sat in this join for 25 minutes while the encode thread
    // waited inside D3D11. When the deadline passes, the thread is left running
    // and everything it touches is left allocated. A leak is the cheap outcome;
    // destroying an encoder under a live thread is a crash.
    MELLO_LOG_INFO(TAG, "stop_host: encode thread join");
    eq_cv_.notify_all();
    bool encode_thread_stopped = true;
    if (encode_thread_.joinable()) {
        if (encode_exited_.valid() &&
            encode_exited_.wait_for(kEncodeJoinDeadline) == std::future_status::ready) {
            encode_thread_.join();
        } else {
            encode_thread_stopped = false;
            encode_thread_detached_.store(true, std::memory_order_relaxed);
            encode_thread_.detach();
            MELLO_LOG_ERROR(TAG,
                "stop_host: the encode thread did not stop within %lld ms. It is left running "
                "and its encoder, preprocessor and frames are leaked on purpose.",
                static_cast<long long>(
                    std::chrono::duration_cast<std::chrono::milliseconds>(kEncodeJoinDeadline)
                        .count()));
        }
    }

    if (!encode_thread_stopped) {
        // Nothing below may run: the thread that is still going owns all of it.
        MELLO_LOG_INFO(TAG, "stop_host: done (encoder and preprocessor abandoned)");
        return;
    }

    encoder_control_.attach(nullptr);
    MELLO_LOG_INFO(TAG, "stop_host: encoder shutdown (%s)", encoder_ ? encoder_->name() : "none");
    if (encoder_)   encoder_->shutdown();
#ifdef _WIN32
    MELLO_LOG_INFO(TAG, "stop_host: preprocessor shutdown");
    if (preprocessor_) preprocessor_->shutdown();
#endif

    uint64_t uptime_s = (now_us() - host_start_time_) / 1'000'000;
    EncoderStats stats{};
    if (encoder_) encoder_->get_stats(stats);

    MELLO_LOG_INFO(TAG, "Host pipeline stopped: uptime=%llus frames_encoded=%llu keyframes=%u bytes_out=%lluMB",
        uptime_s, frames_encoded_, stats.keyframes_sent, stats.bytes_sent / (1024 * 1024));

    capture_.reset();
    encoder_.reset();
#ifdef _WIN32
    {
        std::lock_guard<std::mutex> lock(retired_mutex_);
        retired_preprocessors_.clear();
    }
    preprocessor_.reset();
#endif
}

#ifdef __APPLE__
void VideoPipeline::on_captured_frame(void* cv_pixel_buffer, uint64_t timestamp_us) {
    if (!host_running_.load()) return;

    frames_captured_.fetch_add(1, std::memory_order_relaxed);
    last_capture_us_.store(now_us(), std::memory_order_relaxed);

    if (!decimation_accepts(timestamp_us, next_emit_deadline_us_, effective_output_fps())) {
        return;
    }

    EncodedPacket packet{};
    if (encoder_control_.encode(cv_pixel_buffer, timestamp_us, packet)) {
        frames_encoded_++;

        if (frames_encoded_ <= 3) {
            MELLO_LOG_DEBUG(TAG, "on_captured_frame[%llu]: encoded %zu bytes keyframe=%d",
                frames_encoded_, packet.data.size(), packet.is_keyframe);
        }

        if (frames_encoded_ % 300 == 0) {
            uint64_t uptime_s = (now_us() - host_start_time_) / 1'000'000;
            EncoderStats stats{};
            encoder_->get_stats(stats);
            MELLO_LOG_INFO(TAG, "host: uptime=%llus frames=%llu fps=%u bitrate=%ukbps keyframes=%u bytes=%.1fMB",
                uptime_s, frames_encoded_, stats.fps_actual, stats.bitrate_kbps,
                stats.keyframes_sent, static_cast<double>(stats.bytes_sent) / (1024 * 1024));
        }

        if (packet_cb_) {
            packet_cb_(packet.data.data(), packet.data.size(), packet.is_keyframe, timestamp_us);
        }
    }
}
#endif

void VideoPipeline::get_host_resolution(uint32_t& w, uint32_t& h) const {
    w = encode_w_;
    h = encode_h_;
}

// H.264 level 5.1, the level every encoder backend configures (stage 4;
// every current hardware decoder handles 5.1). An ultrawide source on the
// 1080p60 preset encodes 2560x1080, not the 2288x960 of level 4.2.
static constexpr uint64_t kLevel51MaxFrameMbs  = 36864;
static constexpr uint64_t kLevel51MaxMbsPerSec = 983040;

VideoPipeline::EncodeSize VideoPipeline::encode_size_for(uint32_t capture_w, uint32_t capture_h,
                                                         uint32_t preset_h, uint32_t fps) {
    EncodeSize out{};
    const uint32_t cap_w = capture_w & ~1u;
    const uint32_t cap_h = capture_h & ~1u;
    if (cap_w == 0 || cap_h == 0) return out;

    // The width that keeps the source aspect at height `h`, a multiple of 16
    // and never wider than the capture. Without a vertical scale there is no
    // horizontal scale either.
    auto width_for = [&](uint32_t h) -> uint32_t {
        if (h >= cap_h) return cap_w;
        const double exact = static_cast<double>(cap_w) * h / cap_h;
        uint32_t w = static_cast<uint32_t>(std::lround(exact / 16.0)) * 16;
        if (w > cap_w) w = cap_w & ~15u;
        if (w < 16) w = std::min<uint32_t>(16, cap_w);
        return w;
    };
    auto fits_level = [&](uint32_t w, uint32_t h) {
        const uint64_t mbs = static_cast<uint64_t>((w + 15) / 16) * ((h + 15) / 16);
        return mbs <= kLevel51MaxFrameMbs && mbs * fps <= kLevel51MaxMbsPerSec;
    };

    uint32_t h = (preset_h > 0 && preset_h < cap_h) ? (preset_h & ~1u) : cap_h;
    uint32_t w = width_for(h);
    while (h > 2 && !fits_level(w, h)) {
        out.level_capped = true;
        h -= 2;
        w = width_for(h);
    }
    out.width = w;
    out.height = h;
    return out;
}

void VideoPipeline::request_keyframe() {
    encoder_control_.request_keyframe();
    // An idle stream has no next frame to carry the IDR. Wake the encode
    // thread so it re-encodes the last picture now.
    keepalive_kick_.store(true, std::memory_order_relaxed);
    eq_cv_.notify_all();
}

bool VideoPipeline::idle_repeat_due(bool have_last_frame, bool kicked,
                                    uint64_t now_us, uint64_t last_new_frame_us,
                                    uint64_t last_encode_us) {
    if (!have_last_frame) return false;
    const uint64_t since_new = now_us > last_new_frame_us ? now_us - last_new_frame_us : 0;
    // A keyframe request while frames still flow is carried by the next frame.
    // Only answer it here when capture has been quiet for a few frames.
    static constexpr uint64_t kKickQuietUs = 100'000;
    if (kicked && since_new >= kKickQuietUs) return true;
    if (since_new < kIdleAfterUs) return false;
    const uint64_t since_encode = now_us > last_encode_us ? now_us - last_encode_us : 0;
    return since_encode >= kIdleRepeatIntervalUs;
}

// NOTE for future adaptive bitrate/framerate: avoid reconfiguring the encoder's
// target FPS based on observed/achieved FPS. If frames are dropped (by the
// transport or a frame dropper), lowering the target FPS gives each remaining
// frame a larger bit budget, which increases per-frame size, which triggers
// MORE drops — a cascade that can halve the frame rate permanently. Instead,
// keep the target FPS constant and leave headroom between target and actual
// bitrate so the encoder naturally recovers after transient drops.
void VideoPipeline::set_bitrate(uint32_t kbps) {
    if (kbps != 0) last_bitrate_kbps_.store(kbps, std::memory_order_relaxed);
    encoder_control_.set_bitrate(kbps);
}

bool VideoPipeline::set_encode_height(uint32_t height) {
#ifdef _WIN32
    if (height == 0 || !host_running_.load()) return false;
    encode_height_request_.store(height, std::memory_order_release);
    return true;
#else
    (void)height;
    return false;
#endif
}

void VideoPipeline::get_stats(EncoderStats& out) const {
    if (encoder_) encoder_->get_stats(out);
    else memset(&out, 0, sizeof(out));
}

// Enough frames that one hitch cannot trip a downgrade. At 60fps this is ~1s.
static constexpr uint64_t kEncoderLoadMinFrames = 60;
// Above this share of offered frames being evicted, the encoder is measurably
// failing — the viewer is already losing motion.
static constexpr double kEncoderDropRatioLimit = 0.02;
// Encode time at or above this share of the frame interval leaves no room for a
// harder scene, and the queue is only two deep.
static constexpr double kEncoderBudgetShareLimit = 0.80;

void VideoPipeline::set_output_fps(uint32_t fps) {
    const uint32_t capture_fps = config_.fps > 0 ? config_.fps : 60;
    const uint32_t target = (fps == 0 || fps >= capture_fps) ? 0 : fps;
    const uint32_t previous = output_fps_.exchange(target, std::memory_order_relaxed);
    if (previous == target) {
        return;
    }

    // Rate control budgets bits per frame from the framerate, so the encoder has
    // to be told or a 30fps stream gets 60fps-sized frames.
    encoder_control_.set_framerate(target == 0 ? capture_fps : target);
    MELLO_LOG_INFO(TAG, "output framerate %u -> %u fps (capture %u)",
                   previous == 0 ? capture_fps : previous,
                   target == 0 ? capture_fps : target,
                   capture_fps);
}

// True when this captured frame should be passed downstream under the current
// decimation target. The rule is shared with the capture backend throttles
// (frame_cadence.hpp), so a frame a backend passes is a frame this passes.
bool VideoPipeline::decimation_accepts(uint64_t timestamp_us,
                                       uint64_t& next_deadline_us,
                                       uint32_t output_fps) {
    return frame_cadence_accepts(timestamp_us, next_deadline_us, output_fps);
}

bool VideoPipeline::encoder_is_overloaded(const EncoderLoadSample& sample) {
    if (sample.frames_captured < kEncoderLoadMinFrames || sample.target_fps == 0) {
        return false;
    }

    const double drop_ratio =
        static_cast<double>(sample.queue_drops) / static_cast<double>(sample.frames_captured);
    if (drop_ratio > kEncoderDropRatioLimit) {
        return true;
    }

    const double budget_ms = 1000.0 / static_cast<double>(sample.target_fps);
    return sample.mean_encode_ms >= budget_ms * kEncoderBudgetShareLimit;
}

void VideoPipeline::get_host_telemetry(HostTelemetry& out) const {
    out = HostTelemetry{};

    out.frames_captured = frames_captured_.load(std::memory_order_relaxed);
    const uint64_t last_capture = last_capture_us_.load(std::memory_order_relaxed);
    if (host_running_.load() && last_capture != 0) {
        const uint64_t now = now_us();
        out.capture_idle_ms = (now > last_capture)
            ? static_cast<uint32_t>((now - last_capture) / 1000)
            : 0;
    }

    {
        std::lock_guard<std::mutex> lock(eq_mutex_);
        out.encode_queue_depth = static_cast<uint32_t>(eq_count_);
        out.encode_queue_drops = eq_drops_;
        out.convert_ms = static_cast<float>(last_convert_ms_);
        out.encode_ms  = static_cast<float>(last_encode_ms_);
    }

    if (capture_) {
        out.capture_backend = capture_->backend_name();
        out.capture_failed  = capture_->failed();
        out.capture_state   = static_cast<uint32_t>(capture_->state());
        out.target_exited   = capture_->target_exited();
        out.capture_history = capture_->method_history();
    }
    present_delay_hist_.snapshot(out.present_delay_hist.data());
    if (encoder_) {
        out.encoder_name = encoder_->name();
        out.encoder_cost_tier = encoder_->cost_tier();
        EncodePhaseTiming phases{};
        encoder_->get_phase_timing(phases);
        out.encode_submit_ms = static_cast<float>(phases.submit_ms);
        out.encode_wait_ms   = static_cast<float>(phases.wait_ms);
        out.encode_lock_ms   = static_cast<float>(phases.lock_ms);
    }
    out.encode_ms_mean = static_cast<float>(encode_ms_mean_.load(std::memory_order_relaxed));
    out.idle_repeat_frames = idle_repeat_frames_.load(std::memory_order_relaxed);
    out.source_width = source_w_.load(std::memory_order_relaxed);
    out.source_height = source_h_.load(std::memory_order_relaxed);
    out.source_size_changes = source_size_changes_.load(std::memory_order_relaxed);
    out.gpu_name = device_.adapter_name;
}

bool VideoPipeline::encoder_available() const {
    if (!device_.handle) {
        auto* self = const_cast<VideoPipeline*>(this);
        if (!self->init_device()) return false;
    }
#if defined(_WIN32) || defined(__APPLE__)
    auto encoders = enumerate_encoders(device_);
    return !encoders.empty();
#else
    return false;
#endif
}

#ifdef _WIN32
void VideoPipeline::on_captured_frame(ID3D11Texture2D* texture, uint64_t timestamp_us) {
    if (!host_running_.load()) return;

    frames_captured_.fetch_add(1, std::memory_order_relaxed);
    last_capture_us_.store(now_us(), std::memory_order_relaxed);

    // Decimate before the colour convert: dropping here saves the GPU blit as
    // well as the encode, which is the whole point on a host that is behind.
    if (!decimation_accepts(timestamp_us, next_emit_deadline_us_, effective_output_fps())) {
        return;
    }

    if (capture_ && capture_->consume_swap_event()) {
        MELLO_LOG_WARN(TAG, "Capture backend swap detected, forcing keyframe");
        request_keyframe();
    }

    // Desktop duplication gives the pointer apart from the desktop image: draw
    // it into the frame. The other backends have it in their frames already
    // (WGC), or the viewer draws it from cursor packets (the hook).
    CursorOverlay overlay{};
    const CursorOverlay* cursor = nullptr;
    if (capture_ && capture_->cursor_mode() == CursorMode::Composite &&
        capture_->get_cursor(cursor_frame_) && cursor_frame_.visible &&
        !cursor_frame_.shape_bgra.empty()) {
        overlay.x = cursor_frame_.x;
        overlay.y = cursor_frame_.y;
        overlay.width = cursor_frame_.shape_w;
        overlay.height = cursor_frame_.shape_h;
        overlay.bgra = cursor_frame_.shape_bgra.data();
        overlay.serial = cursor_frame_.shape_serial;
        cursor = &overlay;
    }

    apply_geometry_request(texture);

    // Preprocess on the capture thread (fast GPU blit), then enqueue for encode
    auto t0 = std::chrono::steady_clock::now();
    auto result = preprocessor_->convert(texture, cursor);
    auto t1 = std::chrono::steady_clock::now();
    last_convert_ms_ = std::chrono::duration<double, std::milli>(t1 - t0).count();

    if (!result.texture) {
        MELLO_LOG_WARN(TAG, "on_captured_frame: convert() returned null");
        return;
    }

    // A source of a new size (window resize, display mode change, a ladder
    // move to a method of another size). The preprocessor already scales the
    // whole new picture into the encode size; this records the change for
    // the stats, where the bitrate and geometry controller reads it. The first
    // frame only sets the size: a capture can report its window size before
    // its first frame.
    if (result.source_resized && source_size_known_) {
        const uint32_t changes =
            source_size_changes_.fetch_add(1, std::memory_order_relaxed) + 1;
        MELLO_LOG_INFO(TAG, "source size %ux%u (change %u); encode size stays %ux%u",
                       preprocessor_->source_width(), preprocessor_->source_height(), changes,
                       encode_w_.load(), encode_h_.load());
    }
    source_size_known_ = true;
    source_w_.store(preprocessor_->source_width(), std::memory_order_relaxed);
    source_h_.store(preprocessor_->source_height(), std::memory_order_relaxed);

    // Enqueue for the encode thread (bounded, drop oldest on overflow)
    {
        std::lock_guard<std::mutex> lock(eq_mutex_);
        if (eq_count_ == ENCODE_QUEUE_CAP) {
            // Drop the oldest entry to bound latency
            eq_tail_ = (eq_tail_ + 1) % ENCODE_QUEUE_CAP;
            eq_count_--;
            eq_drops_++;
        }
        encode_queue_[eq_head_] = {result.texture, timestamp_us, preprocessor_w_,
                                   preprocessor_h_, preprocessor_generation_};
        eq_head_ = (eq_head_ + 1) % ENCODE_QUEUE_CAP;
        eq_count_++;
    }
    eq_cv_.notify_one();
}

void VideoPipeline::encode_thread_func() {
    // Keepalive state, only touched on this thread. The last job's texture is a
    // preprocessor NV12 ring slot, valid until stop_host joins this thread.
    EncodeJob last_job{};
    bool have_last_job = false;
    uint64_t last_new_frame_us = 0;
    uint64_t last_encode_us = 0;
    static constexpr auto kIdlePoll = std::chrono::milliseconds(100);

    while (true) {
        // A control value that met a running encode waits here (EncoderControl).
        encoder_control_.apply_pending();

        EncodeJob job{};
        bool idle_repeat = false;
        {
            std::unique_lock<std::mutex> lock(eq_mutex_);
            eq_cv_.wait_for(lock, kIdlePoll, [this] {
                return eq_count_ > 0 || !host_running_.load() ||
                       keepalive_kick_.load(std::memory_order_relaxed);
            });
            if (eq_count_ == 0 && !host_running_.load()) break;
            if (eq_count_ > 0) {
                job = encode_queue_[eq_tail_];
                eq_tail_ = (eq_tail_ + 1) % ENCODE_QUEUE_CAP;
                eq_count_--;
            } else {
                const bool kicked = keepalive_kick_.exchange(false, std::memory_order_relaxed);
                const uint64_t now = now_us();
                if (!idle_repeat_due(have_last_job, kicked, now, last_new_frame_us, last_encode_us)) {
                    continue;
                }
                job = last_job;
                job.timestamp_us = now;
                idle_repeat = true;
            }
        }

        if (job.generation != encoder_generation_) {
            if (!resize_encoder(job.width, job.height, job.generation)) {
                // The encoder is unusable at the new size. Drop the frame;
                // the next job tries again.
                continue;
            }
        }

        if (!idle_repeat) {
            last_job = job;
            have_last_job = true;
            last_new_frame_us = now_us();
        }

        auto t0 = std::chrono::steady_clock::now();
        EncodedPacket packet{};
        if (encoder_control_.encode(job.texture, packet)) {
            auto t1 = std::chrono::steady_clock::now();
            last_encode_ms_ = std::chrono::duration<double, std::milli>(t1 - t0).count();
            // Exponential mean, ~1s of history at 60fps. Cheap, and unlike the
            // last sample it actually tracks whether the encoder is keeping up.
            constexpr double kAlpha = 1.0 / 60.0;
            const double prev_mean = encode_ms_mean_.load(std::memory_order_relaxed);
            encode_ms_mean_.store(prev_mean + kAlpha * (last_encode_ms_ - prev_mean),
                                  std::memory_order_relaxed);
            frames_encoded_++;

            if (frames_encoded_ <= 3) {
                MELLO_LOG_DEBUG(TAG, "encode_thread[%llu]: encoded %zu bytes keyframe=%d",
                    frames_encoded_, packet.data.size(), packet.is_keyframe);
            }

            if (frames_encoded_ % 300 == 0) {
                uint64_t uptime_s = (now_us() - host_start_time_) / 1'000'000;
                EncoderStats stats{};
                encoder_->get_stats(stats);
                MELLO_LOG_INFO(TAG, "host: uptime=%llus frames=%llu fps=%u bitrate=%ukbps keyframes=%u bytes=%.1fMB convert_ms=%.1f encode_ms=%.1f eq_depth=%zu eq_drops=%llu",
                    uptime_s, frames_encoded_, stats.fps_actual, stats.bitrate_kbps,
                    stats.keyframes_sent, static_cast<double>(stats.bytes_sent) / (1024 * 1024),
                    last_convert_ms_, last_encode_ms_, eq_count_, eq_drops_);
            }

            if (idle_repeat) {
                idle_repeat_frames_.fetch_add(1, std::memory_order_relaxed);
            } else {
                maybe_reduce_encoder_cost();
            }

            if (packet_cb_) {
                packet_cb_(packet.data.data(), packet.data.size(), packet.is_keyframe, job.timestamp_us);
            }
        }
        last_encode_us = now_us();
    }
}

// Capture thread. A pending geometry request becomes a new preprocessor at
// the new encode size; the old one is retired, because the encode queue and
// the idle keepalive can still hold its ring textures.
void VideoPipeline::apply_geometry_request(ID3D11Texture2D* texture) {
    const uint32_t request = encode_height_request_.exchange(0, std::memory_order_acq_rel);
    if (request == 0 || !texture) return;
    D3D11_TEXTURE2D_DESC desc{};
    texture->GetDesc(&desc);
    const EncodeSize target = encode_size_for(desc.Width, desc.Height, request,
                                              effective_output_fps());
    if (target.width < kMinEncodeWidth || target.height < kMinEncodeHeight) {
        MELLO_LOG_WARN(TAG, "geometry: %ux%u is under the encoder minimum; ignored",
                       target.width, target.height);
        return;
    }
    if (target.width == preprocessor_w_ && target.height == preprocessor_h_) return;
    auto next = std::make_unique<VideoPreprocessor>();
    if (!next->initialize(device_, desc.Width, desc.Height, target.width, target.height)) {
        MELLO_LOG_ERROR(TAG, "geometry: preprocessor at %ux%u failed; staying at %ux%u",
                        target.width, target.height, preprocessor_w_, preprocessor_h_);
        return;
    }
    {
        std::lock_guard<std::mutex> lock(retired_mutex_);
        retired_preprocessors_.push_back({std::move(preprocessor_), preprocessor_generation_});
    }
    preprocessor_ = std::move(next);
    MELLO_LOG_INFO(TAG, "geometry: encode size %ux%u -> %ux%u (source %ux%u)",
                   preprocessor_w_, preprocessor_h_, target.width, target.height,
                   desc.Width, desc.Height);
    preprocessor_w_ = target.width;
    preprocessor_h_ = target.height;
    ++preprocessor_generation_;
}

// Encode thread. The first job of a new preprocessor generation changes the
// encoder's size; the preprocessors of older generations are then freed.
bool VideoPipeline::resize_encoder(uint32_t width, uint32_t height, uint32_t generation) {
    EncoderConfig config{};
    config.width = width;
    config.height = height;
    config.fps = effective_output_fps();
    config.bitrate_kbps = last_bitrate_kbps_.load(std::memory_order_relaxed);
    config.keyframe_interval = keyframe_interval_for(config_.fps);
    config.codec = VideoCodec::H264;
    if (!encoder_control_.resize(device_, config)) {
        MELLO_LOG_ERROR(TAG, "geometry: the encoder cannot run at %ux%u", width, height);
        return false;
    }
    encoder_generation_ = generation;
    encode_w_ = width;
    encode_h_ = height;
    geometry_changes_.fetch_add(1, std::memory_order_relaxed);
    {
        std::lock_guard<std::mutex> lock(retired_mutex_);
        retired_preprocessors_.erase(
            std::remove_if(retired_preprocessors_.begin(), retired_preprocessors_.end(),
                           [generation](const RetiredPreprocessor& r) {
                               return r.generation < generation;
                           }),
            retired_preprocessors_.end());
    }
    MELLO_LOG_INFO(TAG, "geometry: encoder at %ux%u (generation %u), IDR", width, height,
                   generation);
    return true;
}

// Runs on the encode thread, immediately after a frame is encoded, so the
// measurement and the reconfigure share one thread and no extra locking.
void VideoPipeline::maybe_reduce_encoder_cost() {
    if (!encoder_ || encoder_->cost_tier() >= encoder_->max_cost_tier()) {
        return;
    }

    const uint64_t captured = frames_captured_.load(std::memory_order_relaxed);
    uint64_t drops = 0;
    {
        std::lock_guard<std::mutex> lock(eq_mutex_);
        drops = eq_drops_;
    }

    const uint64_t window_frames = captured - cost_window_start_captured_;
    EncoderLoadSample sample{};
    sample.frames_captured = window_frames;
    sample.queue_drops     = drops - cost_window_start_drops_;
    sample.mean_encode_ms  = encode_ms_mean_.load(std::memory_order_relaxed);
    sample.target_fps      = config_.fps;

    if (window_frames < 60) {
        return;
    }

    if (encoder_is_overloaded(sample)) {
        encoder_control_.reduce_cost_tier();
    }
    // Reset the window either way: after a downgrade so the next decision
    // measures the new configuration, and after a healthy window so old drops
    // cannot accumulate into a false positive later.
    cost_window_start_captured_ = captured;
    cost_window_start_drops_    = drops;
}
#endif

// ─────────────────────────────────────────────────────────────────────────────
// VIEWER SIDE (streaming-quality stage 3: video_viewer.hpp)
// ─────────────────────────────────────────────────────────────────────────────

bool VideoPipeline::start_viewer(const PipelineConfig& config, FrameCallback on_frame) {
    if (viewer_ && viewer_->running()) {
        MELLO_LOG_WARN(TAG, "Viewer pipeline already running");
        return false;
    }
    if (!init_device()) return false;
    config_ = config;
    viewer_ = std::make_unique<ViewerPipeline>();
    viewer_->set_native_frame_callback(native_frame_cb_);
    ViewerConfig vc;
    vc.width = config.width;
    vc.height = config.height;
    vc.fps = config.fps;
    if (!viewer_->start(device_, vc, std::move(on_frame))) {
        viewer_.reset();
        return false;
    }
    viewer_running_ = true;
    return true;
}

void VideoPipeline::set_native_frame_callback(NativeFrameCallback on_native_frame) {
    native_frame_cb_ = on_native_frame;
    if (viewer_) viewer_->set_native_frame_callback(std::move(on_native_frame));
}

void VideoPipeline::attach_receive_timeline(ViewerPipeline::TimelineProvider provider) {
    if (viewer_) viewer_->attach_timeline(std::move(provider));
}

void VideoPipeline::stop_viewer() {
    if (!viewer_) return;
    viewer_running_ = false;
    viewer_->stop();
    viewer_.reset();
}

bool VideoPipeline::feed_packet(const uint8_t* data, size_t size, bool is_keyframe) {
    return viewer_ && viewer_->feed_packet(data, size, is_keyframe);
}

size_t VideoPipeline::decode_queue_depth() const {
    return viewer_ ? viewer_->decode_queue_depth() : 0;
}

size_t VideoPipeline::decoded_ring_depth() const {
    return viewer_ ? viewer_->ring_depth() : 0;
}

bool VideoPipeline::present_frame() {
    return viewer_ && viewer_->take_presented();
}

void VideoPipeline::get_viewer_stats(ViewerStats& out) const {
    if (viewer_) viewer_->stats(out);
}

// ─────────────────────────────────────────────────────────────────────────────
// CURSOR
// ─────────────────────────────────────────────────────────────────────────────

bool VideoPipeline::get_cursor_packet(uint8_t* buf, size_t* size) {
#ifdef _WIN32
    if (!capture_ || !host_running_.load() || !buf || !size) return false;
    std::lock_guard<std::mutex> lock(cursor_packet_mutex_);

    CursorState cs{};
    bool include_shape = false;
    if (capture_->cursor_mode() != CursorMode::Packet) {
        // The frames have the pointer, or nothing does. A viewer that drew it
        // from packets must stop: tell it once.
        if (!cursor_sent_visible_) return false;
        cs.visible = false;
        cs.x = cursor_sent_x_;
        cs.y = cursor_sent_y_;
    } else {
        if (!capture_->get_cursor(cursor_packet_src_)) return false;

        // Source pixels to encode pixels: the preprocessor's rule.
        uint32_t src_w = source_w_.load(std::memory_order_relaxed);
        uint32_t src_h = source_h_.load(std::memory_order_relaxed);
        if (src_w == 0 || src_h == 0) {
            src_w = capture_->width();
            src_h = capture_->height();
        }
        if (src_w == 0 || src_h == 0 || encode_w_ == 0) return false;
        const FitRect fit = VideoPreprocessor::fit(src_w, src_h, encode_w_, encode_h_);
        const double scale = static_cast<double>(fit.width) / src_w;
        const uint32_t scale_milli = static_cast<uint32_t>(scale * 1000.0 + 0.5);

        // The shape is scaled as the frame is, never up, at most
        // kMaxCursorPacketSide a side. Only when it or the scale changes.
        const CursorData& src = cursor_packet_src_;
        if (src.shape_serial != cursor_packet_serial_ ||
            scale_milli != cursor_packet_scale_milli_) {
            cursor_packet_serial_ = src.shape_serial;
            cursor_packet_scale_milli_ = scale_milli;
            CursorState shape{};
            if (src.shape_w > 0 && src.shape_h > 0 && !src.shape_bgra.empty()) {
                double f = std::min(1.0, scale);
                const uint32_t longest = std::max<uint32_t>(src.shape_w, src.shape_h);
                if (longest * f > kMaxCursorPacketSide) f = static_cast<double>(kMaxCursorPacketSide) / longest;
                const uint32_t w = std::max<uint32_t>(1, static_cast<uint32_t>(src.shape_w * f + 0.5));
                const uint32_t h = std::max<uint32_t>(1, static_cast<uint32_t>(src.shape_h * f + 0.5));
                std::vector<uint8_t> bgra = (w == src.shape_w && h == src.shape_h)
                                                ? src.shape_bgra
                                                : scale_bgra(src.shape_bgra, src.shape_w, src.shape_h, w, h);
                // The packet carries RGBA.
                for (size_t i = 0; i + 3 < bgra.size(); i += 4) std::swap(bgra[i], bgra[i + 2]);
                shape.shape_w = static_cast<uint16_t>(w);
                shape.shape_h = static_cast<uint16_t>(h);
                shape.shape_rgba = std::move(bgra);
            }
            cursor_packet_shape_ = std::move(shape);
            ++cursor_shape_version_;
        }

        cs.visible = src.visible && !cursor_packet_shape_.shape_rgba.empty();
        cs.x = static_cast<int32_t>(fit.x) + static_cast<int32_t>(std::floor(src.x * scale));
        cs.y = static_cast<int32_t>(fit.y) + static_cast<int32_t>(std::floor(src.y * scale));
        include_shape = cs.visible && cursor_sent_version_ != cursor_shape_version_;
        if (!include_shape && cs.visible == cursor_sent_visible_ &&
            (!cs.visible || (cs.x == cursor_sent_x_ && cs.y == cursor_sent_y_))) {
            return false;  // nothing new
        }
        if (include_shape) {
            cs.shape_w = cursor_packet_shape_.shape_w;
            cs.shape_h = cursor_packet_shape_.shape_h;
            cs.shape_rgba = cursor_packet_shape_.shape_rgba;
        }
    }

    const size_t written = serialize_cursor_packet(cs, include_shape, buf, *size);
    if (written == 0) return false;
    *size = written;
    cursor_sent_visible_ = cs.visible;
    cursor_sent_x_ = cs.x;
    cursor_sent_y_ = cs.y;
    if (include_shape) cursor_sent_version_ = cursor_shape_version_;
    return true;
#else
    // ScreenCaptureKit draws the pointer into its frames.
    (void)buf;
    (void)size;
    return false;
#endif
}

void VideoPipeline::apply_cursor_packet(const uint8_t* buf, size_t size) {
    std::lock_guard<std::mutex> lock(cursor_mutex_);
    deserialize_cursor_packet(buf, size, viewer_cursor_);
}

void VideoPipeline::get_cursor_state(CursorState& out) const {
    std::lock_guard<std::mutex> lock(cursor_mutex_);
    out = viewer_cursor_;
}

} // namespace mello::video
