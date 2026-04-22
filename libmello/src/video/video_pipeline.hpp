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
#include <array>

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
    using NativeFrameCallback = std::function<void(
        void* shared_handle,
        uint32_t w,
        uint32_t h,
        uint32_t format,
        uint32_t uv_y_offset,
        uint64_t ts
    )>;

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
    size_t decode_queue_depth() const { return decoded_ring_count_; }
    void set_native_frame_callback(NativeFrameCallback on_native_frame);
    void set_native_frame_mirror_rgba(bool enabled);

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
    NativeFrameCallback native_frame_cb_;
    bool native_frame_mirror_rgba_ = false;
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

    static constexpr size_t DECODED_RING_CAP = 3;
#ifdef _WIN32
    std::array<ID3D11Texture2D*, DECODED_RING_CAP> decoded_ring_{};
#elif defined(__APPLE__)
    std::array<void*, DECODED_RING_CAP> decoded_ring_{}; // CVPixelBufferRef
#endif
    size_t decoded_ring_head_ = 0; // next write slot
    size_t decoded_ring_tail_ = 0; // next read slot
    size_t decoded_ring_count_ = 0;

    void push_decoded(
#ifdef _WIN32
        ID3D11Texture2D* frame
#elif defined(__APPLE__)
        void* frame
#endif
    );

#ifdef _WIN32
    ID3D11Texture2D* pop_decoded();
#elif defined(__APPLE__)
    void* pop_decoded();
#endif
};

} // namespace mello::video
