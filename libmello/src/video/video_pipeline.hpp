#pragma once
#include "graphics_device.hpp"
#include "capture_source.hpp"
#include "encoder.hpp"
#include "encoder_control.hpp"
#include "video_viewer.hpp"
#include "cursor.hpp"
#include <memory>
#include <functional>
#include <mutex>
#include <atomic>
#include <future>
#include <array>
#include <thread>
#include <condition_variable>
#include <deque>

#ifdef _WIN32
#include "video_preprocessor.hpp"
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

    /// The encode size for a capture size and a preset height.
    ///
    /// The height is the preset height, the width follows the source aspect
    /// and is rounded to a multiple of 16. The size never goes above the
    /// capture size. When the size is over H.264 level 5.1 (36864 macroblocks
    /// per frame, 983040 per second), the height goes down until it fits and
    /// `level_capped` is set. Pure, so it is testable without a GPU.
    struct EncodeSize {
        uint32_t width = 0;
        uint32_t height = 0;
        bool     level_capped = false;
    };
    static EncodeSize encode_size_for(uint32_t capture_w, uint32_t capture_h,
                                      uint32_t preset_h, uint32_t fps);

    void request_keyframe();
    void set_bitrate(uint32_t kbps);
    /// Evidence gathered over one observation window, used to decide whether the
    /// encoder is failing to hold the frame budget.
    struct EncoderLoadSample {
        uint64_t frames_captured = 0;   // arrivals offered to the encoder
        uint64_t queue_drops     = 0;   // frames evicted unencoded (newest-wins)
        double   mean_encode_ms  = 0.0;
        uint32_t target_fps      = 60;
    };

    /// True when the encoder should give up a quality feature to keep up.
    ///
    /// Pure so it can be tested without a GPU — the machines that trigger it are
    /// exactly the ones we do not have. Two independent triggers:
    ///
    ///  - **Dropped frames.** The direct symptom. The encode queue evicts
    ///    silently, so any sustained drop rate means frames are already being
    ///    lost and no other signal will report it.
    ///  - **Encode time against budget.** Leading indicator. Sitting at or above
    ///    the frame interval means no headroom for a complex scene, and the
    ///    drops are about to start.
    ///
    /// Requires a minimum sample so a single hitch during startup or a scene cut
    /// cannot permanently downgrade quality.
    static bool encoder_is_overloaded(const EncoderLoadSample& sample);

    /// Retarget the encoded output framerate without restarting capture.
    ///
    /// Frames are decimated at the capture callback, before GPU colour
    /// conversion, so both convert and encode cost fall — which is the point on
    /// a host that cannot keep up. Capture itself keeps running at its own rate:
    /// its cadence is driven by the display, and reconfiguring each backend's
    /// throttle at runtime would be far more invasive for no extra saving.
    ///
    /// `fps` is clamped to the configured capture rate; 0 restores it.
    void set_output_fps(uint32_t fps);
    uint32_t output_fps() const { return output_fps_.load(std::memory_order_relaxed); }

    /// Change the encode size to the height `height` (a geometry rung of the
    /// stage 4 quality controller). The width follows the current source
    /// aspect (`encode_size_for`). Any thread; returns at once. The capture
    /// thread makes a new preprocessor at the new size at the next frame; the
    /// encode thread then changes the encoder's size (NVENC reconfigures in
    /// place, every other backend, and NVENC when the driver refuses, is
    /// re-initialized on the same device) and the next frame is an IDR with
    /// SPS and PPS. The old preprocessor is freed when the encoder no longer
    /// reads its textures. A request for the current size changes nothing,
    /// and a new source aspect needs a new request. Returns false when no
    /// host runs or the height is 0. Windows only; false on other platforms.
    bool set_encode_height(uint32_t height);
    /// Geometry changes applied since the host started.
    uint32_t geometry_changes() const { return geometry_changes_.load(std::memory_order_relaxed); }

    /// True when a captured frame should be passed downstream under the current
    /// decimation target. Static and pure so the cadence is testable without a
    /// capture device.
    /// `next_deadline_us` is advanced by exactly one output interval per accepted
    /// frame — never reset to the arrival time, which is what collapses the
    /// output rate back onto the source's quantisation. 0 starts a fresh cadence.
    static bool decimation_accepts(uint64_t timestamp_us, uint64_t& next_deadline_us,
                                   uint32_t output_fps);

    /// The rate frames are actually emitted at: the ladder's target when it has
    /// dropped a rung, otherwise the configured rate.
    ///
    /// Always a real number rather than "0 = unlimited". Capture backends deliver
    /// at the display's refresh rate and their own throttles have been measured
    /// over-delivering (83fps against a 60fps target on a real host), which the
    /// encoder then faithfully encodes — overshooting the bitrate by exactly the
    /// same ratio, because rate control budgets bits per frame from the framerate
    /// it was configured with. Clamping here does not depend on any backend
    /// throttle being correct.
    uint32_t effective_output_fps() const {
        const uint32_t ladder = output_fps_.load(std::memory_order_relaxed);
        return ladder != 0 ? ladder : config_.fps;
    }

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
        // Rolling mean over the recent window, not the last sample: encode time
        // swings by an order of magnitude frame to frame, so a single reading
        // says nothing about whether the encoder is keeping up.
        float       encode_ms_mean      = 0.0f;
        float       encode_submit_ms    = 0.0f;
        float       encode_wait_ms      = 0.0f;
        float       encode_lock_ms      = 0.0f;
        int         encoder_cost_tier   = 0;
        // Frames re-encoded from the last picture because capture delivered
        // nothing new. A quiet stream, not a dead capture.
        uint64_t    idle_repeat_frames  = 0;
        // Every capture method failed (process capture ladder exhausted).
        bool        capture_failed      = false;
        // What the capture is doing, for the streamer's and viewers' screens.
        uint32_t    capture_state       = 0;
        // The process behind the capture target has exited. Separate from
        // capture_state because it is final: every capture state recovers, a
        // quit game does not. Sticky; false for monitor capture.
        bool        target_exited       = false;
        // Capture method changes and reasons, oldest first.
        std::string capture_history;
        // Cumulative present-to-capture delay, 1 ms buckets.
        std::array<uint32_t, PresentDelayHistogram::kBuckets> present_delay_hist{};
        // The source size of the latest captured frame, and how many times it
        // changed after the first frame. The encode size stays fixed: the
        // whole source scales into it. The counter is the signal for a
        // geometry change (streaming-quality stage 4 reacts to it).
        uint32_t    source_width        = 0;
        uint32_t    source_height       = 0;
        uint32_t    source_size_changes = 0;
    };
    void get_host_telemetry(HostTelemetry& out) const;

    /// The active capture source, or nullptr when not hosting.
    /// Exposed so the stream audio pipeline can attach to backends that carry
    /// system audio alongside video (ScreenCaptureKit). Valid until stop_host().
    CaptureSource* capture() const { return capture_.get(); }

    // VIEWER SIDE (streaming-quality stage 3). The work runs in
    // ViewerPipeline (video_viewer.hpp): a decode thread fed by the viewer's
    // receive timeline, a 3-slot ring and a present thread that calls the
    // frame callbacks at each frame's render time.
    bool start_viewer(const PipelineConfig& config, FrameCallback on_frame);
    void stop_viewer();
    /// Take frames from the receive timeline of the viewer's peer.
    void attach_receive_timeline(ViewerPipeline::TimelineProvider provider);
    /// Without a timeline: decode this access unit now, show it at once.
    bool feed_packet(const uint8_t* data, size_t size, bool is_keyframe);
    /// True when the present thread showed a frame since the last call. The
    /// present thread calls the frame callbacks by itself.
    bool present_frame();
    /// Access units fed without a timeline that wait for decode.
    size_t decode_queue_depth() const;
    /// Decoded frames waiting in the ring for their render time.
    size_t decoded_ring_depth() const;
    void set_native_frame_callback(NativeFrameCallback on_native_frame);
    void get_viewer_stats(ViewerStats& out) const;

    // CURSOR
    /// The next cursor packet for the viewers, or false when there is nothing
    /// new. Only a backend whose frames have no pointer and must not get one
    /// (CursorMode::Packet, the hook) sends position and shape. The position
    /// is in encode pixels, and the shape is scaled as the frame is (at most
    /// kMaxCursorPacketSide a side), so the viewer draws it over the decoded
    /// picture as it is. When the stream moves to a backend that has the
    /// pointer in its frames, one hidden packet tells the viewer to stop
    /// drawing. A packet with a shape follows every shape change. Any one
    /// thread (the stream manager polls it).
    bool get_cursor_packet(uint8_t* buf, size_t* size);
    void apply_cursor_packet(const uint8_t* buf, size_t size);
    void get_cursor_state(CursorState& out) const;

    // Info
    const GraphicsDevice& device() const { return device_; }
    bool is_host_running()   const { return host_running_.load(); }
    /// True when `stop_host` had to abandon a capture thread stuck in the
    /// display driver. This pipeline owns objects that thread may still touch,
    /// so the caller must leak it instead of destroying it.
    bool abandoned() const { return abandoned_.load(std::memory_order_relaxed); }
    bool is_viewer_running() const { return viewer_running_.load(); }
    bool encoder_available() const;

private:
#ifdef _WIN32
    void on_captured_frame(ID3D11Texture2D* texture, uint64_t timestamp_us);
#elif defined(__APPLE__) || defined(__linux__)
    /// macOS: a CVPixelBufferRef. Linux: a CaptureFrame (capture thread).
    void on_captured_frame(void* cv_pixel_buffer, uint64_t timestamp_us);
#endif
#if defined(__linux__)
    /// Linux: no picture for a while; drain the encoder (capture thread).
    void on_capture_idle();
#endif

    GraphicsDevice                       device_{};
    std::unique_ptr<CaptureSource>       capture_;
    std::unique_ptr<Encoder>             encoder_;
    // Every call into encoder_ that can run beside the encode path goes
    // through this: encode, cost tiers, bitrate, framerate, keyframes.
    EncoderControl                       encoder_control_;
#ifdef _WIN32
    std::unique_ptr<VideoPreprocessor>   preprocessor_;
#endif
    std::unique_ptr<ViewerPipeline>      viewer_;

    PacketCallback  packet_cb_;
    NativeFrameCallback native_frame_cb_;
    PipelineConfig  config_{};

    std::atomic<bool> host_running_{false};
    std::atomic<bool> abandoned_{false};
    std::atomic<bool> viewer_running_{false};

    mutable std::mutex cursor_mutex_;
    CursorState        viewer_cursor_;

    // Host pointer. `cursor_frame_` belongs to the capture thread (drawn into
    // the frame for CursorMode::Composite). The packet state below belongs to
    // the get_cursor_packet caller, under `cursor_packet_mutex_`.
    CursorData           cursor_frame_;
    std::mutex           cursor_packet_mutex_;
    CursorData           cursor_packet_src_;
    CursorState          cursor_packet_shape_;      // RGBA, scaled to encode pixels
    uint32_t             cursor_packet_serial_ = 0;  // source serial of that shape
    uint32_t             cursor_packet_scale_milli_ = 0;
    uint32_t             cursor_shape_version_ = 0;  // bumps when the scaled shape changes
    uint32_t             cursor_sent_version_ = 0;
    bool                 cursor_sent_visible_ = false;
    int32_t              cursor_sent_x_ = 0;
    int32_t              cursor_sent_y_ = 0;

    // Encode dimensions (even-aligned from capture). Written at start and on
    // the encode thread at a geometry change, read by the stats and cursor
    // callers.
    std::atomic<uint32_t> encode_w_{0};
    std::atomic<uint32_t> encode_h_{0};

    // Geometry change (set_encode_height). The request is a height; 0 = none.
    std::atomic<uint32_t> encode_height_request_{0};
    // The last encoder bitrate asked for, for a full encoder re-initialize.
    std::atomic<uint32_t> last_bitrate_kbps_{0};
    std::atomic<uint32_t> geometry_changes_{0};
#ifdef _WIN32
    // Capture thread: the size and generation of the current preprocessor.
    uint32_t preprocessor_w_ = 0;
    uint32_t preprocessor_h_ = 0;
    uint32_t preprocessor_generation_ = 0;
    // Encode thread: the generation the encoder runs at.
    uint32_t encoder_generation_ = 0;
    // Preprocessors of earlier sizes. The encode thread frees one when it
    // runs at a later generation: until then a queued job (and the idle
    // keepalive picture) can still be one of its ring textures.
    struct RetiredPreprocessor {
        std::unique_ptr<VideoPreprocessor> preprocessor;
        uint32_t generation = 0;
    };
    std::mutex retired_mutex_;
    std::vector<RetiredPreprocessor> retired_preprocessors_;
    void apply_geometry_request(ID3D11Texture2D* texture);
    bool resize_encoder(uint32_t width, uint32_t height, uint32_t generation);
#endif

    // Async encode thread: capture enqueues, encode thread dequeues
    struct EncodeJob {
#ifdef _WIN32
        ID3D11Texture2D* texture = nullptr;
#elif defined(__APPLE__)
        void* texture = nullptr;
#endif
        uint64_t timestamp_us = 0;
        // The encode size of the texture and the preprocessor generation
        // that made it (geometry changes).
        uint32_t width = 0;
        uint32_t height = 0;
        uint32_t generation = 0;
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
    /// Set by the encode thread as it leaves. `stop_host` waits on this with a
    /// deadline instead of joining without one: a thread stuck inside a
    /// graphics driver must never hold the whole teardown (2026-09-16, where a
    /// stream-host sat in this join for 25 minutes).
    std::future<void> encode_exited_;
    /// True when that deadline passed and the thread was left running.
    std::atomic<bool> encode_thread_detached_{false};

    // Idle keepalive (Windows encode thread). DXGI and WGC deliver a frame only
    // when pixels change, so a paused game or an idle desktop sends no video at
    // all. A viewer who joined then saw black: its keyframe request only set a
    // flag that waited for the next encoded frame, which never came.
public:
    /// After this long without a new captured frame, the encode thread
    /// re-encodes the last picture.
    static constexpr uint64_t kIdleAfterUs = 500'000;
    /// Interval between keepalive re-encodes while idle (2 fps).
    static constexpr uint64_t kIdleRepeatIntervalUs = 500'000;
    /// Pure decision for the encode thread, testable without a GPU. Returns
    /// true when the last picture should be re-encoded now.
    static bool idle_repeat_due(bool have_last_frame, bool kicked,
                                uint64_t now_us, uint64_t last_new_frame_us,
                                uint64_t last_encode_us);
private:
    // Set by request_keyframe(): wake the encode thread so an idle stream
    // answers the keyframe request with the last picture at once.
    std::atomic<bool> keepalive_kick_{false};
    std::atomic<uint64_t> idle_repeat_frames_{0};
    PresentDelayHistogram present_delay_hist_;
#ifdef _WIN32
    // Called on the encode thread after each encoded frame.
    void maybe_reduce_encoder_cost();
    uint64_t cost_window_start_captured_ = 0;
    uint64_t cost_window_start_drops_    = 0;
#endif

    // Stats
    uint64_t host_start_time_  = 0;
    uint64_t frames_encoded_   = 0;
    // Capture arrivals, counted before any encode work so a stalled capture is
    // distinguishable from a stalled encoder. Written on the capture thread and
    // read from the stats caller — atomic. Rate/stall windows are derived by the
    // caller; keeping the time math out of here avoids a second clock policy.
    std::atomic<uint64_t> frames_captured_{0};
    std::atomic<uint64_t> last_capture_us_{0};
    // Source size of the latest converted frame and the count of changes after
    // the first one. Written on the capture thread, read by the stats caller.
    std::atomic<uint32_t> source_w_{0};
    std::atomic<uint32_t> source_h_{0};
    std::atomic<uint32_t> source_size_changes_{0};
    bool                  source_size_known_ = false;  // capture thread only

    // Decimation target. 0 means "no decimation" (encode every captured frame).
    // Read on the capture thread, written from the control thread.
    std::atomic<uint32_t> output_fps_{0};

    // Rolling encode-time mean. Written on the encode thread, read by the stats
    // caller; a plain double is fine because a torn read of a diagnostic mean is
    // harmless and locking the encode path for telemetry is not.
    std::atomic<double> encode_ms_mean_{0.0};
    // When the next frame is due, in the capture thread's own clock. Advanced by
    // one output interval per emitted frame. Only touched on the capture thread.
    uint64_t next_emit_deadline_us_ = 0;
    double   last_convert_ms_  = 0;
    double   last_encode_ms_   = 0;
};

} // namespace mello::video
