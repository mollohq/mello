#include "video_pipeline.hpp"
#include "encoder_factory.hpp"
#include "decoder_factory.hpp"
#include "../util/log.hpp"
#include <chrono>
#include <cstring>

namespace mello::video {

static constexpr const char* TAG = "video/pipeline";

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
#endif
        device_.handle = nullptr;
    }
}

bool VideoPipeline::init_device() {
    if (device_.handle) return true;
    device_ = create_d3d11_device();
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
    encode_w_ = capture_->width()  & ~1u;
    encode_h_ = capture_->height() & ~1u;
    uint32_t enc_w = encode_w_;
    uint32_t enc_h = encode_h_;

    // 2. Color converter
    converter_ = std::make_unique<ColorConverter>();
    if (!converter_->initialize(device_, enc_w, enc_h)) {
        MELLO_LOG_ERROR(TAG, "Failed to initialize color converter");
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

    // 4. Start capture — frames flow through on_captured_frame
    auto self = this;
    if (!capture_->start(config.fps, [self](ID3D11Texture2D* tex, uint64_t ts) {
        self->on_captured_frame(tex, ts);
    })) {
        MELLO_LOG_ERROR(TAG, "Failed to start capture");
        return false;
    }
#endif

    host_running_    = true;
    host_start_time_ = now_us();
    frames_encoded_  = 0;

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
    if (encoder_)   encoder_->shutdown();
    if (converter_) converter_->shutdown();

    uint64_t uptime_s = (now_us() - host_start_time_) / 1'000'000;
    EncoderStats stats{};
    if (encoder_) encoder_->get_stats(stats);

    MELLO_LOG_INFO(TAG, "Host pipeline stopped: uptime=%llus frames_encoded=%llu keyframes=%u bytes_out=%lluMB",
        uptime_s, frames_encoded_, stats.keyframes_sent, stats.bytes_sent / (1024 * 1024));

    capture_.reset();
    encoder_.reset();
    converter_.reset();
}

void VideoPipeline::get_host_resolution(uint32_t& w, uint32_t& h) const {
    w = encode_w_;
    h = encode_h_;
}

void VideoPipeline::request_keyframe() {
    if (encoder_) encoder_->request_keyframe();
}

void VideoPipeline::set_bitrate(uint32_t kbps) {
    if (encoder_) encoder_->set_bitrate(kbps);
}

void VideoPipeline::get_stats(EncoderStats& out) const {
    if (encoder_) encoder_->get_stats(out);
    else memset(&out, 0, sizeof(out));
}

bool VideoPipeline::encoder_available() const {
    if (!device_.handle) {
        auto* self = const_cast<VideoPipeline*>(this);
        if (!self->init_device()) return false;
    }
    auto encoders = enumerate_encoders(device_);
    return !encoders.empty();
}

#ifdef _WIN32
void VideoPipeline::on_captured_frame(ID3D11Texture2D* texture, uint64_t timestamp_us) {
    if (!host_running_.load()) return;

    if (frames_encoded_ < 3) {
        D3D11_TEXTURE2D_DESC cap_desc{};
        texture->GetDesc(&cap_desc);
        MELLO_LOG_DEBUG(TAG, "on_captured_frame[%llu]: capture tex fmt=%u %ux%u bind=0x%X",
            frames_encoded_, cap_desc.Format, cap_desc.Width, cap_desc.Height, cap_desc.BindFlags);
    }

    // Capture → Color Convert → Encode → Packet callback
    ID3D11Texture2D* nv12 = converter_->convert(texture);
    if (!nv12) {
        MELLO_LOG_WARN(TAG, "on_captured_frame: convert() returned null");
        return;
    }

    EncodedPacket packet{};
    if (encoder_->encode(nv12, packet)) {
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
    if (!staging_->initialize(device_, config.width, config.height, frame_fmt)) {
        MELLO_LOG_ERROR(TAG, "Failed to initialize staging texture");
        return false;
    }
#endif

    rgba_buf_.resize(static_cast<size_t>(config.width) * config.height * 4);

    viewer_running_    = true;
    viewer_start_time_ = now_us();
    frames_decoded_    = 0;
    frames_dropped_    = 0;

    MELLO_LOG_INFO(TAG, "Viewer pipeline starting: decoder=%s codec=H264 res=%ux%u",
        decoder_ ? decoder_->name() : "none",
        config.width, config.height);

    return true;
}

void VideoPipeline::stop_viewer() {
    if (!viewer_running_.load()) return;
    viewer_running_ = false;

    uint64_t uptime_s = (now_us() - viewer_start_time_) / 1'000'000;

    MELLO_LOG_INFO(TAG, "Viewer pipeline stopped: uptime=%llus frames_decoded=%llu frames_dropped=%llu",
        uptime_s, frames_decoded_, frames_dropped_);

    if (decoder_) decoder_->shutdown();
    if (staging_) staging_->shutdown();
    decoder_.reset();
    staging_.reset();
    rgba_buf_.clear();
}

bool VideoPipeline::feed_packet(const uint8_t* data, size_t size, bool is_keyframe) {
    if (!viewer_running_.load() || !decoder_) return false;

#ifdef _WIN32
    if (!decoder_->decode(data, size, is_keyframe)) {
        frames_dropped_++;
        return false;
    }

    ID3D11Texture2D* decoded = decoder_->get_frame();
    if (decoded) latest_decoded_ = decoded;

    frames_decoded_++;

    if (frames_decoded_ % 300 == 0) {
        uint64_t uptime_s = (now_us() - viewer_start_time_) / 1'000'000;
        MELLO_LOG_INFO(TAG, "viewer: uptime=%llus decoded=%llu dropped=%llu dec=%s",
            uptime_s, frames_decoded_, frames_dropped_, decoder_->name());
    }
#else
    (void)data; (void)size; (void)is_keyframe;
#endif

    return true;
}

bool VideoPipeline::present_frame() {
#ifdef _WIN32
    if (!viewer_running_.load() || !latest_decoded_) return false;

    staging_->copy_from(latest_decoded_);
    staging_->read_rgba(rgba_buf_.data());

    if (frame_cb_) {
        frame_cb_(rgba_buf_.data(), config_.width, config_.height, now_us());
    }

    latest_decoded_ = nullptr;
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
