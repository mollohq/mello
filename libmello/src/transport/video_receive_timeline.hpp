#pragma once

// The viewer receive timeline (streaming-quality stage 3, spec 12 section 7).
//
// One per viewer. Complete H.264 access units go in, in any order. The
// timeline holds them in WebRTC's FrameBuffer (M131, the vendored overlay
// third_party/webrtc-video-timing) and releases each one for decode when
// VCMTiming says so. The target delay is the jitter estimate plus the 95th
// percentile decode time plus the render delay, between 1 ms and 150 ms. It
// rises when the link jitters and falls when the link calms.
//
// This class replaces upstream's VideoStreamBufferController, which runs on
// task queues. It is synchronous: the decode thread asks `poll()` what to do
// now, and waits in `wait()` for the next event.
//
// This header has no WebRTC include, so the rest of libmello stays C++17.
// The implementation compiles in the C++20 object library
// mello_video_timeline (libmello/CMakeLists.txt).
//
// Threads: `insert`, `on_loss` and `render_time_us` run on the receiver
// worker. `poll`, `on_decoded`, `on_decode_error` and `wait` run on the
// decode thread. The setters and `stats` run on any thread. One mutex guards
// the state; no callback runs under it.

#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <optional>
#include <vector>

namespace mello::transport {

/// One complete access unit from the H.264 receiver.
struct TimelineInput {
    /// The frame id: the unwrapped RTP sequence number of the last packet.
    int64_t id = 0;
    /// The id of the access unit this one decodes from (the one just before
    /// it). Not used for a key frame.
    int64_t reference = -1;
    uint32_t rtp_timestamp = 0;
    /// Steady-clock time of the last packet, in microseconds.
    int64_t arrival_us = 0;
    bool keyframe = false;
    /// A NACK repair completed it. Its arrival says nothing about the link
    /// jitter, so it does not train the estimators.
    bool retransmitted = false;
    std::vector<uint8_t> annex_b;
};

/// One access unit the timeline releases for decode.
struct TimelineFrame {
    std::vector<uint8_t> annex_b;
    int64_t id = 0;
    uint32_t rtp_timestamp = 0;
    bool keyframe = false;
    /// When the picture must be on the screen, steady clock, microseconds.
    int64_t render_time_us = 0;
    int64_t arrival_us = 0;
    int64_t released_us = 0;
};

struct TimelineDecision {
    enum class Kind {
        /// Nothing to do until a frame arrives.
        Idle,
        /// Ask again at `wake_us`, or earlier when a frame arrives.
        Wait,
        /// Decode `frame` now and show it at `frame.render_time_us`.
        Release,
        /// A reference is gone for real: ask the host for a key frame.
        KeyframeNeeded,
    };
    Kind kind = Kind::Idle;
    int64_t wake_us = 0;
    TimelineFrame frame;
};

struct TimelineStats {
    /// Jitter delay plus decode time plus render delay, clamped.
    double target_delay_ms = 0.0;
    /// The delay frames get now. It moves toward the target at 100 ms/s.
    double current_delay_ms = 0.0;
    /// The jitter estimate alone.
    double jitter_ms = 0.0;
    /// The 95th percentile decode time.
    double decode_ms_p95 = 0.0;
    uint64_t frames_inserted = 0;
    uint64_t frames_rejected = 0;
    uint64_t frames_released = 0;
    /// Released after the time the decode had to start.
    uint64_t frames_late = 0;
    /// Never released: dropped while the timeline waited for a key frame, or
    /// skipped because a newer frame was due.
    uint64_t frames_dropped = 0;
    uint64_t keyframe_requests = 0;
    uint64_t losses = 0;
    uint64_t timing_resets = 0;
    uint64_t frames_buffered = 0;
    bool keyframe_required = true;
};

class VideoReceiveTimeline {
public:
    /// Microseconds on one monotonic clock. The default is the steady clock
    /// of `mello_clock_now_us`.
    using ClockFn = std::function<int64_t()>;

    struct Config {
        /// Upper bound of the delay a frame gets (VCMTiming max playout
        /// delay).
        int64_t max_delay_us = 150'000;
        /// Lower bound. At least 1 ms: a minimum of 0 with a maximum under
        /// 500 ms switches VCMTiming to low-latency rendering, which has no
        /// jitter buffer at all.
        int64_t min_delay_us = 1'000;
        /// Two jitter estimators that each restart every `stagger_window_us`,
        /// half a window apart. The delay follows the larger one. Upstream
        /// alone forgets with a fixed weight (about 13 s at 60 fps) and does
        /// not rise in a 3 s jitter spike. Off: upstream behaviour.
        bool staggered_pair = true;
        int64_t stagger_window_us = 4'000'000;
        /// While the timeline waits for a key frame, it asks again after
        /// this time.
        int64_t keyframe_retry_us = 1'000'000;
        /// A delta frame released this much after its render time means the
        /// decoder cannot keep up. The timeline drops to the next key frame
        /// instead of decoding a backlog.
        int64_t max_lateness_us = 1'000'000;
        /// FrameBuffer bounds (upstream VideoReceiveStream2 with NACK on).
        int max_frames = 800;
        int max_decode_history = 1 << 13;
    };

    VideoReceiveTimeline();
    explicit VideoReceiveTimeline(Config config, ClockFn clock = {});
    ~VideoReceiveTimeline();

    VideoReceiveTimeline(const VideoReceiveTimeline&) = delete;
    VideoReceiveTimeline& operator=(const VideoReceiveTimeline&) = delete;

    // ---- Receiver worker ----------------------------------------------------

    /// Store one complete access unit. False when FrameBuffer refuses it (a
    /// duplicate, a frame older than the last decoded one, or a full buffer).
    bool insert(TimelineInput&& input);

    /// A reference is gone for real (a packet missed its repair deadline). The
    /// frames after it cannot decode: the timeline asks for a key frame and
    /// drops delta frames until one arrives.
    void on_loss();

    /// The render time of a frame with this RTP timestamp, for the repair
    /// deadline of a missing packet. Empty before the first frame.
    std::optional<int64_t> render_time_us(uint32_t rtp_timestamp) const;

    // ---- Decode thread ------------------------------------------------------

    TimelineDecision poll();

    /// The decoder took `decode_us` for the frame just released.
    void on_decoded(int64_t decode_us);

    /// The decoder refused a frame: the picture is broken until a key frame.
    void on_decode_error();

    /// Block until a frame arrives, `wake_us` passes, or `stop()`. Returns
    /// false after `stop()`. `wake_us` 0 waits for a frame only.
    bool wait(int64_t wake_us);

    /// Wake every waiter; `wait` returns false from now on.
    void stop();

    /// Wake a waiter once (its owner stops or changes source).
    void wake();

    // ---- Any thread ---------------------------------------------------------

    void set_rtt_us(int64_t rtt_us);
    int64_t rtt_us() const;

    /// The smallest delay a frame gets (stage 7 A/V sync: audio's playout
    /// delay). It is applied again after a timing reset. Clamped to
    /// [Config::min_delay_us, Config::max_delay_us].
    void set_min_delay_us(int64_t min_delay_us);

    /// Count frames the present path skipped because a newer one was due.
    void add_dropped(uint64_t frames);

    /// Runs (outside the timeline's lock, on the polling thread) each time
    /// `poll` returns KeyframeNeeded. The receiver session sets it: it sends
    /// the PLI.
    void set_keyframe_request_handler(std::function<void()> handler);

    TimelineStats stats() const;
    bool keyframe_required() const;

    int64_t now_us() const;

private:
    class Impl;
    std::unique_ptr<Impl> impl_;
};

} // namespace mello::transport
