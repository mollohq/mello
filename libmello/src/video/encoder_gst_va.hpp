#pragma once
#include "encoder.hpp"

#if defined(__linux__)
#include <atomic>
#include <chrono>
#include <deque>
#include <memory>

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
/// Threads: `initialize`, `encode` and `shutdown` on the capture thread
/// (`initialize` on the caller of start_host). The setters store atomics
/// that `encode` applies before the next picture.
class GstVaEncoder : public Encoder {
public:
    GstVaEncoder();
    ~GstVaEncoder() override;

    /// True when GStreamer loads and has the VA H.264 encoder and
    /// post-processor of the primary GPU.
    static bool is_available();

    bool        initialize(const GraphicsDevice& device, const EncoderConfig& config) override;
    void        shutdown() override;
    bool        encode(const CpuFrame& frame, uint64_t capture_us, EncodedPacket& out) override;
    void        request_keyframe() override;
    void        set_bitrate(uint32_t kbps) override;
    void        get_stats(EncoderStats& out) const override;
    bool        supports_codec(VideoCodec codec) const override { return codec == VideoCodec::H264; }
    const char* name() const override { return "VA-API"; }
    void        get_phase_timing(EncodePhaseTiming& out) const override;

private:
    struct Pipeline;
    std::unique_ptr<Pipeline> p_;
    EncoderConfig config_{};

    std::atomic<bool>     force_idr_{false};
    std::atomic<uint32_t> pending_bitrate_kbps_{0};

    // Access units the encoder gave beyond one per picture; they go out
    // first, in order (none may be dropped).
    std::deque<EncodedPacket> backlog_;
    uint64_t first_pts_us_ = 0;
    bool failed_ = false;

    EncoderStats stats_{};
    std::chrono::steady_clock::time_point stats_window_start_{};
    uint32_t stats_window_frames_ = 0;
    uint64_t stats_window_bytes_ = 0;
    EncodePhaseTiming phases_{};
};

} // namespace mello::video
#endif
