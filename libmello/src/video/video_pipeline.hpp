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
#include <thread>
#include <condition_variable>
#include <deque>

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

    // Host-side diagnostics beyond the encoder's own contract. String members
    // are borrowed and valid only while this pipeline lives; callers copy them
    // out immediately.
    struct HostTelemetry {
        uint64_t    frames_captured     = 0;
        uint32_t    capture_idle_ms     = 0;
        uint32_t    encode_queue_depth  = 0;
        uint64_t    encode_queue_drops  = 0;
        float       convert_ms          = 0.0f;
        float       encode_ms           = 0.0f;
        const char* capture_backend     = "";
        const char* encoder_name        = "";
        const char* gpu_name            = "";
    };
    void get_host_telemetry(HostTelemetry& out) const;

    // VIEWER SIDE
    bool start_viewer(const PipelineConfig& config, FrameCallback on_frame);
    void stop_viewer();
    bool feed_packet(const uint8_t* data, size_t size, bool is_keyframe);
    bool present_frame();
    // Decode input backlog: compressed jobs waiting for the decode thread.
    // This is what the viewer backlog guard (via FFI
    // mello_stream_viewer_decode_queue_depth) and probe telemetry want.
    size_t decode_queue_depth() const {
        std::lock_guard<std::mutex> lock(decode_jobs_mutex_);
        return decode_jobs_.size();
    }
    // Decoded frames waiting in the present ring (used by the present path).
    size_t decoded_ring_depth() const {
        std::lock_guard<std::mutex> lock(decoded_ring_mutex_);
        return decoded_ring_count_;
    }
    void set_native_frame_callback(NativeFrameCallback on_native_frame);

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
    PipelineConfig  config_{};

    std::atomic<bool> host_running_{false};
    std::atomic<bool> viewer_running_{false};

    mutable std::mutex cursor_mutex_;
    CursorState        viewer_cursor_;

    // Encode dimensions (even-aligned from capture)
    uint32_t encode_w_ = 0;
    uint32_t encode_h_ = 0;

    // Async encode thread: capture enqueues, encode thread dequeues
    struct EncodeJob {
#ifdef _WIN32
        ID3D11Texture2D* texture = nullptr;
#elif defined(__APPLE__)
        void* texture = nullptr;
#endif
        uint64_t timestamp_us = 0;
    };
    static constexpr size_t ENCODE_QUEUE_CAP = 2;
    std::array<EncodeJob, ENCODE_QUEUE_CAP> encode_queue_{};
    size_t eq_head_ = 0; // next write
    size_t eq_tail_ = 0; // next read
    size_t eq_count_ = 0;
    uint64_t eq_drops_ = 0;
    // mutable so the const telemetry getter can read the queue counters under
    // the same lock the capture/encode threads use, rather than racing them.
    mutable std::mutex eq_mutex_;
    std::condition_variable eq_cv_;
    std::thread encode_thread_;
    void encode_thread_func();

    // Stats
    uint64_t host_start_time_  = 0;
    uint64_t frames_encoded_   = 0;
    // Capture arrivals, counted before any encode work so a stalled capture is
    // distinguishable from a stalled encoder. Written on the capture thread and
    // read from the stats caller — atomic. Rate/stall windows are derived by the
    // caller; keeping the time math out of here avoids a second clock policy.
    std::atomic<uint64_t> frames_captured_{0};
    std::atomic<uint64_t> last_capture_us_{0};
    double   last_convert_ms_  = 0;
    double   last_encode_ms_   = 0;
    uint64_t viewer_start_time_ = 0;
    // Written on the decode thread, read on others (stop_viewer, present
    // debug dumps) — atomic.
    std::atomic<uint64_t> frames_decoded_{0};
    std::atomic<uint64_t> decode_errors_{0};

    std::vector<uint8_t> rgba_buf_;

    static constexpr size_t DECODED_RING_CAP = 3;
    mutable std::mutex decoded_ring_mutex_;
#ifdef _WIN32
    std::array<ID3D11Texture2D*, DECODED_RING_CAP> decoded_ring_{};
#elif defined(__APPLE__)
    std::array<void*, DECODED_RING_CAP> decoded_ring_{}; // CVPixelBufferRef
#endif
    size_t decoded_ring_head_ = 0; // next write slot
    size_t decoded_ring_tail_ = 0; // next read slot
    size_t decoded_ring_count_ = 0;

    // Async decode: feed_packet enqueues, decode_thread_ dequeues and runs
    // decoder_->decode()/get_frame() — those may ONLY ever run on
    // decode_thread_.
    struct DecodeJob {
        std::vector<uint8_t> bytes;
        bool is_keyframe = false;
    };
    static constexpr size_t DECODE_QUEUE_CAP = 8;
    mutable std::mutex decode_jobs_mutex_;
    std::condition_variable decode_jobs_cv_;
    std::deque<DecodeJob> decode_jobs_;
    std::thread decode_thread_;
    void decode_thread_func();

    // Continuous jitter regulator: present immediately when the ring holds
    // >= JITTER_TARGET frames (we are ahead), otherwise at frame cadence
    // (~90% of the configured frame interval since the last present). No
    // one-shot priming latch — underflow recovers without a stall.
    static constexpr size_t JITTER_TARGET = 2;
    uint64_t last_present_us_ = 0;

    bool jitter_should_present(size_t depth, uint64_t now_us) const;

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
