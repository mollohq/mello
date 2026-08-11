#include "video_pipeline.hpp"
#include "encoder_factory.hpp"
#include "decoder_factory.hpp"
#include "../util/log.hpp"
#include <chrono>
#include <cstring>
#include <cstdio>
#include <algorithm>

#ifdef __APPLE__
#include <CoreVideo/CoreVideo.h>
#include <Accelerate/Accelerate.h>
#endif

namespace mello::video {

// iOS has no hosting/capture in v1 (and the macOS impl lives in the excluded
// capture_screencapturekit.mm), so provide the nullptr stub here too.
#if (!defined(_WIN32) && !defined(__APPLE__)) || defined(MELLO_IOS_NO_HOSTING)
std::unique_ptr<CaptureSource> create_capture_source(const CaptureSourceDesc&) { return nullptr; }
#endif

static constexpr const char* TAG = "video/pipeline";

// Ring-buffer helpers for decoded frames ─────────────────────────────────────

#ifdef _WIN32
void VideoPipeline::push_decoded(ID3D11Texture2D* frame) {
#elif defined(__APPLE__)
void VideoPipeline::push_decoded(void* frame) {
#endif
    std::lock_guard<std::mutex> lock(decoded_ring_mutex_);
    if (decoded_ring_count_ == DECODED_RING_CAP) {
#ifdef __APPLE__
        if (decoded_ring_[decoded_ring_tail_])
            CVPixelBufferRelease((CVPixelBufferRef)decoded_ring_[decoded_ring_tail_]);
#endif
        decoded_ring_[decoded_ring_tail_] = nullptr;
        decoded_ring_tail_ = (decoded_ring_tail_ + 1) % DECODED_RING_CAP;
        decoded_ring_count_--;
    }
    decoded_ring_[decoded_ring_head_] = frame;
    decoded_ring_head_ = (decoded_ring_head_ + 1) % DECODED_RING_CAP;
    decoded_ring_count_++;
}

#ifdef _WIN32
ID3D11Texture2D* VideoPipeline::pop_decoded() {
#elif defined(__APPLE__)
void* VideoPipeline::pop_decoded() {
#endif
    std::lock_guard<std::mutex> lock(decoded_ring_mutex_);
    if (decoded_ring_count_ == 0) return nullptr;
    auto* frame = decoded_ring_[decoded_ring_tail_];
    decoded_ring_[decoded_ring_tail_] = nullptr;
    decoded_ring_tail_ = (decoded_ring_tail_ + 1) % DECODED_RING_CAP;
    decoded_ring_count_--;
    return frame;
}
static constexpr uint32_t NATIVE_FMT_UNKNOWN = 0;
static constexpr uint32_t NATIVE_FMT_RGBA8 = 1;
static constexpr uint32_t NATIVE_FMT_R8_NV12_LAYOUT = 2;
static constexpr uint32_t NATIVE_FMT_NV12 = 3;

static void save_bmp_rgba(const char* path, const uint8_t* rgba, uint32_t w, uint32_t h) {
    FILE* f = fopen(path, "wb");
    if (!f) return;

    uint32_t row_bytes = w * 4;
    uint32_t img_size  = row_bytes * h;
    uint32_t file_size = 54 + img_size;

    uint8_t hdr[54]{};
    hdr[0] = 'B'; hdr[1] = 'M';
    memcpy(hdr + 2, &file_size, 4);
    uint32_t off = 54; memcpy(hdr + 10, &off, 4);
    uint32_t dib = 40;  memcpy(hdr + 14, &dib, 4);
    memcpy(hdr + 18, &w, 4);
    int32_t neg_h = -(int32_t)h;
    memcpy(hdr + 22, &neg_h, 4);
    uint16_t planes = 1; memcpy(hdr + 26, &planes, 2);
    uint16_t bpp = 32;   memcpy(hdr + 28, &bpp, 2);
    memcpy(hdr + 34, &img_size, 4);
    fwrite(hdr, 1, 54, f);

    for (uint32_t i = 0; i < w * h; ++i) {
        uint8_t bgra[4] = { rgba[i*4+2], rgba[i*4+1], rgba[i*4+0], rgba[i*4+3] };
        fwrite(bgra, 1, 4, f);
    }
    fclose(f);
    MELLO_LOG_INFO(TAG, "Saved debug frame: %s (%ux%u)", path, w, h);
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

    // 1. Capture
    capture_ = create_capture_source(source);
    if (!capture_ || !capture_->initialize(device_, source)) {
        MELLO_LOG_ERROR(TAG, "Failed to initialize capture source");
        return false;
    }

#ifdef _WIN32
    // NV12 requires even dimensions (chroma plane is half-res)
    uint32_t cap_w = capture_->width()  & ~1u;
    uint32_t cap_h = capture_->height() & ~1u;

    // If the config specifies a target resolution smaller than the capture,
    // the color converter will downscale in the same GPU pass as BGRA→NV12.
    uint32_t target_w = (config.width  > 0 && config.width  < cap_w) ? (config.width  & ~1u) : cap_w;
    uint32_t target_h = (config.height > 0 && config.height < cap_h) ? (config.height & ~1u) : cap_h;
    encode_w_ = target_w;
    encode_h_ = target_h;
    uint32_t enc_w = encode_w_;
    uint32_t enc_h = encode_h_;

    // 2. Video preprocessor: BGRA→NV12 color conversion + GPU downscale
    preprocessor_ = std::make_unique<VideoPreprocessor>();
    if (!preprocessor_->initialize(device_, cap_w, cap_h, enc_w, enc_h)) {
        MELLO_LOG_ERROR(TAG, "Failed to initialize video preprocessor");
        return false;
    }

    // 3. Encoder
    EncoderConfig enc_config{};
    enc_config.width         = enc_w;
    enc_config.height        = enc_h;
    enc_config.fps           = config.fps;
    enc_config.bitrate_kbps  = config.bitrate_kbps;
    enc_config.keyframe_interval = 120;
    enc_config.codec         = VideoCodec::H264;

    encoder_ = create_best_encoder(device_, enc_config);
    if (!encoder_) {
        MELLO_LOG_ERROR(TAG, "No encoder available");
        return false;
    }

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
    last_emitted_us_ = 0;

    encode_thread_ = std::thread(&VideoPipeline::encode_thread_func, this);

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
    uint32_t target_w = (config.width  > 0 && config.width  < cap_w) ? (config.width  & ~1u) : cap_w;
    uint32_t target_h = (config.height > 0 && config.height < cap_h) ? (config.height & ~1u) : cap_h;
    encode_w_ = target_w;
    encode_h_ = target_h;

    EncoderConfig enc_config{};
    enc_config.width         = encode_w_;
    enc_config.height        = encode_h_;
    enc_config.fps           = config.fps;
    enc_config.bitrate_kbps  = config.bitrate_kbps;
    enc_config.keyframe_interval = 120;
    enc_config.codec         = VideoCodec::H264;

    encoder_ = create_best_encoder(device_, enc_config);
    if (!encoder_) {
        MELLO_LOG_ERROR(TAG, "No encoder available");
        return false;
    }

    host_running_    = true;
    host_start_time_ = now_us();
    frames_encoded_  = 0;
    frames_captured_.store(0, std::memory_order_relaxed);
    last_capture_us_.store(host_start_time_, std::memory_order_relaxed);
    encode_ms_mean_.store(0.0, std::memory_order_relaxed);
    output_fps_.store(0, std::memory_order_relaxed);
    last_emitted_us_ = 0;

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

    if (capture_)   capture_->stop();

    // Wake and join the encode thread before shutting down encoder/preprocessor
    eq_cv_.notify_all();
    if (encode_thread_.joinable()) encode_thread_.join();

    if (encoder_)   encoder_->shutdown();
#ifdef _WIN32
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
    preprocessor_.reset();
#endif
}

#ifdef __APPLE__
void VideoPipeline::on_captured_frame(void* cv_pixel_buffer, uint64_t timestamp_us) {
    if (!host_running_.load()) return;

    frames_captured_.fetch_add(1, std::memory_order_relaxed);
    last_capture_us_.store(now_us(), std::memory_order_relaxed);

    if (!decimation_accepts(timestamp_us, last_emitted_us_,
                            output_fps_.load(std::memory_order_relaxed),
                            config_.fps)) {
        return;
    }
    last_emitted_us_ = timestamp_us;

    EncodedPacket packet{};
    if (encoder_->encode(cv_pixel_buffer, packet)) {
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

void VideoPipeline::request_keyframe() {
    if (encoder_) encoder_->request_keyframe();
}

// NOTE for future adaptive bitrate/framerate: avoid reconfiguring the encoder's
// target FPS based on observed/achieved FPS. If frames are dropped (by the
// transport or a frame dropper), lowering the target FPS gives each remaining
// frame a larger bit budget, which increases per-frame size, which triggers
// MORE drops — a cascade that can halve the frame rate permanently. Instead,
// keep the target FPS constant and leave headroom between target and actual
// bitrate so the encoder naturally recovers after transient drops.
void VideoPipeline::set_bitrate(uint32_t kbps) {
    if (encoder_) encoder_->set_bitrate(kbps);
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
    if (encoder_) {
        encoder_->set_framerate(target == 0 ? capture_fps : target);
    }
    MELLO_LOG_INFO(TAG, "output framerate %u -> %u fps (capture %u)",
                   previous == 0 ? capture_fps : previous,
                   target == 0 ? capture_fps : target,
                   capture_fps);
}

// True when this captured frame should be passed downstream under the current
// decimation target. Uses a half-interval tolerance for the same reason the DXGI
// throttle does: capture lands on vsync boundaries, so demanding a full interval
// would systematically reject the frame that is closest to the deadline and
// deliver 20fps when 30 was asked for.
bool VideoPipeline::decimation_accepts(uint64_t timestamp_us, uint64_t last_emitted_us,
                                       uint32_t output_fps, uint32_t capture_fps) {
    if (output_fps == 0) {
        return true;
    }
    if (last_emitted_us == 0 || timestamp_us < last_emitted_us) {
        return true; // first frame, or a clock reset
    }

    const uint64_t interval_us = 1'000'000ULL / output_fps;
    // Tolerance is half a *source* interval, matching the DXGI throttle's
    // `target_interval - half_vsync`. Half the *target* interval looks
    // equivalent and is not: at 60->30 it equals a whole source frame, so every
    // frame clears the deadline and no decimation happens at all.
    const uint64_t tolerance_us =
        capture_fps > 0 ? (1'000'000ULL / capture_fps) / 2 : 0;
    const uint64_t deadline_us =
        interval_us > tolerance_us ? interval_us - tolerance_us : 0;
    return (timestamp_us - last_emitted_us) >= deadline_us;
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

    if (capture_)  out.capture_backend = capture_->backend_name();
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
    if (!decimation_accepts(timestamp_us, last_emitted_us_,
                            output_fps_.load(std::memory_order_relaxed),
                            config_.fps)) {
        return;
    }
    last_emitted_us_ = timestamp_us;

    if (capture_ && capture_->consume_swap_event()) {
        MELLO_LOG_WARN(TAG, "Capture backend swap detected, forcing keyframe");
        request_keyframe();
    }

    // Preprocess on the capture thread (fast GPU blit), then enqueue for encode
    auto t0 = std::chrono::steady_clock::now();
    auto result = preprocessor_->convert(texture);
    auto t1 = std::chrono::steady_clock::now();
    last_convert_ms_ = std::chrono::duration<double, std::milli>(t1 - t0).count();

    if (!result.texture) {
        MELLO_LOG_WARN(TAG, "on_captured_frame: convert() returned null");
        return;
    }

    // Enqueue for the encode thread (bounded, drop oldest on overflow)
    {
        std::lock_guard<std::mutex> lock(eq_mutex_);
        if (eq_count_ == ENCODE_QUEUE_CAP) {
            // Drop the oldest entry to bound latency
            eq_tail_ = (eq_tail_ + 1) % ENCODE_QUEUE_CAP;
            eq_count_--;
            eq_drops_++;
        }
        encode_queue_[eq_head_] = {result.texture, timestamp_us};
        eq_head_ = (eq_head_ + 1) % ENCODE_QUEUE_CAP;
        eq_count_++;
    }
    eq_cv_.notify_one();
}

void VideoPipeline::encode_thread_func() {
    while (true) {
        EncodeJob job{};
        {
            std::unique_lock<std::mutex> lock(eq_mutex_);
            eq_cv_.wait(lock, [this] { return eq_count_ > 0 || !host_running_.load(); });
            if (eq_count_ == 0 && !host_running_.load()) break;
            job = encode_queue_[eq_tail_];
            eq_tail_ = (eq_tail_ + 1) % ENCODE_QUEUE_CAP;
            eq_count_--;
        }

        auto t0 = std::chrono::steady_clock::now();
        EncodedPacket packet{};
        if (encoder_->encode(job.texture, packet)) {
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

            maybe_reduce_encoder_cost();

            if (packet_cb_) {
                packet_cb_(packet.data.data(), packet.data.size(), packet.is_keyframe, job.timestamp_us);
            }
        }
    }
}

// Runs on the encode thread, immediately after a frame is encoded, so the
// measurement and the reconfigure share one thread and no extra locking.
void VideoPipeline::maybe_reduce_encoder_cost() {
    if (!encoder_ || encoder_->cost_tier() >= 2) {
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
        encoder_->reduce_cost_tier();
    }
    // Reset the window either way: after a downgrade so the next decision
    // measures the new configuration, and after a healthy window so old drops
    // cannot accumulate into a false positive later.
    cost_window_start_captured_ = captured;
    cost_window_start_drops_    = drops;
}
#endif

// ─────────────────────────────────────────────────────────────────────────────
// VIEWER SIDE
// ─────────────────────────────────────────────────────────────────────────────

bool VideoPipeline::start_viewer(const PipelineConfig& config, FrameCallback on_frame) {
    if (viewer_running_.load()) {
        MELLO_LOG_WARN(TAG, "Viewer pipeline already running");
        return false;
    }

    if (!init_device()) return false;

    config_   = config;
    frame_cb_ = std::move(on_frame);

#ifdef _WIN32
    // Decoder
    DecoderConfig dec_config{};
    dec_config.width  = config.width;
    dec_config.height = config.height;
    dec_config.codec  = VideoCodec::H264;

    decoder_ = create_best_decoder(device_, dec_config);
    if (!decoder_) {
        MELLO_LOG_ERROR(TAG, "No decoder available");
        return false;
    }

    // Staging texture for VRAM → CPU handoff (format matches decoder output)
    staging_ = std::make_unique<StagingTexture>();
    DXGI_FORMAT frame_fmt = decoder_->frame_format();
    uint32_t uv_offset = decoder_->coded_height();
    const bool enable_cpu_readback = static_cast<bool>(frame_cb_);
    if (!staging_->initialize(device_, config.width, config.height, frame_fmt, uv_offset, enable_cpu_readback)) {
        MELLO_LOG_ERROR(TAG, "Failed to initialize staging texture");
        return false;
    }
#elif defined(__APPLE__)
    DecoderConfig dec_config{};
    dec_config.width  = config.width;
    dec_config.height = config.height;
    dec_config.codec  = VideoCodec::H264;

    decoder_ = create_best_decoder(device_, dec_config);
    if (!decoder_) {
        MELLO_LOG_ERROR(TAG, "No decoder available");
        return false;
    }
#endif

    if (frame_cb_) {
        rgba_buf_.resize(static_cast<size_t>(config.width) * config.height * 4);
    } else {
        rgba_buf_.clear();
        rgba_buf_.shrink_to_fit();
    }

    viewer_running_    = true;
    viewer_start_time_ = now_us();
    frames_decoded_    = 0;
    decode_errors_     = 0;
    last_present_us_   = 0;

    // Async decode: feed_packet only enqueues; decode_thread_ runs
    // decoder_->decode()/get_frame(). Reset stale queue state from any
    // previous run, then launch.
    if (decoder_) {
        {
            std::lock_guard<std::mutex> lock(decode_jobs_mutex_);
            decode_jobs_.clear();
        }
        decode_thread_ = std::thread([this] { decode_thread_func(); });
    }

    MELLO_LOG_INFO(TAG, "Viewer pipeline starting: decoder=%s codec=H264 res=%ux%u",
        decoder_ ? decoder_->name() : "none",
        config.width, config.height);

    return true;
}

void VideoPipeline::set_native_frame_callback(NativeFrameCallback on_native_frame) {
    native_frame_cb_ = std::move(on_native_frame);
}

void VideoPipeline::stop_viewer() {
    if (!viewer_running_.load()) return;
    viewer_running_ = false;

    // Wake the decode thread and join it BEFORE shutting down the decoder —
    // decoder_->decode()/get_frame() may only run on the decode thread.
    decode_jobs_cv_.notify_all();
    if (decode_thread_.joinable()) {
        // stop_viewer is never called from the decode thread today, but
        // guard against self-join anyway.
        if (decode_thread_.get_id() == std::this_thread::get_id())
            decode_thread_.detach();
        else
            decode_thread_.join();
    }

    uint64_t uptime_s = (now_us() - viewer_start_time_) / 1'000'000;

    MELLO_LOG_INFO(TAG, "Viewer pipeline stopped: uptime=%llus frames_decoded=%llu decode_errors=%llu",
        uptime_s, frames_decoded_.load(), decode_errors_.load());

    if (decoder_) decoder_->shutdown();
#ifdef _WIN32
    if (staging_) staging_->shutdown();
#endif
    decoder_.reset();
#ifdef _WIN32
    staging_.reset();
#endif

    // Drop any jobs the decode thread did not consume
    {
        std::lock_guard<std::mutex> lock(decode_jobs_mutex_);
        decode_jobs_.clear();
    }

    // Drain any remaining frames in the ring buffer
    while (decoded_ring_depth() > 0) {
#ifdef __APPLE__
        void* buf = pop_decoded();
        if (buf) CVPixelBufferRelease((CVPixelBufferRef)buf);
#else
        pop_decoded();
#endif
    }

    rgba_buf_.clear();
}

// feed_packet runs on the caller's (client tick) thread and is O(copy):
// it enqueues the packet and returns. Decode errors from the async decode
// thread surface via the decode_errors_ counter only; the return value is
// "accepted", matching how callers already treat it.
bool VideoPipeline::feed_packet(const uint8_t* data, size_t size, bool is_keyframe) {
    if (!viewer_running_.load() || !decoder_) return false;
    {
        std::lock_guard<std::mutex> lock(decode_jobs_mutex_);
        if (decode_jobs_.size() >= DECODE_QUEUE_CAP) {
            // Shed load: drop the oldest non-keyframe job (never the newest
            // keyframe — it's the recovery point).
            auto drop = std::find_if(decode_jobs_.begin(), decode_jobs_.end(),
                [](const DecodeJob& j) { return !j.is_keyframe; });
            if (drop != decode_jobs_.end()) {
                decode_jobs_.erase(drop);
            } else {
                decode_jobs_.pop_front();
            }
        }
        DecodeJob job;
        job.bytes.assign(data, data + size);
        job.is_keyframe = is_keyframe;
        decode_jobs_.push_back(std::move(job));
    }
    decode_jobs_cv_.notify_one();
    return true;
}

// Runs on decode_thread_ only. decoder_->decode() and get_frame()/
// get_frame_buffer() must ONLY ever execute here.
void VideoPipeline::decode_thread_func() {
    for (;;) {
        DecodeJob job;
        {
            std::unique_lock<std::mutex> lock(decode_jobs_mutex_);
            decode_jobs_cv_.wait(lock, [this] {
                return !decode_jobs_.empty() || !viewer_running_.load();
            });
            if (!viewer_running_.load() && decode_jobs_.empty()) return;
            job = std::move(decode_jobs_.front());
            decode_jobs_.pop_front();
        }

        DecodeFeedResult result = decoder_->decode(job.bytes.data(), job.bytes.size(), job.is_keyframe);
        if (result == DecodeFeedResult::Error) {
            decode_errors_++;
            continue;
        }
        if (result == DecodeFeedResult::Accepted) continue;

#ifdef _WIN32
        ID3D11Texture2D* decoded = decoder_->get_frame();
        if (!decoded) {
            MELLO_LOG_ERROR(TAG, "Decoder %s reported a frame without a texture", decoder_->name());
            decode_errors_++;
            continue;
        }
        push_decoded(decoded);
#elif defined(__APPLE__)
        void* decoded = decoder_->get_frame_buffer();
        if (!decoded) {
            MELLO_LOG_ERROR(TAG, "Decoder %s reported a frame without a pixel buffer", decoder_->name());
            decode_errors_++;
            continue;
        }
        CVPixelBufferRetain((CVPixelBufferRef)decoded);
        push_decoded(decoded);
#endif

        frames_decoded_++;
        if (frames_decoded_.load() % 300 == 0) {
            uint64_t uptime_s = (now_us() - viewer_start_time_) / 1'000'000;
            MELLO_LOG_INFO(TAG, "viewer: uptime=%llus decoded=%llu decode_errors=%llu dec=%s ring=%zu",
                uptime_s, frames_decoded_.load(), decode_errors_.load(), decoder_->name(), decoded_ring_depth());
        }
    }
}

bool VideoPipeline::jitter_should_present(size_t depth, uint64_t now_us_value) const {
    if (depth == 0) return false;
    if (depth >= JITTER_TARGET) return true;
    // depth == 1: present on cadence (~90% of frame interval since last
    // present) so a steady stream keeps a one-frame cushion, and after an
    // underrun the next frame is not artificially delayed.
    if (last_present_us_ == 0) return true;
    const uint32_t fps = config_.fps > 0 ? config_.fps : 60;
    const uint64_t interval_us = 1'000'000ULL / fps;
    return now_us_value - last_present_us_ >= interval_us * 9 / 10;
}

bool VideoPipeline::present_frame() {
#ifdef _WIN32
    if (!viewer_running_.load()) return false;

    if (!jitter_should_present(decoded_ring_depth(), now_us())) return false;

    ID3D11Texture2D* frame = pop_decoded();
    if (!frame) return false;
    if (decoder_) {
        decoder_->publish_d3d11_frame();
    }
    last_present_us_ = now_us();

    // Native GPU presenter path: first try direct decoded-texture handoff.
    if (native_frame_cb_) {
        void* direct_handle = decoder_ ? decoder_->shared_frame_handle() : nullptr;
        if (direct_handle) {
            DXGI_FORMAT frame_fmt = decoder_->shared_frame_format();
            uint32_t native_fmt = NATIVE_FMT_UNKNOWN;
            if (frame_fmt == DXGI_FORMAT_R8_UNORM) {
                native_fmt = NATIVE_FMT_R8_NV12_LAYOUT;
            } else if (frame_fmt == DXGI_FORMAT_NV12) {
                native_fmt = NATIVE_FMT_NV12;
            } else if (frame_fmt == DXGI_FORMAT_R8G8B8A8_UNORM) {
                native_fmt = NATIVE_FMT_RGBA8;
            }

            if (native_fmt != NATIVE_FMT_UNKNOWN) {
                uint32_t uv_offset = decoder_->shared_frame_uv_offset();
                if (uv_offset == 0) uv_offset = config_.height;
                if (native_fmt == NATIVE_FMT_RGBA8) {
                    native_frame_cb_(
                        direct_handle,
                        config_.width,
                        config_.height,
                        native_fmt,
                        uv_offset,
                        now_us()
                    );
                    return true;
                }
            }
        }

        // Fallback: GPU convert to shared RGBA surface for native handle callback.
        if (staging_->shared_rgba_handle()) {
            staging_->copy_from(frame, false);
            native_frame_cb_(
                staging_->shared_rgba_handle(),
                config_.width,
                config_.height,
                NATIVE_FMT_RGBA8,
                config_.height,
                now_us()
            );
            return true;
        }
    }

    if (!frame_cb_) {
        // Zero-copy native callback is required in this mode; no CPU RGBA fallback.
        return false;
    }

    staging_->copy_from(frame, true);
    staging_->read_rgba(rgba_buf_.data());

    if (frames_decoded_.load() < 2 && getenv("MELLO_DUMP_FRAMES")) {
        char path[256];
        snprintf(path, sizeof(path), "mello_viewer_frame_%llu.bmp", frames_decoded_.load());
        save_bmp_rgba(path, rgba_buf_.data(), config_.width, config_.height);
    }

    if (frame_cb_) {
        frame_cb_(rgba_buf_.data(), config_.width, config_.height, now_us());
    }

    return true;
#elif defined(__APPLE__)
    if (!viewer_running_.load()) return false;

    {
        if (!jitter_should_present(decoded_ring_depth(), now_us())) return false;
    }

    void* popped = pop_decoded();
    if (!popped) return false;
    last_present_us_ = now_us();

    CVPixelBufferRef pb = (CVPixelBufferRef)popped;
    CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);

    uint32_t w = (uint32_t)CVPixelBufferGetWidth(pb);
    uint32_t h = (uint32_t)CVPixelBufferGetHeight(pb);
    size_t stride = CVPixelBufferGetBytesPerRow(pb);
    uint8_t* base = (uint8_t*)CVPixelBufferGetBaseAddress(pb);

    if (base && w > 0 && h > 0) {
        size_t needed = static_cast<size_t>(w) * h * 4;
        if (rgba_buf_.size() < needed) rgba_buf_.resize(needed);

        vImage_Buffer src  = { base, h, w, stride };
        vImage_Buffer dest = { rgba_buf_.data(), h, w, w * 4u };
        const uint8_t permuteMap[4] = {2, 1, 0, 3};
        vImagePermuteChannels_ARGB8888(&src, &dest, permuteMap, kvImageNoFlags);

        if (frames_decoded_.load() <= 2 && getenv("MELLO_DUMP_FRAMES")) {
            char path[256];
            snprintf(path, sizeof(path), "mello_viewer_frame_%llu.bmp", frames_decoded_.load());
            save_bmp_rgba(path, rgba_buf_.data(), w, h);
        }

        if (frame_cb_) {
            frame_cb_(rgba_buf_.data(), w, h, now_us());
        }
    }

    CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
    CVPixelBufferRelease(pb);
    return true;
#else
    return false;
#endif
}

// ─────────────────────────────────────────────────────────────────────────────
// CURSOR
// ─────────────────────────────────────────────────────────────────────────────

bool VideoPipeline::get_cursor_packet(uint8_t* buf, size_t* size) {
    if (!capture_) return false;

    CursorData cd{};
    if (!capture_->get_cursor(cd)) return false;

    CursorState cs{};
    cs.x       = cd.x;
    cs.y       = cd.y;
    cs.visible = cd.visible;
    cs.shape_w = cd.shape_w;
    cs.shape_h = cd.shape_h;
    cs.shape_rgba = std::move(cd.shape_rgba);

    size_t written = serialize_cursor_packet(cs, cd.shape_changed, buf, *size);
    if (written == 0) return false;
    *size = written;
    return true;
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
