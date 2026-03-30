#pragma once
#include "graphics_device.hpp"
#include "capture_source.hpp"
#include "encoder.hpp"
#include "decoder.hpp"
#include "cursor.hpp"
#include <memory>
#include <functional>
#include <mutex>
#include <atomic>

#ifdef _WIN32
#include "video_preprocessor.hpp"
#include "staging_texture.hpp"
#endif

namespace mello::video {

struct PipelineConfig {
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t bitrate_kbps;
    bool     low_latency = true;
};

class VideoPipeline {
public:
    using PacketCallback = std::function<void(const uint8_t* data, size_t size, bool is_keyframe, uint64_t ts)>;
    using FrameCallback  = std::function<void(const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t ts)>;

    VideoPipeline();
    ~VideoPipeline();

    bool init_device();

    // HOST SIDE
    bool start_host(const CaptureSourceDesc& source, const PipelineConfig& config, PacketCallback on_packet);
    void stop_host();
    void get_host_resolution(uint32_t& w, uint32_t& h) const;
    void request_keyframe();
    void set_bitrate(uint32_t kbps);
    void get_stats(EncoderStats& out) const;

    // VIEWER SIDE
    bool start_viewer(const PipelineConfig& config, FrameCallback on_frame);
    void stop_viewer();
    bool feed_packet(const uint8_t* data, size_t size, bool is_keyframe);
    bool present_frame();

    // CURSOR
    bool get_cursor_packet(uint8_t* buf, size_t* size);
    void apply_cursor_packet(const uint8_t* buf, size_t size);
    void get_cursor_state(CursorState& out) const;

    // Info
    const GraphicsDevice& device() const { return device_; }
    bool is_host_running()   const { return host_running_.load(); }
    bool is_viewer_running() const { return viewer_running_.load(); }
    bool encoder_available() const;

private:
#ifdef _WIN32
    void on_captured_frame(ID3D11Texture2D* texture, uint64_t timestamp_us);
#elif defined(__APPLE__)
    void on_captured_frame(void* cv_pixel_buffer, uint64_t timestamp_us);
#endif

    GraphicsDevice                       device_{};
    std::unique_ptr<CaptureSource>       capture_;
    std::unique_ptr<Encoder>             encoder_;
    std::unique_ptr<Decoder>             decoder_;
#ifdef _WIN32
    std::unique_ptr<VideoPreprocessor>   preprocessor_;
    std::unique_ptr<StagingTexture>      staging_;
#endif

    PacketCallback  packet_cb_;
    FrameCallback   frame_cb_;
    PipelineConfig  config_{};

    std::atomic<bool> host_running_{false};
    std::atomic<bool> viewer_running_{false};

    mutable std::mutex cursor_mutex_;
    CursorState        viewer_cursor_;

    // Encode dimensions (even-aligned from capture)
    uint32_t encode_w_ = 0;
    uint32_t encode_h_ = 0;

    // Stats
    uint64_t host_start_time_  = 0;
    uint64_t frames_encoded_   = 0;
    uint64_t viewer_start_time_ = 0;
    uint64_t frames_decoded_   = 0;
    uint64_t frames_dropped_   = 0;

    std::vector<uint8_t> rgba_buf_;
#ifdef _WIN32
    ID3D11Texture2D*     latest_decoded_ = nullptr;
#elif defined(__APPLE__)
    void*                latest_decoded_ = nullptr; // CVPixelBufferRef
#endif
};

} // namespace mello::video
