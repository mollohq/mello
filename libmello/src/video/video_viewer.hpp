#pragma once

// The stream viewer pipeline (streaming-quality stage 3, spec 14 section 5,
// spec 12 section 7).
//
//   receive timeline --poll--> decode thread --ring--> present thread --> callback
//
// - The decode thread asks the viewer's receive timeline what to do
//   (VideoReceiveTimeline::poll). A released frame goes to the decoder at
//   once and lands in a ring slot with its render time. Without a timeline
//   (feed_packet, the tests and tools) a fed access unit decodes at once and
//   is due at once.
// - The ring has Decoder::kOutputSlots slots; slot i is the decoder's output
//   slot i, a texture of its own. A slot the present thread reads is never
//   the slot of the next decode. When every slot is full, the oldest frame
//   that is not on screen gives its slot up (counted as dropped).
// - The present thread is the one present clock. It sleeps until the render
//   time of the next frame, shows the newest frame that is due, and drops
//   the older ones. On Windows it converts the picture into the shared RGBA
//   texture (keyed mutex) and calls the native frame callback, which presents
//   it on the swap chain; or it reads the RGBA back for the CPU frame
//   callback. On macOS it gives the CPU frame callback the RGBA pixels.
//
// Threads: start, stop, attach_timeline, set_*_callback, feed_packet and the
// getters run on the owner's thread (any one thread at a time). The callbacks
// run on the present thread.

#include "graphics_device.hpp"
#include "decoder.hpp"
#include "../transport/video_receive_timeline.hpp"

#include <array>
#include <atomic>
#include <condition_variable>
#include <cstdint>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#ifdef _WIN32
#include "staging_texture.hpp"
#endif

namespace mello::video {

struct ViewerConfig {
    uint32_t width = 0;
    uint32_t height = 0;
    uint32_t fps = 60;
};

struct ViewerStats {
    std::string decoder_name;
    bool attached = false;
    uint64_t frames_decoded = 0;
    uint64_t decode_errors = 0;
    uint64_t frames_presented = 0;
    /// Decoded and never shown: a newer frame was due, or the ring needed
    /// the slot.
    uint64_t frames_dropped_present = 0;
    double decode_ms_last = 0.0;
    double decode_ms_mean = 0.0;
    /// Present time minus render time of the last frame shown, ms. Positive:
    /// the frame was late.
    double present_lateness_ms = 0.0;
    mello::transport::TimelineStats timeline;
};

class ViewerPipeline {
public:
    using FrameCallback = std::function<void(const uint8_t* rgba, uint32_t w, uint32_t h, uint64_t ts)>;
    using NativeFrameCallback = std::function<void(void* shared_handle, uint32_t w, uint32_t h,
                                                   uint32_t format, uint32_t uv_y_offset, uint64_t ts)>;
    /// Returns the current timeline of the viewer's peer, or null. Asked
    /// again when the timeline in use stops (a new session).
    using TimelineProvider = std::function<std::shared_ptr<mello::transport::VideoReceiveTimeline>()>;

    ViewerPipeline();
    ~ViewerPipeline();

    ViewerPipeline(const ViewerPipeline&) = delete;
    ViewerPipeline& operator=(const ViewerPipeline&) = delete;

    /// `decoder` replaces the factory's choice (tests: a fake decoder).
    bool start(const GraphicsDevice& device, const ViewerConfig& config, FrameCallback on_frame,
               std::unique_ptr<Decoder> decoder = nullptr);
    void stop();
    bool running() const { return running_.load(std::memory_order_acquire); }

    void set_native_frame_callback(NativeFrameCallback cb);
    void attach_timeline(TimelineProvider provider);

    /// Without a timeline: decode this access unit now and show it at once.
    bool feed_packet(const uint8_t* data, size_t size, bool is_keyframe);
    size_t decode_queue_depth() const;
    size_t ring_depth() const;

    /// True when the present thread showed at least one frame since the
    /// last call.
    bool take_presented();

    void stats(ViewerStats& out) const;

    /// Steady clock of the pipeline and the timeline, microseconds.
    static int64_t now_us();

private:
    struct RingSlot {
        enum class State { Free, Decoding, Ready, Presenting };
        State state = State::Free;
        int64_t render_us = 0;
        uint64_t seq = 0;
#ifdef _WIN32
        DecodedPicture picture;
#else
        void* pixel_buffer = nullptr; // CVPixelBufferRef, retained
#endif
    };

    struct DecodeJob {
        std::vector<uint8_t> bytes;
        bool keyframe = false;
    };

    void decode_thread_main();
    void present_thread_main();
    std::shared_ptr<mello::transport::VideoReceiveTimeline> current_timeline();
    bool decode_into_ring(const std::vector<uint8_t>& bytes, bool keyframe, int64_t render_us);
    int acquire_slot_for_decode();
    void release_slot_locked(RingSlot& slot);
    void present_slot(RingSlot& slot);

    GraphicsDevice device_{};
    ViewerConfig config_{};
    std::unique_ptr<Decoder> decoder_;
#ifdef _WIN32
    StagingTexture converter_;
    bool converter_ready_ = false;
#endif
    std::vector<uint8_t> rgba_buf_;  // present thread

    std::mutex callback_mutex_;
    FrameCallback frame_cb_;
    NativeFrameCallback native_cb_;

    std::atomic<bool> running_{false};
    std::thread decode_thread_;
    std::thread present_thread_;

    std::mutex provider_mutex_;
    TimelineProvider provider_;
    std::shared_ptr<mello::transport::VideoReceiveTimeline> timeline_;  // decode thread
    int64_t provider_checked_us_ = 0;                                    // decode thread

    mutable std::mutex jobs_mutex_;
    std::condition_variable jobs_cv_;
    std::deque<DecodeJob> jobs_;

    mutable std::mutex ring_mutex_;
    std::condition_variable ring_cv_;
    std::array<RingSlot, Decoder::kOutputSlots> ring_;
    uint64_t next_seq_ = 1;

    std::atomic<bool> presented_since_take_{false};
    std::atomic<uint64_t> frames_decoded_{0};
    std::atomic<uint64_t> decode_errors_{0};
    std::atomic<uint64_t> frames_presented_{0};
    std::atomic<uint64_t> frames_dropped_present_{0};
    std::atomic<double> decode_ms_last_{0.0};
    std::atomic<double> decode_ms_mean_{0.0};
    std::atomic<double> present_lateness_ms_{0.0};
    std::atomic<bool> attached_{false};
    mutable std::mutex timeline_stats_mutex_;
    std::shared_ptr<mello::transport::VideoReceiveTimeline> stats_timeline_;
};

} // namespace mello::video
