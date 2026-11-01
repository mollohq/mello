#pragma once
#include "encoder.hpp"

#if defined(__linux__)
#include <atomic>
#include <chrono>
#include <deque>
#include <memory>
#include <string>
#include <vector>

namespace mello::video {

/// VA-API H.264 encode on AMD and Intel through GStreamer
/// (encoder_gst_va_linux.cpp): `appsrc ! vapostproc ! vah264enc ! appsink`
/// on the primary GPU. vapostproc converts the capture's BGRx to NV12 and
/// scales it to the encode size on the GPU; the source size may change
/// between pictures (a window that is resized).
///
/// The settings follow NVENC and AMF on Windows: High profile, VBR with
/// peaks up to 1.25x the target, no B-frames, one reference frame, an IDR
/// every `keyframe_interval` frames and on request, the fastest preset.
///
/// One picture is in flight: `encode` pushes picture N and returns the
/// access unit of N-1, which the GPU has finished meanwhile, so capture and
/// encode overlap. `drain` gives the last one while the screen is still.
/// The input caps carry the configured frame rate: the encoder's rate
/// control budgets per frame from it.
///
/// Threads: `initialize`, `encode`, `drain` and `shutdown` on the capture
/// thread (`initialize` on the caller of start_host). The setters store
/// atomics that `encode` applies before the next picture.
class GstVaEncoder : public Encoder {
public:
    GstVaEncoder();
    ~GstVaEncoder() override;

    /// True when GStreamer loads and has the VA H.264 encoder and
    /// post-processor of the primary GPU.
    static bool is_available();

    /// The DMA-BUF layouts vapostproc imports, for the capture to offer, so
    /// the picture never leaves the GPU. Empty without GStreamer 1.24 (the
    /// DMA_DRM format) or libgstallocators: the capture then sends CPU
    /// pictures. Read once per process.
    static std::vector<DmaBufFormat> dmabuf_formats();

    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(const CaptureFrame& frame, uint64_t capture_us, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override { return codec == VideoCodec::H264; }
    const char* name() const override { return "VA-API"; }
    void        get_phase_timing(EncodePhaseTiming& out) const override;
    void        set_framerate(uint32_t fps) override;
    /// The access units that are ready, without a new picture.
    bool        drain(EncodedPacket& out) override;

private:
    struct Pipeline;
    std::unique_ptr<Pipeline> p_;
    EncoderConfig config_{};

    /// Pulls every ready access unit into the backlog, waiting up to
    /// `timeout` for the first. Then hands out the oldest.
    bool collect(uint64_t timeout_ns, uint64_t capture_us, EncodedPacket& out);

    std::atomic<bool>     force_idr_{false};
    std::atomic<uint32_t> pending_bitrate_kbps_{0};
    std::atomic<uint32_t> pending_fps_{0};
    uint32_t fps_ = 60;  // the frame rate the input caps announce
    // Pictures pushed whose access unit has not come out yet.
    uint64_t in_flight_ = 0;

    // Access units the encoder gave beyond one per picture; they go out
    // first, in order (none may be dropped).
    std::deque<EncodedPacket> backlog_;
    std::string input_caps_;  // what appsrc announces now
    void* dmabuf_allocator_ = nullptr;  // GstAllocator*, DMA-BUF import
    uint64_t first_pts_us_ = 0;
    bool failed_ = false;

    EncoderStats stats_{};
    std::chrono::steady_clock::time_point stats_window_start_{};
    uint32_t stats_window_frames_ = 0;
    uint64_t stats_window_bytes_ = 0;
    EncodePhaseTiming phases_{};
};

/// The 32-bit RGB DMA-BUF layouts in a caps string such as vapostproc's sink
/// caps (`drm-format=(string){ XR24:0x0200000000000901, AR24, ... }`).
/// A fourcc without a modifier is linear (0). Exposed for the tests.
std::vector<DmaBufFormat> parse_rgb_drm_formats(const std::string& caps);

} // namespace mello::video
#endif
